use crate::{
    gemini_pool::{GeminiPool, effective_keys, now_unix, selected_live_models},
    journal::Journal,
    plugins::{Host, Runtime},
    session::{Dialog, Session, queue_mic, take_mic},
    settings::{Profile, Settings, Store},
    tools::Tools,
    voice_error::{Stage, VoiceFailure, classify},
    vpn::{Proxy, Route},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    time::Duration,
};

pub struct VoiceOptions {
    pub gemini_pool: Option<Arc<GeminiPool>>,
    pub store: Option<Store>,
    pub proxy: Option<Arc<Proxy>>,
    pub allow_failover: bool,
}

impl Default for VoiceOptions {
    fn default() -> Self {
        Self {
            gemini_pool: None,
            store: None,
            proxy: None,
            allow_failover: true,
        }
    }
}

pub struct Voice {
    id: String,
    dialog: Dialog,
    rt: tokio::runtime::Handle,
    pending: Mutex<Vec<i16>>,
    output: Mutex<Option<SyncSender<Vec<i16>>>>,
    audio: Mutex<Option<Receiver<Vec<i16>>>>,
    ready: Mutex<Option<Receiver<Result<(), String>>>>,
    commands: tokio::sync::mpsc::Sender<String>,
    cancelled: AtomicBool,
    close: tokio::sync::Notify,
    interrupted: AtomicBool,
    last_failure: Mutex<Option<VoiceFailure>>,
}

