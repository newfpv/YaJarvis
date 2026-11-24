use super::{Engine, INTERRUPTED, Mode, Profile, Settings, voice_preview};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) const PHRASES: &[&str] = &[
    "М?",
    "Слушаю.",
    "Тут.",
    "Ну?",
    "Да?",
    "Я тут.",
    "Угу.",
    "Ага.",
    "Говори.",
    "На связи.",
    "Да, слушаю.",
    "Хм?",
];
pub(super) const FOLLOW_ON_GAP: Duration = Duration::from_millis(70);
const MAX_REPLY: Duration = Duration::from_millis(900);
const CACHE_BUDGET: u64 = 2 << 20;

#[derive(Default)]
struct ManagerState {
    error: String,
    error_code: String,
    done: usize,
    total: usize,
    identity: String,
}

#[derive(Default)]
pub(super) struct WakeReplyManager {
    generation: AtomicU64,
    running: AtomicBool,
    state: Mutex<ManagerState>,
}

fn identity(profile: &Profile) -> (&str, &str, &str, &str) {
    let engine = if profile.is_classic() {
        profile.effective_tts_engine()
    } else {
        profile.engine.as_str()
    };
    let voice = if (profile.is_classic() || profile.engine == "eleven_realtime")
        && !profile.tts_voice.is_empty()
    {
        &profile.tts_voice
    } else {
        &profile.voice
    };
    // Для Gemini голос одинаков на всех Live-моделях — не привязываем кеш откликов к одной модели,
    // чтобы при переходе на запасную модель (например 2.5 Flash) отклики продолжали работать.
    let model = if engine == "gemini" {
        "gemini-live"
    } else {
        &profile.live_model
    };
    (engine, voice, model, &profile.tts_model)
}

fn identity_key(profile: &Profile) -> String {
    let (engine, voice, model, tts_model) = identity(profile);
    format!("{engine}|{voice}|{model}|{tts_model}")
}

fn reply_name(profile: &Profile, phrase: &str) -> String {
    let mut voice = profile.clone();
    let (_, v, model, _) = identity(profile);
    voice.voice = v.into();
    if profile.engine == "gemini" {
        voice.live_model = model.into();
    }
    voice_preview::stable_sample_name(&voice, &format!("wake-reply/v1/{phrase}"))
}

fn count_ready(directory: &Path, profile: &Profile) -> usize {
    PHRASES
        .iter()
        .filter(|&&phrase| {
            let path = directory.join(reply_name(profile, phrase));
            voice_preview::Sample::read(&path).is_some_and(|s| short_enough(&s))
        })
        .count()
}

impl Engine {
    /// Текущий статус кеша и ручной генерации быстрых откликов для активного ассистента.
    pub fn wake_reply_status(&self) -> Value {
        let settings = self.store.get();
        let profile = settings.active_profile();
        let (engine, voice, model, _) = identity(&profile);
        let dir = self.data.join("wake-replies");
        let ready = count_ready(&dir, &profile);
        let missing = crate::engine::tts_missing(&self.host, &settings, &profile);
        let running = self.wake_replies.running.load(Ordering::Acquire);
        let id_key = identity_key(&profile);
        let st = self.wake_replies.state.lock().unwrap();
        let same_profile = st.identity == id_key;
        json!({
            "engine": engine,
            "engine_name": crate::voice_error::provider_display(engine),
            "voice": if voice.is_empty() { "по умолчанию" } else { voice },
            "model": model,
            "ready": ready,
            "total": PHRASES.len(),
            "running": running && same_profile,
            "done": if running && same_profile { st.done } else { ready },
            "error": if same_profile { &st.error } else { "" },
            "error_code": if same_profile { &st.error_code } else { "" },
            "can_generate": missing.is_empty(),
            "missing": missing,
        })
    }

