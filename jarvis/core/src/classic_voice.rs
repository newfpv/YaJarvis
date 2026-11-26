//! Classic STT → tools/LLM → TTS, without a Go process.
use crate::{plugins::Host,settings::{Settings,Profile},tools::Tools,journal::Journal,session::{Session,Dialog}};
use std::{sync::{Arc,Mutex,atomic::{AtomicBool,AtomicU64,Ordering},mpsc::{Receiver,SyncSender,sync_channel}},time::Duration};
use tokio::sync::mpsc;
use serde_json::json;
use base64::{Engine as _,engine::general_purpose::STANDARD as B64};

pub struct Classic {
    dialog:Dialog,frames:mpsc::Sender<Vec<i16>>,say:mpsc::Sender<String>,
    out:Mutex<Option<SyncSender<Vec<i16>>>>,audio:Mutex<Option<Receiver<Vec<i16>>>>,
    closed:AtomicBool,generation:AtomicU64,interrupted:AtomicBool,stop:tokio::sync::Notify,
    last_failure:Mutex<Option<crate::voice_error::VoiceFailure>>,
}
struct Context {
    host:Arc<Host>,tools:Arc<Tools>,settings:Settings,profile:Profile,instructions:String,phrase:String,
    history:Mutex<Vec<crate::llm::Turn>>,
    gemini_pool:Option<Arc<crate::gemini_pool::GeminiPool>>,
    proxy:Option<Arc<crate::vpn::Proxy>>,
}
impl Classic {
    pub fn open(rt:&tokio::runtime::Handle,host:Arc<Host>,tools:Arc<Tools>,journal:Arc<Journal>,settings:Settings,profile:Profile,instructions:String,phrase:String)->Arc<Self>{
        Self::open_with(rt,host,tools,journal,settings,profile,instructions,phrase,None,None)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn open_with(rt:&tokio::runtime::Handle,host:Arc<Host>,tools:Arc<Tools>,journal:Arc<Journal>,settings:Settings,profile:Profile,instructions:String,phrase:String,gemini_pool:Option<Arc<crate::gemini_pool::GeminiPool>>,proxy:Option<Arc<crate::vpn::Proxy>>)->Arc<Self>{
        let (out,audio)=sync_channel(64);let (frames,mut input)=mpsc::channel(64);let (say,mut messages)=mpsc::channel(8);
        let model={let m=profile.effective_llm_model();let e=profile.effective_llm_engine();if !m.is_empty(){m}else if !e.is_empty(){e}else{&profile.backend_model}};
        let me=Arc::new(Self{dialog:Dialog::new(journal,"классический",model),frames,say,out:Mutex::new(Some(out)),audio:Mutex::new(Some(audio)),closed:AtomicBool::new(false),generation:AtomicU64::new(0),interrupted:AtomicBool::new(false),stop:tokio::sync::Notify::new(),last_failure:Mutex::new(None)});
        let actor=me.clone();let context=Arc::new(Context{host,tools,settings,profile,instructions,phrase,history:Mutex::new(Vec::new()),gemini_pool,proxy});
        rt.spawn(async move {
            let mut vad=Capture::default();let mut tasks=tokio::task::JoinSet::new();
            loop {tokio::select! {
                biased;
                _=actor.stop.notified()=>break,
                Some(text)=messages.recv()=>{
                    tasks.abort_all();let generation=actor.generation.fetch_add(1,Ordering::SeqCst)+1;
                    let me=actor.clone();let context=context.clone();tasks.spawn(async move{me.speak(&context,&text,generation).await});
                },
                result=tasks.join_next(),if !tasks.is_empty()=>{
                    actor.dialog.working(-1);
                    if let Some(Ok(Err(error)))=result {
                        actor.dialog.finish_turn(&error);
                        if actor.last_failure.lock().unwrap().is_some() {
                            break;
                        }
                    }
                },
                Some(pcm)=input.recv()=>{
                    let starting=!vad.speaking && crate::engine::loud(&pcm);
                    if starting {tasks.abort_all();actor.generation.fetch_add(1,Ordering::SeqCst);actor.interrupted.store(true,Ordering::SeqCst);}
                    if let Some(pcm)=vad.feed(&pcm){
                        let me=actor.clone();let context=context.clone();let generation=actor.generation.load(Ordering::SeqCst);
                        actor.dialog.working(1);tasks.spawn(async move{me.turn(&context,pcm,generation).await});
                    }
                },
            }}
            tasks.shutdown().await;actor.out.lock().unwrap().take();
        });me
    }
    fn record_failure(&self, context: &Context, provider: &str, stage: crate::voice_error::Stage, err: &str) {
        let vpn_blocked = context.proxy.as_ref().is_some_and(|p| matches!(p.route(provider), crate::vpn::Route::Blocked(_)));
        *self.last_failure.lock().unwrap() = Some(crate::voice_error::classify(provider, stage, err, None, vpn_blocked));
    }
    async fn turn(&self,context:&Context,pcm:Vec<i16>,generation:u64)->Result<(),String>{
        let stt=context.profile.effective_stt_engine();
        let wav_b64=B64.encode(wav(&pcm));
        let response=match stt {
            "openai"|"elevenlabs"=>{
                let key=if stt=="openai"{&context.settings.openai_key}else{&context.settings.elevenlabs_key};
                let model=if context.profile.stt_model.is_empty(){if stt=="openai"{"gpt-4o-mini-transcribe"}else{"scribe_v2"}}else{&context.profile.stt_model};
                match context.host.provider(stt)?.request(stt,json!({"op":"stt","key":key,"model":model,"language":context.settings.language,"audio":wav_b64}),Duration::from_secs(45)).await {
                    Ok(v) => v,
                    Err(e) => { self.record_failure(context, stt, crate::voice_error::Stage::Connect, &e); return Err(e); }
                }
            },
            id=>match context.host.plugin_stt(id,json!({"audio_b64":wav_b64,"format":"wav","rate":16000,"model":context.profile.stt_model,"language":context.settings.language})).await {
                Ok(v) => v,
                Err(e) => { self.record_failure(context, id, crate::voice_error::Stage::Connect, &e); return Err(e); }
            },
        };
        let text=response.as_str().or_else(||response["text"].as_str()).unwrap_or("").trim();if text.is_empty(){return Ok(());}
        self.dialog.on_user(text);
        let command = if context.phrase == crate::wake_profiles::DEFAULT_PHRASE { text.to_owned() }
            else { crate::fastcmd::command_for_phrase(text, &context.phrase) };
        let command = command.as_str();
        if crate::fastcmd::is_stop(command){
            self.dialog.annotate("стоп","");self.dialog.finish_turn("");self.close();return Ok(());
        }
        if context.tools.store.with(|s| s.profiles.len() > 1)
            && let Some(target) = crate::fastcmd::profile_switch_target(command)
        {
            let answer = match context.tools.switch_profile(&target) {
                Ok(answer) | Err(answer) => answer,
            };
            self.dialog.on_bot(&answer);
            self.speak(context, &answer, generation).await?;
            self.dialog.finish_turn("");
            return Ok(());
        }
        if let Some(question)=crate::fastcmd::clarification(command) {
            self.dialog.annotate("уточнение",question);self.speak(context,question,generation).await?;self.dialog.finish_turn("");return Ok(());
        }
        if let Some(control)=&context.tools.music_fast {
            let answer=control(command.to_string()).await;
            if let Some(answer)=answer{self.dialog.on_bot(&answer);self.speak(context,&answer,generation).await?;self.dialog.finish_turn("");return Ok(());}
        }
        if let Some(command)=crate::fastcmd::alice_skill_for(command,context.settings.use_home("yandexmusic")) {
            tokio::task::spawn_blocking(move||crate::alice::text(&command,false)).await.map_err(|e|e.to_string())??;
            self.dialog.on_bot("Передал Алисе");self.dialog.finish_turn("");return Ok(());
        }
        let mut close_after = false;
        let answer=if let Some(answer)=context.host.command(command).await{answer}else if let Some(answer)=context.tools.ha_vacuum(command).await{answer}else if let Some(answer)=context.tools.smartlife_fast(command).await{answer}else if let Some(answer)=context.tools.fast(command).await{answer}else{
            let history=context.history.lock().unwrap().clone();
            let (extra,defs)=context.tools.prepare().await;
            let llm_provider = {
                let e = context.profile.effective_llm_engine();
                if e.is_empty() { "classic" } else { e }
            };
            let reply=match crate::llm::ask_with_history_detail_pool(&context.host,&context.settings,&context.profile,&format!("{}\n{}\n{extra}",context.instructions,crate::rules::BACKEND_RULES),&history,text,&context.tools,defs,context.gemini_pool.as_deref()).await {
                Ok(r) => r,
                Err(e) => { self.record_failure(context, llm_provider, crate::voice_error::Stage::Connect, &e); return Err(e); }
            };
            close_after = reply.close_dialog && !crate::music::wants_keep_talking(text);
            if self.generation.load(Ordering::SeqCst)==generation{
                let mut h=context.history.lock().unwrap();
                h.push(crate::llm::Turn{role:"user",content:text.to_owned()});
                h.push(crate::llm::Turn{role:"assistant",content:reply.text.clone()});
                if h.len()>12{let drop_n=h.len()-12;h.drain(..drop_n);}
            }
            reply.text
        };
        if self.generation.load(Ordering::SeqCst)!=generation{return Ok(());}
        self.dialog.on_bot(&answer);
        if close_after {
            self.dialog.finish_turn("");
            self.dialog.request_close();
            return Ok(());
        }
        self.speak(context,&answer,generation).await?;self.dialog.finish_turn("");Ok(())
    }
    async fn speak(&self,context:&Context,text:&str,generation:u64)->Result<(),String>{
        let provider=context.profile.effective_tts_engine();
        let voice=if !context.profile.tts_voice.is_empty(){context.profile.tts_voice.as_str()}else if !context.profile.voice.is_empty(){context.profile.voice.as_str()}else if provider=="openai"{"alloy"}else{""};
        let result=match provider {
            "openai"=>{
                let model=if context.profile.tts_model.is_empty(){"gpt-4o-mini-tts"}else{&context.profile.tts_model};
                let body=json!({"model":model,"voice":voice,"input":text});
                match context.host.provider("openai")?.request("openai",json!({"op":"tts","key":&context.settings.openai_key,"voice":voice,"body":body}),Duration::from_secs(60)).await {
                    Ok(v) => v,
                    Err(e) => { self.record_failure(context, "openai", crate::voice_error::Stage::Connect, &e); return Err(e); }
                }
            },
            "elevenlabs"=>{
                let model=if context.profile.tts_model.is_empty(){crate::settings::ELEVEN_DEFAULT_MODEL}else{&context.profile.tts_model};
                let body=json!({"text":text,"model_id":model});
                match context.host.provider("elevenlabs")?.request("elevenlabs",json!({"op":"tts","key":&context.settings.elevenlabs_key,"voice":voice,"body":body}),Duration::from_secs(60)).await {
                    Ok(v) => v,
                    Err(e) => { self.record_failure(context, "elevenlabs", crate::voice_error::Stage::Connect, &e); return Err(e); }
                }
            },
            id=>match context.host.plugin_tts(id,json!({"text":text,"voice":voice,"model":context.profile.tts_model,"language":context.settings.language})).await {
                Ok(v) => v,
                Err(e) => { self.record_failure(context, id, crate::voice_error::Stage::Connect, &e); return Err(e); }
            },
        };
        let b64=result["audio"].as_str().or_else(||result["audio_b64"].as_str()).ok_or("Нет голоса TTS")?;
        let bytes=B64.decode(b64).map_err(|_|"Неверный голос TTS")?;
        let rate=result["rate"].as_u64().unwrap_or(24000) as usize;
        if !matches!(rate,16000|24000|48000) || bytes.len()%2!=0{return Err("Неверный формат TTS".into());}
        let samples:Vec<i16>=bytes.chunks_exact(2).map(|b|i16::from_le_bytes([b[0],b[1]])).collect();
        let samples=resample_pcm(&samples,rate,24000);
        for chunk in samples.chunks(1200){
            let mut pcm=chunk.to_vec();
            loop {
                if self.closed.load(Ordering::SeqCst) || self.generation.load(Ordering::SeqCst)!=generation{return Ok(());}
                let tx=self.out.lock().unwrap().clone().ok_or("Разговор закрыт")?;
                match tx.try_send(pcm){Ok(())=>break,Err(std::sync::mpsc::TrySendError::Full(data))=>pcm=data,Err(_)=>return Ok(())}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        Ok(())
    }
}
impl Session for Classic {
    fn handles_commands(&self)->bool{true}
    fn feed(&self,pcm:&[i16]){if !self.closed.load(Ordering::SeqCst){let _=self.frames.try_send(pcm.to_vec());}}
    fn take_audio(&self)->Option<Receiver<Vec<i16>>>{self.audio.lock().unwrap().take()}
    fn rate(&self)->usize{24000}
    fn ready(&self,_:Duration)->Result<(),String>{Ok(())}
    fn say(&self,text:&str)->Result<(),String>{self.say.try_send(text.to_string()).map_err(|e|e.to_string())}
    fn close(&self){self.closed.store(true,Ordering::SeqCst);self.stop.notify_one();}
    fn dialog(&self)->&Dialog{&self.dialog}
    fn interrupted(&self)->bool{self.interrupted.swap(false,Ordering::SeqCst)}
    fn last_failure(&self)->Option<crate::voice_error::VoiceFailure>{self.last_failure.lock().unwrap().clone()}
}
#[derive(Default)]struct Capture{preroll:Vec<i16>,speech:Vec<i16>,silence:usize,speaking:bool}
impl Capture {
    fn feed(&mut self,pcm:&[i16])->Option<Vec<i16>>{
        let voiced=crate::engine::loud(pcm);
        if !self.speaking && !voiced {self.preroll.extend(pcm);if self.preroll.len()>4800{self.preroll.drain(..self.preroll.len()-4800);}return None;}
        if !self.speaking {self.speaking=true;self.speech=std::mem::take(&mut self.preroll);}
        self.speech.extend(pcm);if voiced{self.silence=0;}else{self.silence+=pcm.len();}
        if self.silence>=11200 || self.speech.len()>=16000*45 {self.speaking=false;self.silence=0;let result=std::mem::take(&mut self.speech);if result.len()>=3200{return Some(result);}}
        None
    }
}
fn resample_pcm(samples:&[i16],from_rate:usize,to_rate:usize)->Vec<i16>{
    if from_rate==to_rate || samples.is_empty(){return samples.to_vec();}
    let out_len=(samples.len() as u64 * to_rate as u64 / from_rate as u64) as usize;
    (0..out_len).map(|i|{
        let pos=i as u64 * from_rate as u64;
        let left=(pos / to_rate as u64) as usize;
        let frac=(pos % to_rate as u64) as f32 / to_rate as f32;
        let a=f32::from(samples[left]);
        let b=f32::from(*samples.get(left+1).unwrap_or(&samples[left]));
        (a+(b-a)*frac).round().clamp(-32768.0,32767.0) as i16
    }).collect()
}
fn wav(pcm:&[i16])->Vec<u8>{
    let size=(pcm.len()*2) as u32;let mut out=Vec::with_capacity(size as usize+44);out.extend(b"RIFF");out.extend((size+36).to_le_bytes());out.extend(b"WAVEfmt ");out.extend(16u32.to_le_bytes());out.extend(1u16.to_le_bytes());out.extend(1u16.to_le_bytes());out.extend(16000u32.to_le_bytes());out.extend(32000u32.to_le_bytes());out.extend(2u16.to_le_bytes());out.extend(16u16.to_le_bytes());out.extend(b"data");out.extend(size.to_le_bytes());out.extend(pcm.iter().flat_map(|p|p.to_le_bytes()));out
}
#[cfg(test)]mod tests{use super::*;#[test]fn speech_capture_is_bounded_and_wav_has_real_lengths(){let mut capture=Capture::default();for _ in 0..30{assert!(capture.feed(&[0;1600]).is_none());}assert_eq!(capture.preroll.len(),4800);assert!(capture.feed(&[2000;3200]).is_none());let data=capture.feed(&[0;11200]).unwrap();assert!(!capture.speaking);let encoded=wav(&data);assert_eq!(&encoded[..4],b"RIFF");assert_eq!(encoded.len(),44+data.len()*2);}
    #[test]fn resamples_16k_24k_and_48k_to_24k(){
        let s16:Vec<i16>=(0..1600).map(|i|(i*10) as i16).collect();
        let r16=resample_pcm(&s16,16000,24000);assert_eq!(r16.len(),2400);assert_eq!(r16[0],0);
        let s24=vec![123i16;2400];assert_eq!(resample_pcm(&s24,24000,24000),s24);
        let s48:Vec<i16>=(0..4800).map(|i|(i*5) as i16).collect();
        let r48=resample_pcm(&s48,48000,24000);assert_eq!(r48.len(),2400);assert_eq!(r48[10],100);
    }
    #[cfg(feature="lua")]
    #[tokio::test]
    async fn classic_pipeline_runs_lua_stt_llm_with_history_and_tts(){
        let dir=std::env::temp_dir().join(format!("jarvis-classic-lua-{}",crate::settings::rand_hex(6)));
        for (id,provides,src) in [
            ("my-stt","stt",r#"local n=0;return {transcribe=function(req) n=n+1;assert(req.rate==16000 and req.model=='whisper-x');return {text='вопрос '..n} end}"#),
            ("my-llm","llm",r#"return {chat=function(req) assert(req.model=='custom-m');if #req.messages==2 then return 'ответ 1' end;assert(#req.messages==4 and req.messages[2].content=='вопрос 1' and req.messages[3].content=='ответ 1');return 'ответ 2' end}"#),
            ("my-tts","tts",r#"return {speak=function(req) return {audio='AQABAA==',rate=16000} end}"#),
        ]{
            let pdir=dir.join("plugins").join(id);std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("plugin.json"),json!({"id":id,"name":id,"version":"1","kind":"lua","category":"assistant","main":"main.lua","provides":[provides]}).to_string()).unwrap();
            std::fs::write(pdir.join("main.lua"),src).unwrap();
        }
        let host=Host::new(dir.clone(),crate::plugins::Hooks::default());host.load().await;
        let store=crate::settings::Store::open(dir.join("settings.json")).unwrap();
        let tools=Arc::new(Tools::new(host.clone(),store));
        let journal=Arc::new(Journal::default());
        let profile=Profile{mode:"classic".into(),engine:"classic".into(),stt_engine:"my-stt".into(),stt_model:"whisper-x".into(),llm_engine:"my-llm".into(),llm_model:"custom-m".into(),tts_engine:"my-tts".into(),tts_voice:"v1".into(),..Default::default()};
        let sess=Classic::open(&tokio::runtime::Handle::current(),host.clone(),tools.clone(),journal.clone(),Settings::default(),profile.clone(),"inst".into(),"Джарвис".into());
        let rx=sess.take_audio().unwrap();
        let ctx=Context{host:host.clone(),tools,settings:Settings::default(),profile,instructions:"inst".into(),phrase:"Джарвис".into(),history:Mutex::new(Vec::new()),gemini_pool:None,proxy:None};
        sess.turn(&ctx,vec![0;3200],0).await.unwrap();
        let pcm1=rx.recv_timeout(Duration::from_secs(2)).unwrap();assert_eq!(pcm1.len(),3);
        sess.turn(&ctx,vec![0;3200],0).await.unwrap();
        let pcm2=rx.recv_timeout(Duration::from_secs(2)).unwrap();assert_eq!(pcm2.len(),3);
        let items=journal.items(10);assert!(items.iter().any(|t|t.answer=="ответ 2"));
        sess.close();host.stop_all().await;std::fs::remove_dir_all(dir).unwrap();
    }
}