impl Voice {
    pub fn open(
        rt: &tokio::runtime::Handle,
        host: Arc<Host>,
        tools: Option<Arc<Tools>>,
        journal: Arc<Journal>,
        settings: Settings,
        profile: Profile,
        instructions: String,
    ) -> Arc<Self> {
        Self::open_with(
            rt,
            host,
            tools,
            journal,
            settings,
            profile,
            instructions,
            VoiceOptions::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_with(
        rt: &tokio::runtime::Handle,
        host: Arc<Host>,
        tools: Option<Arc<Tools>>,
        journal: Arc<Journal>,
        settings: Settings,
        profile: Profile,
        instructions: String,
        opts: VoiceOptions,
    ) -> Arc<Self> {
        let id = match profile.engine.as_str() {
            "" | "live" => "openai",
            "eleven_realtime" => "elevenlabs",
            id => id,
        }
        .to_string();
        let (tx, rx) = sync_channel(128);
        let (ready, ready_rx) = sync_channel(1);
        let (commands, command_rx) = tokio::sync::mpsc::channel(16);
        let me = Arc::new(Self {
            id: id.clone(),
            dialog: Dialog::new(journal, &id, &profile.live_model),
            rt: rt.clone(),
            pending: Mutex::default(),
            output: Mutex::new(Some(tx)),
            audio: Mutex::new(Some(rx)),
            ready: Mutex::new(Some(ready_rx)),
            commands,
            cancelled: AtomicBool::new(false),
            close: tokio::sync::Notify::new(),
            interrupted: AtomicBool::new(false),
            last_failure: Mutex::new(None),
        });
        let run = me.clone();
        rt.spawn(async move {
            let result = tokio::select! {
                result = run.run(host, tools, settings, profile, instructions, ready.clone(), command_rx, opts) => result,
                _ = run.close.notified() => Ok(()),
            };
            if let Err(error) = result {
                // Если ready ещё не принят — ошибку запишет Engine::converse (без дубля в журнале).
                if ready.try_send(Err(error.clone())).is_err() {
                    run.dialog.finish_turn(&error);
                }
            }
            run.output.lock().unwrap().take();
            run.cancelled.store(true, Ordering::SeqCst);
        });
        me
    }

    fn set_failure(&self, failure: VoiceFailure) {
        *self.last_failure.lock().unwrap() = Some(failure);
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        host: Arc<Host>,
        tools: Option<Arc<Tools>>,
        settings: Settings,
        p: Profile,
        instructions: String,
        ready: SyncSender<Result<(), String>>,
        mut commands: tokio::sync::mpsc::Receiver<String>,
        opts: VoiceOptions,
    ) -> Result<(), String> {
        let provider = host.provider(&self.id)?;
        let vpn_blocked = || {
            opts.proxy
                .as_ref()
                .is_some_and(|proxy| matches!(proxy.route(&self.id), Route::Blocked(_)))
        };
        let tz = jiff::tz::TimeZone::get(&settings.timezone).unwrap_or(jiff::tz::TimeZone::UTC);

        if self.id == "gemini" {
            return self
                .run_gemini(
                    provider,
                    tools,
                    settings,
                    p,
                    instructions,
                    ready,
                    &mut commands,
                    opts,
                    &tz,
                )
                .await;
        }

        let key = match self.id.as_str() {
            "openai" => settings.openai_key.clone(),
            "grok" => settings.grok_key.clone(),
            "elevenlabs" => settings.elevenlabs_key.clone(),
            _ => return Err("Этот движок требует классический режим".into()),
        };
        let prepare = async {
            match &tools {
                Some(tools) => tools.prepare().await,
                None => (String::new(), vec![]),
            }
        };
        let ((context, defs), socket) = if self.id == "elevenlabs" {
            let (context, defs) = prepare.await;
            let session = eleven_session(&p, &settings.language, &format!("{instructions}\n{context}"), &defs);
            let socket = match Link::open(provider, &key, session).await {
                Ok(s) => s,
                Err(err) => {
                    self.set_failure(classify(&self.id, Stage::Connect, &err, None, vpn_blocked()));
                    return Err(err);
                }
            };
            ((context, defs), socket)
        } else {
            match tokio::try_join!(
                async { Ok::<_, String>(prepare.await) },
                Link::open(provider, &key, Value::Null)
            ) {
                Ok(pair) => pair,
                Err(err) => {
                    self.set_failure(classify(&self.id, Stage::Connect, &err, None, vpn_blocked()));
                    return Err(err);
                }
            }
        };

        let model = if p.live_model.is_empty() {
            match self.id.as_str() {
                "grok" => "grok-voice-latest",
                _ => "gpt-live-1",
            }
        } else {
            &p.live_model
        };
        let voice = if p.voice.is_empty() {
            match self.id.as_str() {
                "grok" => "Ara",
                _ => "cedar",
            }
        } else {
            &p.voice
        };
        let start = match self.id.as_str() {
            "grok" => json!({"type":"session.update","session":{"instructions":format!("{instructions}\n{context}"),"voice":voice,"turn_detection":{"type":"server_vad"},"audio":{"input":{"format":{"type":"audio/pcm","rate":24000}},"output":{"format":{"type":"audio/pcm","rate":24000}}},"tools":defs}}),
            _ => {
                let mut backend = defs.clone();
                if p.web_search {
                    backend.push(json!({"type":"web_search"}));
                }
                json!({"type":"session.start","session":{"model":model,"instructions":instructions,"audio":{"format":{"type":"audio/pcm","rate":16000},"output":{"voice":voice}},"delegation":{"type":"responses","responses":{"model":if p.backend_model.is_empty(){crate::settings::BACKEND_MODELS[0]}else{&p.backend_model},"instructions":format!("{}\n{context}",crate::rules::BACKEND_RULES),"tools":backend,"tool_choice":"auto","reasoning":{"effort":crate::chatgpt::effort(&p.backend_model,&p.effort)}}}}})
            }
        };
        self.drive_socket(
            socket,
            &key,
            model,
            Some(start).filter(|_| self.id != "elevenlabs"),
            tools,
            defs,
            ready,
            &mut commands,
            None,
            None,
            &tz,
            &vpn_blocked,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_gemini(
        &self,
        provider: Runtime,
        tools: Option<Arc<Tools>>,
        settings: Settings,
        p: Profile,
        instructions: String,
        ready: SyncSender<Result<(), String>>,
        commands: &mut tokio::sync::mpsc::Receiver<String>,
        opts: VoiceOptions,
        tz: &jiff::tz::TimeZone,
    ) -> Result<(), String> {
        let vpn_blocked = || {
            opts.proxy
                .as_ref()
                .is_some_and(|proxy| matches!(proxy.route("gemini"), Route::Blocked(_)))
        };
        let all_keys = effective_keys(&settings.gemini_keys, &settings.gemini_key);
        if all_keys.is_empty() {
            let failure = classify("gemini", Stage::Config, "Добавьте ключ Gemini в «Подключениях»", None, false);
            self.set_failure(failure.clone());
            return Err(failure.detail);
        }
        let models = selected_live_models(&p);
        let (context, defs) = match &tools {
            Some(tools) => tools.prepare().await,
            None => (String::new(), vec![]),
        };
        let voice = if p.voice.is_empty() { "Kore" } else { &p.voice };

        let mut candidates = if let Some(pool) = &opts.gemini_pool {
            pool.candidates(&all_keys, &settings.gemini_key, &models, now_unix())
        } else {
            vec![crate::gemini_pool::Candidate {
                key_index: 0,
                key: all_keys[0].clone(),
                tail: crate::gemini_pool::key_tail(&all_keys[0]),
                model: models[0].clone(),
            }]
        };
        if !opts.allow_failover && candidates.len() > 1 {
            candidates.truncate(1);
        }

        if candidates.is_empty() {
            let failure = opts
                .gemini_pool
                .as_ref()
                .map(|pool| pool.all_exhausted_failure(&all_keys, &models, now_unix(), tz))
                .unwrap_or_else(|| classify("gemini", Stage::Connect, "Все ключи Gemini в лимите", None, false));
            log::warn!("Gemini: {} | Что делать: {}", failure.detail, failure.action);
            self.set_failure(failure.clone());
            return Err(failure.detail);
        }

        let total = candidates.len();
        for (attempt_idx, cand) in candidates.iter().enumerate() {
            if self.cancelled.load(Ordering::SeqCst) {
                return Ok(());
            }
            let next_cand = candidates.get(attempt_idx + 1);
            let socket = match Link::open(provider.clone(), &cand.key, Value::Null).await {
                Ok(s) => s,
                Err(err) => {
                    let failure = classify("gemini", Stage::Connect, &err, None, vpn_blocked());
                    if let Some(pool) = &opts.gemini_pool
                        && let Some(ev) = pool.record_failure(
                            &cand.key,
                            cand.key_index,
                            &cand.model,
                            &failure,
                            now_unix(),
                            tz,
                        )
                    {
                        log_gemini_switch(&ev.summary, &cand.model, next_cand, opts.allow_failover);
                        if opts.allow_failover && next_cand.is_some() {
                            continue;
                        }
                        let final_failure = if attempt_idx + 1 >= total {
                            pool.all_exhausted_failure(&all_keys, &models, now_unix(), tz)
                        } else {
                            failure
                        };
                        log::warn!("Gemini: {} | Что делать: {}", final_failure.detail, final_failure.action);
                        self.set_failure(final_failure.clone());
                        return Err(final_failure.detail);
                    }
                    self.set_failure(failure.clone());
                    return Err(err);
                }
            };

            let start = crate::gemini::live_setup(
                &cand.model,
                voice,
                &format!("{instructions}\n{context}"),
                &settings.language,
                &defs,
                tools.is_some() && p.web_search,
            );
            let mut socket = socket;
            if let Err(err) = send(&mut socket, start).await {
                let failure = classify("gemini", Stage::Connect, &err, None, vpn_blocked());
                self.set_failure(failure);
                return Err(err);
            }

            // Ждём setupComplete или мгновенный отказ по квоте/ключу (WS Close 1011 / error) до старта разговора.
            match wait_gemini_setup(&mut socket, &cand.key).await {
                Ok(()) => {
                    if let Some(pool) = &opts.gemini_pool {
                        pool.record_success(&cand.key, &cand.model);
                    }
                    self.dialog.set_model(&cand.model);
                    if attempt_idx > 0 {
                        log::info!(
                            "Gemini: переключение сработало -> ключ #{} ({}), модель {}",
                            cand.key_index + 1,
                            cand.tail,
                            cand.model
                        );
                    }
                    if cand.key != settings.gemini_key
                        && let Some(store) = &opts.store
                    {
                        let new_key = cand.key.clone();
                        let _ = store.update(|s| -> Result<(), std::io::Error> {
                            if s.gemini_keys.contains(&new_key) {
                                s.gemini_key = new_key;
                            }
                            Ok(())
                        });
                    }
                    let _ = ready.try_send(Ok(()));
                    return self
                        .drive_socket(
                            socket,
                            &cand.key,
                            &cand.model,
                            None,
                            tools,
                            defs,
                            ready,
                            commands,
                            opts.gemini_pool.as_ref().map(|p| (p.clone(), cand.key_index)),
                            Some(true),
                            tz,
                            &vpn_blocked,
                        )
                        .await;
                }
                Err((ws_code, err_msg)) => {
                    let failure = classify("gemini", Stage::Connect, &err_msg, ws_code, vpn_blocked());
                    if let Some(pool) = &opts.gemini_pool
                        && let Some(ev) = pool.record_failure(
                            &cand.key,
                            cand.key_index,
                            &cand.model,
                            &failure,
                            now_unix(),
                            tz,
                        )
                    {
                        log_gemini_switch(&ev.summary, &cand.model, next_cand, opts.allow_failover);
                        if opts.allow_failover && next_cand.is_some() {
                            continue;
                        }
                        let final_failure = if attempt_idx + 1 >= total {
                            pool.all_exhausted_failure(&all_keys, &models, now_unix(), tz)
                        } else {
                            failure
                        };
                        log::warn!("Gemini: {} | Что делать: {}", final_failure.detail, final_failure.action);
                        self.set_failure(final_failure.clone());
                        return Err(final_failure.detail);
                    }
                    self.set_failure(failure);
                    return Err(err_msg);
                }
            }
        }

        let failure = opts
            .gemini_pool
            .as_ref()
            .map(|pool| pool.all_exhausted_failure(&all_keys, &models, now_unix(), tz))
            .unwrap_or_else(|| classify("gemini", Stage::Connect, "Не удалось открыть сессию Gemini", None, false));
        self.set_failure(failure.clone());
        Err(failure.detail)
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_socket(
        &self,
        mut socket: Link,
        key: &str,
        model: &str,
        initial_send: Option<Value>,
        tools: Option<Arc<Tools>>,
        defs: Vec<Value>,
        ready: SyncSender<Result<(), String>>,
        commands: &mut tokio::sync::mpsc::Receiver<String>,
        gemini_ctx: Option<(Arc<GeminiPool>, usize)>,
        already_started: Option<bool>,
        tz: &jiff::tz::TimeZone,
        vpn_blocked: &impl Fn() -> bool,
    ) -> Result<(), String> {
        if let Some(start) = initial_send {
            if let Err(err) = send(&mut socket, start).await {
                self.set_failure(classify(&self.id, Stage::Connect, &err, None, vpn_blocked()));
                return Err(err);
            }
        }
        let gemini = self.id == "gemini";
        // Send the real-time stream every 40 ms. Drain up to 160 ms per tick
        // while catching up with speech captured during setup.
        const TICK_MS: u64 = 40;
        const SAMPLES_PER_TICK: usize = crate::audio::MIC_RATE as usize * TICK_MS as usize / 1000;
        const MAX_SAMPLES_PER_TICK: usize = SAMPLES_PER_TICK * 4;
        let mut started = already_started.unwrap_or(false);
        let mut tasks = tokio::task::JoinSet::<(String, String, String, Value)>::new();
        let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut handles = std::collections::HashMap::<String, tokio::task::AbortHandle>::new();
        let mut seen = std::collections::HashSet::new();
        let mut pending_calls = Vec::<Value>::new();
        let mut music_turn = crate::music::MusicTurnTracker::default();
        let mut music_turn_id = 0i64;
        let mut up = jarvis_dsp::audio::Upsampler::new(3);
        let mut phase = 0;
        let mut speaking = false;
        let mut diag = SessionDiag::new(
            &self.id,
            if matches!(self.id.as_str(), "grok" | "elevenlabs") {
                24_000
            } else {
                16_000
            },
        );
        loop {
            tokio::select! {
                biased;
                result = tasks.join_next(), if !tasks.is_empty() => {
                    self.dialog.working(-1);
                    let (name, args, outcome, value) = match result.ok_or("Вызов инструмента завершился")? {
                        Ok(item) => item,
                        Err(e) if e.is_cancelled() => continue,
                        Err(e) => return Err(e.to_string()),
                    };
                    let id = value
                        .pointer("/toolResponse/functionResponses/0/id")
                        .or_else(|| value.pointer("/item/call_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    handles.remove(id);
                    let turn_id = self.dialog.utterance().0;
                    if turn_id != music_turn_id {
                        music_turn = crate::music::MusicTurnTracker::default();
                        music_turn_id = turn_id;
                    }
                    music_turn.observe_tool(&name, &args, &outcome);
                    if tasks.is_empty()
                        && music_turn.should_close(&outcome)
                        && !crate::music::wants_keep_talking(&self.dialog.utterance().1)
                    {
                        self.dialog.on_bot_note(&outcome);
                        self.dialog.mute();
                        self.dialog.request_close();
                        return Ok(());
                    }
                    send(&mut socket, value).await?;
                    if self.id != "gemini" && (self.id != "elevenlabs" || tasks.is_empty()) {
                        send(&mut socket, json!({"type":"response.create"})).await?;
                    }
                },
                Some(text) = commands.recv(), if started => {
                    let value = match self.id.as_str() {
                        "gemini" => json!({"clientContent":{"turns":[{"role":"user","parts":[{"text":format!("Произнеси дословно: {text}")}]}],"turnComplete":true}}),
                        "grok" | "elevenlabs" => json!({"type":"conversation.item.create","item":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("Произнеси дословно: {text}")}]}}),
                        _ => json!({"type":"session.commentary.append","delegation_id":null,"content":text}),
                    };
                    send(&mut socket, value).await?;
                    if matches!(self.id.as_str(), "grok" | "elevenlabs") {
                        send(&mut socket, json!({"type":"response.create"})).await?;
                    }
                },
                _ = interval.tick(), if started && (!gemini || tools.is_some()) => {
                    let mut pcm = take_mic(&mut self.pending.lock().unwrap(), MAX_SAMPLES_PER_TICK);
                    if pcm.is_empty() {
                        pcm.resize(SAMPLES_PER_TICK, 0);
                    }
                    let has_signal = pcm.iter().any(|&s| s != 0);
                    if diag.input_rate == 24_000 {
                        let input = up.run(&pcm);
                        pcm = input.iter().skip(phase).step_by(2).copied().collect();
                        phase = (phase + input.len()) % 2;
                    }
                    diag.note_sent(pcm.len(), has_signal);
                    let data = B64.encode(pcm.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
                    let value = match self.id.as_str() {
                        "gemini" => json!({"realtimeInput":{"audio":{"mimeType":"audio/pcm;rate=16000","data":data}}}),
                        "grok" | "elevenlabs" => json!({"type":"input_audio_buffer.append","audio":data}),
                        _ => json!({"type":"session.input_audio.append","audio":data}),
                    };
                    if let Err(err) = send(&mut socket, value).await {
                        self.set_failure(classify(&self.id, Stage::Stream, &err, None, vpn_blocked()));
                        return Err(err);
                    }
                },
                message = socket.events.recv() => {
                    let Some(message) = message else { return Ok(()); };
                    let mut message = match message {
                        Ok(m) => m,
                        Err(err) => {
                            let st = if started { Stage::Stream } else { Stage::Connect };
                            self.set_failure(classify(&self.id, st, &err, None, vpn_blocked()));
                            return Err(err);
                        }
                    };
                    if let Some(close) = message["close"].as_str() {
                        let code = message["code"].as_u64().map(|c| c as u16);
                        let detail = close.replace(key, "***").chars().take(220).collect::<String>();
                        let abnormal = code.is_some_and(|c| c != 1000) || !detail.is_empty();
                        if !started || abnormal {
                            let msg = if detail.is_empty() {
                                format!("{} закрыл голосовое соединение (код {})", self.id, code.unwrap_or(1006))
                            } else {
                                format!("{}: {detail}", self.id)
                            };
                            let st = if started { Stage::Stream } else { Stage::Connect };
                            let failure = classify(&self.id, st, &msg, code, vpn_blocked());
                            if let Some((pool, key_idx)) = &gemini_ctx
                                && let Some(ev) = pool.record_failure(key, *key_idx, model, &failure, now_unix(), tz)
                            {
                                log::warn!("Gemini (во время разговора): {}", ev.summary);
                            }
                            self.set_failure(failure);
                            return Err(msg);
                        }
                        return Ok(());
                    }
                    let event = message["event"].take();
                    for id in event.pointer("/toolCallCancellation/ids").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                        if let Some(handle) = handles.remove(id) {
                            handle.abort();
                        }
                    }
                    let kind = event["type"].as_str().unwrap_or("");
                    diag.note_event(kind);
                    if event.get("setupComplete").is_some() || ["session.started", "session.updated", "session.created"].contains(&kind) {
                        if let Some(rate) = negotiated_input_rate(&event) {
                            diag.input_rate = rate;
                        }
                        if !started {
                            started = true;
                            let _ = ready.try_send(Ok(()));
                        }
                        continue;
                    }
                    if kind == "error" || event.get("error").is_some() {
                        let message = event
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("Провайдер отклонил голосовую сессию");
                        let msg: String = message.replace(key, "***").chars().take(240).collect();
                        let st = if started { Stage::Stream } else { Stage::Connect };
                        let failure = classify(&self.id, st, &msg, None, vpn_blocked());
                        if let Some((pool, key_idx)) = &gemini_ctx
                            && let Some(ev) = pool.record_failure(key, *key_idx, model, &failure, now_unix(), tz)
                        {
                            log::warn!("Gemini (ошибка сессии): {}", ev.summary);
                        }
                        self.set_failure(failure);
                        return Err(msg);
                    }
                    if ["session.closed"].contains(&kind) {
                        return Ok(());
                    }
                    if kind == "input_audio_buffer.speech_started" {
                        diag.speech_started += 1;
                        self.dialog.heard_input(std::time::Instant::now());
                    }
                    if should_interrupt_on_event(kind, speaking, &event) {
                        self.dialog.mute();
                        self.interrupted.store(true, Ordering::SeqCst);
                        speaking = false;
                    }
                    if kind == "response.created" {
                        self.dialog.unmute();
                    }
                    if matches!(
                        kind,
                        "session.input_transcript.delta"
                            | "conversation.item.input_audio_transcription.delta"
                            | "conversation.item.input_audio_transcription.completed"
                    ) {
                        diag.transcripts += 1;
                        self.dialog.on_user(event[if kind.ends_with("completed") { "transcript" } else { "delta" }].as_str().unwrap_or(""));
                        if kind.ends_with("completed") {
                            self.dialog.unmute();
                        }
                    }
                    if ["session.output_transcript.delta", "response.audio_transcript.delta", "response.output_audio_transcript.delta"].contains(&kind) {
                        self.dialog.on_bot(event["delta"].as_str().unwrap_or(""));
                    }
                    if ["session.output_audio.delta", "response.audio.delta", "response.output_audio.delta"].contains(&kind) {
                        diag.audio_chunks += 1;
                        self.audio(event["delta"].as_str().unwrap_or("")).await?;
                        speaking = true;
                    }
                    if let Some(content) = event.get("serverContent") {
                        if content.get("groundingMetadata").is_some() {
                            log::debug!("Gemini Live: получены результаты поиска");
                            let turn_id = self.dialog.utterance().0;
                            if turn_id != music_turn_id {
                                music_turn = crate::music::MusicTurnTracker::default();
                                music_turn_id = turn_id;
                            }
                            music_turn.note_other();
                        }
                        if let Some(text) = content.pointer("/inputTranscription/text").and_then(Value::as_str) {
                            self.dialog.on_user(text);
                        }
                        if let Some(text) = content.pointer("/outputTranscription/text").and_then(Value::as_str) {
                            self.dialog.on_bot(text);
                        }
                        for part in content.pointer("/modelTurn/parts").and_then(Value::as_array).into_iter().flatten() {
                            if let Some(data) = part.pointer("/inlineData/data").and_then(Value::as_str) {
                                self.audio(data).await?;
                                speaking = true;
                            }
                        }
                        if content["turnComplete"].as_bool() == Some(true) {
                            self.dialog.provider_turn_complete();
                        }
                    }
                    if kind == "session.delegation.created" {
                        self.dialog.working(1);
                    }
                    if kind == "response.event" {
                        let inner = event["event"]["type"].as_str().unwrap_or("");
                        if ["response.completed", "response.failed", "response.cancelled", "response.incomplete"].contains(&inner) {
                            self.dialog.working(-1);
                        }
                    }
                    let mut calls = vec![];
                    if kind == "response.event" && event["event"]["type"] == "response.output_item.done" && event["event"]["item"]["type"] == "function_call" {
                        calls.push(event["event"]["item"].clone());
                    }
                    if kind == "response.function_call_arguments.done" {
                        if self.id == "elevenlabs" {
                            if pending_calls.len() >= 32 {
                                return Err("Слишком много вызовов инструментов".into());
                            }
                            if !pending_calls.iter().any(|c| c["call_id"] == event["call_id"]) {
                                pending_calls.push(event.clone());
                            }
                        } else {
                            calls.push(event.clone());
                        }
                    }
                    if self.id == "elevenlabs" && kind == "response.done" {
                        calls = std::mem::take(&mut pending_calls);
                    }
                    calls.extend(event.pointer("/toolCall/functionCalls").and_then(Value::as_array).cloned().unwrap_or_default());
                    for call in calls {
                        if tasks.len() >= 16 {
                            return Err("Слишком много вызовов инструментов".into());
                        }
                        let call_id = call[if self.id == "gemini" { "id" } else { "call_id" }].as_str().unwrap_or("").to_string();
                        if call_id.is_empty() || call_id.len() > 256 {
                            return Err("Нет ID голосового инструмента".into());
                        }
                        if !seen.insert(call_id.clone()) {
                            continue;
                        }
                        if seen.len() > 1024 {
                            return Err("Слишком много инструментов в разговоре".into());
                        }
                        let Some(tools) = tools.clone() else { continue };
                        let id = self.id.clone();
                        let declared = defs.iter().any(|d| d["name"] == call["name"]);
                        self.dialog.working(1);
                        let handle = tasks.spawn(async move {
                            let name = call["name"].as_str().unwrap_or("").to_owned();
                            let arguments = call["arguments"].as_str().map(str::to_string).unwrap_or_else(|| call["args"].to_string());
                            let limit = if name == "play_music" { 35 } else { 15 };
                            let result = if declared {
                                match tokio::time::timeout(Duration::from_secs(limit), tools.call(&name, &arguments)).await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        if name == "play_music" {
                                            tools.cancel_loading_music().await;
                                        }
                                        "Инструмент не ответил вовремя".into()
                                    }
                                }
                            } else {
                                "Этот инструмент не был объявлен".into()
                            };
                            let payload = match id.as_str() {
                                "gemini" => json!({"toolResponse":{"functionResponses":[{"id":call["id"],"name":name,"response":{"result":result}}]}}),
                                _ => json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":call["call_id"],"output":result}}),
                            };
                            (name, arguments, result, payload)
                        });
                        handles.insert(call_id, handle);
                    }
                    if speaking && ["session.output_audio.done", "response.done"].contains(&kind) {
                        speaking = false;
                    }
                }
            }
        }
    }

    async fn audio(&self, data: &str) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        if data.len() > 128000 {
            return Err("Слишком большой кусок PCM".into());
        }
        let bytes = B64.decode(data).map_err(|_| "Неверный голосовой PCM")?;
        if bytes.len() % 2 != 0 {
            return Err("Неверный голосовой PCM".into());
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.dialog.heard_voice();
        let output = self.output.lock().unwrap().clone();
        if let Some(output) = output {
            for bytes in bytes.chunks(2400) {
                let mut pcm = bytes
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                loop {
                    if self.cancelled.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    match output.try_send(pcm) {
                        Ok(()) => break,
                        Err(TrySendError::Full(pending)) if tokio::time::Instant::now() < deadline => pcm = pending,
                        Err(TrySendError::Full(_)) => return Err("Динамик не успевает принять голос".into()),
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Ok(())
    }
}

async fn wait_gemini_setup(socket: &mut Link, key: &str) -> Result<(), (Option<u16>, String)> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.events.recv())
            .await
            .map_err(|_| (None, "Gemini не ответил на setup вовремя".to_string()))?;
        let Some(msg) = msg else {
            return Err((None, "Gemini закрыл соединение до начала разговора".into()));
        };
        let mut msg = msg.map_err(|e| (None, e))?;
        if let Some(close) = msg["close"].as_str() {
            let code = msg["code"].as_u64().map(|c| c as u16);
            let detail: String = close.replace(key, "***").chars().take(220).collect();
            let text = if detail.is_empty() {
                format!("gemini закрыл голосовое соединение до начала разговора (код {})", code.unwrap_or(1006))
            } else {
                format!("gemini: {detail}")
            };
            return Err((code, text));
        }
        let event = msg["event"].take();
        if event.get("setupComplete").is_some() {
            return Ok(());
        }
        if event["type"].as_str() == Some("error") || event.get("error").is_some() {
            let message = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Gemini отклонил голосовую сессию");
            let text: String = message.replace(key, "***").chars().take(240).collect();
            return Err((None, format!("gemini: {text}")));
        }
    }
}

fn log_gemini_switch(
    summary: &str,
    current_model: &str,
    next: Option<&crate::gemini_pool::Candidate>,
    allow_failover: bool,
) {
    if !allow_failover {
        log::warn!("Gemini: {summary}.");
        return;
    }
    match next {
        Some(n) if n.model == current_model => {
            log::warn!(
                "Gemini: {summary}. Переключаюсь на ключ #{} ({}).",
                n.key_index + 1,
                n.tail
            );
        }
        Some(n) => {
            log::warn!(
                "Gemini: {summary}. Свободных ключей на {current_model} нет, перехожу на {} (ключ #{} {}).",
                n.model,
                n.key_index + 1,
                n.tail
            );
        }
        None => {
            log::warn!("Gemini: {summary}. Свободных ключей и отмеченных моделей больше нет.");
        }
    }
}

struct SessionDiag {
    id: String,
    started_at: std::time::Instant,
    input_rate: u64,
    sent_chunks: u64,
    nonzero_chunks: u64,
    sent_samples: u64,
    events_in: u64,
    speech_started: u64,
    transcripts: u64,
    audio_chunks: u64,
    first_event_ms: Option<u128>,
    seen_kinds: Vec<String>,
}

impl SessionDiag {
    fn new(id: &str, input_rate: u64) -> Self {
        Self {
            id: id.to_owned(),
            started_at: std::time::Instant::now(),
            input_rate,
            sent_chunks: 0,
            nonzero_chunks: 0,
            sent_samples: 0,
            events_in: 0,
            speech_started: 0,
            transcripts: 0,
            audio_chunks: 0,
            first_event_ms: None,
            seen_kinds: Vec::new(),
        }
    }

    fn note_sent(&mut self, samples: usize, nonzero: bool) {
        self.sent_chunks += 1;
        self.sent_samples += samples as u64;
        if nonzero {
            self.nonzero_chunks += 1;
        }
    }

    fn note_event(&mut self, kind: &str) {
        self.events_in += 1;
        if self.first_event_ms.is_none() {
            self.first_event_ms = Some(self.started_at.elapsed().as_millis());
        }
        if !kind.is_empty() && self.seen_kinds.len() < 12 && !self.seen_kinds.iter().any(|k| k == kind) {
            self.seen_kinds.push(kind.to_owned());
        }
    }
}

impl Drop for SessionDiag {
    fn drop(&mut self) {
        if self.id != "gemini" && self.sent_chunks > 0 && self.audio_chunks == 0 {
            log::info!(
                "{}: сессия завершилась без аудиоответа (mic_rate={}, session_rate={}, кадров={}, ненулевых={}, сэмплов={}, событий={}, первое={} мс, speech_started={}, транскриптов={}, типы=[{}])",
                self.id,
                crate::audio::MIC_RATE,
                self.input_rate,
                self.sent_chunks,
                self.nonzero_chunks,
                self.sent_samples,
                self.events_in,
                self.first_event_ms.map_or_else(|| "-".into(), |ms| ms.to_string()),
                self.speech_started,
                self.transcripts,
                self.seen_kinds.join(", "),
            );
        }
    }
}

fn negotiated_input_rate(event: &Value) -> Option<u64> {
    let rate = event
        .pointer("/session/audio/input/format/rate")
        .or_else(|| event.pointer("/session/audio/format/rate"))
        .and_then(Value::as_u64)?;
    matches!(rate, 16_000 | 24_000).then_some(rate)
}

fn should_interrupt_on_event(kind: &str, speaking: bool, event: &Value) -> bool {
    (kind == "input_audio_buffer.speech_started" && speaking)
        || event.pointer("/serverContent/interrupted").and_then(Value::as_bool) == Some(true)
}

fn eleven_session(profile: &Profile, language: &str, instructions: &str, tools: &[Value]) -> Value {
    let model = match profile.live_model.as_str() {
        "eleven_realtime_v1_max" => "eleven_realtime_v1_max",
        _ => "eleven_realtime_v1_mini",
    };
    let voice = if profile.tts_voice.is_empty() {
        "brian"
    } else {
        &profile.tts_voice
    };
    let lang = if language.trim().is_empty() { "ru" } else { language.trim() };
    json!({"type":"realtime","model":model,"instructions":instructions,"output_modalities":["audio"],"tools":tools,"max_duration_seconds":600,
        "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"language":lang},"turn_detection":{"type":"semantic_vad","eagerness":"medium","create_response":true,"interrupt_response":true}},"output":{"format":{"type":"audio/pcm"},"voice":voice}}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eleven_config_preserves_voice_instructions_and_tools() {
        let p = Profile {
            live_model: "eleven_realtime_v1_max".into(),
            tts_voice: "custom-voice".into(),
            ..Default::default()
        };
        let config = eleven_session(&p, "ru", "Jarvis", &[json!({"type":"function","name":"test"})]);
        assert_eq!(config["model"], p.live_model);
        assert_eq!(config["audio"]["output"]["voice"], p.tts_voice);
        assert_eq!(config["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(config["tools"][0]["name"], "test");
        assert_eq!(config["instructions"], "Jarvis");
    }

    #[test]
    fn speech_started_does_not_mute_initial_turn_before_model_speaks() {
        let ev = json!({"type": "input_audio_buffer.speech_started"});
        assert!(!should_interrupt_on_event("input_audio_buffer.speech_started", false, &ev));
        assert!(should_interrupt_on_event("input_audio_buffer.speech_started", true, &ev));
        let gemini_intr = json!({"serverContent": {"interrupted": true}});
        assert!(should_interrupt_on_event("", false, &gemini_intr));
        assert_eq!(
            negotiated_input_rate(&json!({"type":"session.created","session":{"audio":{"input":{"format":{"type":"audio/pcm","rate":16000}}}}})),
            Some(16_000)
        );
        assert_eq!(
            negotiated_input_rate(&json!({"type":"session.created","session":{"audio":{"input":{"format":{"type":"audio/pcm","rate":24000}}}}})),
            Some(24_000)
        );
    }
}

struct Link {
    provider: Runtime,
    id: Value,
    events: tokio::sync::mpsc::Receiver<Result<Value, String>>,
    receiver: tokio::task::JoinHandle<()>,
}

impl Link {
    async fn open(provider: Runtime, key: &str, session: Value) -> Result<Self, String> {
        let opened = provider
            .request("live.open", json!({"key":key,"session":session}), Duration::from_secs(20))
            .await?;
        let id = opened["id"].clone();
        let (tx, events) = tokio::sync::mpsc::channel(12);
        let receiver = tokio::spawn({
            let provider = provider.clone();
            let id = id.clone();
            async move {
                loop {
                    let reply = provider
                        .request("live.recv", json!({"id":id}), Duration::from_secs(15))
                        .await;
                    match reply {
                        Ok(value) if value.is_null() => break,
                        Ok(value) if value["idle"] == true => continue,
                        _ => {}
                    }
                    let end = reply.is_err();
                    if tx.send(reply).await.is_err() || end {
                        break;
                    }
                }
            }
        });
        Ok(Self {
            provider,
            id,
            events,
            receiver,
        })
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.receiver.abort();
        let provider = self.provider.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            let _ = provider.notify("live.close", json!({"id":id})).await;
        });
    }
}

async fn send(socket: &Link, value: Value) -> Result<(), String> {
    socket
        .provider
        .request("live.send", json!({"id":socket.id,"event":value}), Duration::from_secs(5))
        .await
        .map(|_| ())
}

impl Session for Voice {
    fn out_boost(&self) -> f64 {
        if self.id == "elevenlabs" { 0.35 } else { 1.0 }
    }
    fn feed(&self, pcm: &[i16]) {
        queue_mic(&mut self.pending.lock().unwrap(), pcm);
    }
    fn take_audio(&self) -> Option<Receiver<Vec<i16>>> {
        self.audio.lock().unwrap().take()
    }
    fn rate(&self) -> usize {
        if self.id == "openai" { 16000 } else { 24000 }
    }
    fn ready(&self, timeout: Duration) -> Result<(), String> {
        let Some(rx) = self.ready.lock().unwrap().take() else {
            return Ok(());
        };
        rx.recv_timeout(timeout)
            .map_err(|_| "Голосовая сессия не открылась".to_string())?
    }
    fn say(&self, text: &str) -> Result<(), String> {
        self.commands.try_send(text.to_string()).map_err(|e| e.to_string())
    }
    fn close(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.close.notify_one();
    }
    fn dialog(&self) -> &Dialog {
        &self.dialog
    }
    fn interrupted(&self) -> bool {
        self.interrupted.swap(false, Ordering::SeqCst)
    }
    fn last_failure(&self) -> Option<VoiceFailure> {
        self.last_failure.lock().unwrap().clone()
    }
}