    /// Явный запуск генерации недостающих фраз по команде из панели.
    /// Без фоновых циклов и без прожига запасных ключей: при первом лимите сразу стоп.
    pub fn start_wake_reply_generation(self: &Arc<Self>) -> Result<Value, String> {
        let settings = self.store.get();
        let profile = settings.active_profile();
        let missing = crate::engine::tts_missing(&self.host, &settings, &profile);
        if !missing.is_empty() {
            return Err(missing);
        }
        if self.talking() || self.player.playing() {
            return Err("Сейчас идёт разговор — дождитесь окончания".into());
        }
        if profile.engine == "gemini" {
            let keys = crate::gemini_pool::effective_keys(&settings.gemini_keys, &settings.gemini_key);
            let models = crate::gemini_pool::selected_live_models(&profile);
            let now = crate::gemini_pool::now_unix();
            if self.gemini_pool.candidates(&keys, &settings.gemini_key, &models, now).is_empty() {
                let tz = self.store.timezone();
                let f = self.gemini_pool.all_exhausted_failure(&keys, &models, now, &tz);
                return Err(format!("{}. {}", f.detail, f.action));
            }
        }
        let directory = self.data.join("wake-replies");
        let pending: Vec<(&'static str, std::path::PathBuf)> = PHRASES
            .iter()
            .filter_map(|&phrase| {
                let path = directory.join(reply_name(&profile, phrase));
                let ok = voice_preview::Sample::read(&path).is_some_and(|s| short_enough(&s));
                (!ok).then_some((phrase, path))
            })
            .collect();
        if pending.is_empty() {
            return Ok(self.wake_reply_status());
        }
        if self
            .wake_replies
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            return Err("Генерация откликов уже идёт".into());
        }

        let gen_id = self.wake_replies.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let id_key = identity_key(&profile);
        let initial_ready = PHRASES.len().saturating_sub(pending.len());
        {
            let mut st = self.wake_replies.state.lock().unwrap();
            st.error.clear();
            st.error_code.clear();
            st.done = initial_ready;
            st.total = PHRASES.len();
            st.identity = id_key.clone();
        }

        let engine = self.clone();
        std::thread::Builder::new()
            .name("wake-reply-gen".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                struct RunGuard<'a>(&'a Engine);
                impl Drop for RunGuard<'_> {
                    fn drop(&mut self) {
                        self.0.wake_replies.running.store(false, Ordering::Release);
                    }
                }
                let _guard = RunGuard(&engine);
                let cancelled = || {
                    engine.wake_replies.generation.load(Ordering::SeqCst) != gen_id
                        || engine.talking()
                        || engine.player.playing()
                        || engine.store.with(|s| identity_key(&s.active_profile()) != id_key)
                };
                log::info!(
                    "Отклики на активацию: начинаю генерацию {} недостающих фраз (уже готово {initial_ready} из {}, движок={})",
                    pending.len(),
                    PHRASES.len(),
                    profile.engine
                );
                let res = engine.record_wake_replies(
                    settings,
                    &profile,
                    &directory,
                    &pending,
                    initial_ready,
                    &cancelled,
                );
                let now_ready = count_ready(&directory, &profile);
                let mut st = engine.wake_replies.state.lock().unwrap();
                st.done = now_ready;
                match res {
                    Ok(recorded) => {
                        st.error.clear();
                        st.error_code.clear();
                        log::info!(
                            "Отклики на активацию: готово {now_ready} из {} (добавлено {recorded})",
                            PHRASES.len()
                        );
                    }
                    Err((code, error)) if error == INTERRUPTED => {
                        st.error_code = code;
                        st.error = if now_ready > 0 {
                            format!("Генерация остановлена. Уже готово {now_ready} из {} фраз — они работают.", PHRASES.len())
                        } else {
                            "Генерация остановлена.".into()
                        };
                        log::info!("Отклики на активацию: генерация остановлена (готово {now_ready}/{})", PHRASES.len());
                    }
                    Err((code, error)) => {
                        st.error_code = code;
                        st.error = if now_ready > 0 {
                            format!("Остановлено на {now_ready} из {}: {error}. Уже готовые {now_ready} фраз сохранены и работают.", PHRASES.len())
                        } else {
                            format!("Не удалось сгенерировать отклики: {error}")
                        };
                        log::warn!("Отклики на активацию: остановил генерацию на {now_ready}/{}: {error}", PHRASES.len());
                    }
                }
            })
            .map_err(|e| format!("не запустить поток генерации: {e}"))?;

        Ok(self.wake_reply_status())
    }

    pub fn cancel_wake_reply_generation(&self) {
        self.wake_replies.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn clear_wake_reply_cache(&self) -> usize {
        self.cancel_wake_reply_generation();
        let profile = self.store.with(|s| s.active_profile());
        let dir = self.data.join("wake-replies");
        let mut removed = 0;
        for &phrase in PHRASES {
            let path = dir.join(reply_name(&profile, phrase));
            if fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
        let mut st = self.wake_replies.state.lock().unwrap();
        st.error.clear();
        st.error_code.clear();
        st.done = 0;
        removed
    }

    fn record_wake_replies(
        &self,
        settings: Settings,
        profile: &Profile,
        directory: &Path,
        pending: &[(&str, std::path::PathBuf)],
        initial_ready: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<usize, (String, String)> {
        fs::create_dir_all(directory).map_err(|error| ("io".into(), error.to_string()))?;
        if cancelled() {
            return Err(("cancelled".into(), INTERRUPTED.into()));
        }
        let session = self.preview_session_no_failover(settings, profile.clone());
        let result = (|| {
            let ready = session.ready(Duration::from_secs(12));
            if cancelled() {
                return Err(("cancelled".into(), INTERRUPTED.into()));
            }
            if let Err(err) = ready {
                let code = session
                    .last_failure()
                    .map(|f| f.code().to_string())
                    .unwrap_or_else(|| "connect".into());
                return Err((code, err));
            }
            let output = session
                .take_audio()
                .ok_or_else(|| ("audio".into(), "Голосовой движок не открыл аудио".into()))?;
            let mut recorded = 0usize;
            let pacing = if profile.engine == "gemini" {
                Duration::from_millis(750)
            } else {
                Duration::from_millis(150)
            };
            for (idx, (phrase, path)) in pending.iter().enumerate() {
                if idx > 0 {
                    let wait_until = Instant::now() + pacing;
                    while Instant::now() < wait_until {
                        if cancelled() {
                            return Err(("cancelled".into(), INTERRUPTED.into()));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                let sample = match voice_preview::capture_phrase(
                    &*session,
                    &output,
                    phrase,
                    Duration::from_secs(10),
                    cancelled,
                    false,
                ) {
                    Ok(s) => s,
                    Err(err) => {
                        let code = session
                            .last_failure()
                            .map(|f| f.code().to_string())
                            .unwrap_or_else(|| {
                                if err == INTERRUPTED { "cancelled" } else { "generate" }.into()
                            });
                        let detail = session
                            .last_failure()
                            .map(|f| f.detail)
                            .unwrap_or(err);
                        return Err((code, detail));
                    }
                };
                if short_enough(&sample) {
                    recorded += 1;
                    crate::settings::write_atomic(path, &sample.encode(), 0o600)
                        .map_err(|error| ("io".into(), error.to_string()))?;
                    voice_preview::prune(directory, path, CACHE_BUDGET);
                    let mut st = self.wake_replies.state.lock().unwrap();
                    st.done = initial_ready + recorded;
                }
            }
            Ok(recorded)
        })();
        session.close();
        result
    }

    pub(super) fn cached_wake_reply(&self, profile: &Profile) -> Option<voice_preview::Sample> {
        let directory = self.data.join("wake-replies");
        let start = self.wake_reply_sequence.fetch_add(1, Ordering::Relaxed) as usize;
        (0..PHRASES.len()).find_map(|offset| {
            let path = directory.join(reply_name(
                profile,
                PHRASES[start.wrapping_add(offset) % PHRASES.len()],
            ));
            voice_preview::Sample::read(&path).filter(short_enough)
        })
    }

    pub(super) fn play_wake_reply(&self, sample: &voice_preview::Sample, engine: &str, opening_at: Instant) {
        if self.player.start(sample.rate).is_err() {
            return;
        }
        self.player.set_boost(sample.boost * 0.65);
        self.leds.set(Mode::Listening);
        if self.player.write(&sample.pcm).is_ok() {
            log::info!("Отклик на активацию из кеша ({engine}) за {:.2} с", opening_at.elapsed().as_secs_f64());
            self.player.finish(MAX_REPLY + Duration::from_millis(200));
        } else {
            self.player.stop(true);
        }
    }
}

fn short_enough(sample: &voice_preview::Sample) -> bool {
    sample.pcm.len() as u128 * 1000 <= sample.rate as u128 * MAX_REPLY.as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_replies_are_separate_for_each_voice_and_model_and_stable_across_versions() {
        assert!((10..=15).contains(&PHRASES.len()));
        let mut profile = Profile {
            engine: "gemini".into(),
            voice: "Kore".into(),
            live_model: "gemini-3.8-live".into(),
            ..Default::default()
        };
        let original = reply_name(&profile, PHRASES[0]);
        // Стабильное имя не зависит от crate::version(), поэтому отличается от sample_name с версией.
        assert_ne!(original, voice_preview::sample_name(&profile, &format!("wake-reply/v1/{}", PHRASES[0])));
        assert_ne!(reply_name(&profile, PHRASES[1]), original);
        profile.voice = "Fenrir".into();
        assert_ne!(reply_name(&profile, PHRASES[0]), original);
        profile.voice = "Kore".into();
        // Для Gemini отклики переиспользуются при переключении на запасную модель.
        profile.live_model = "gemini-2.5-flash-native-audio-preview-12-2025".into();
        assert_eq!(reply_name(&profile, PHRASES[0]), original);
    }
}
