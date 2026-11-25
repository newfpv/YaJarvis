mod account;
mod media;
mod player_script;
mod remote;

use account::{Account, COOKIES};
use base64::{Engine, engine::general_purpose::STANDARD};
use jarvis_home_common::{Host, load, normal, random, save, text};
use remote::{
    Playlist, Remote, SearchFilter, SearchItem, Stream, Track, playlist_id_from_url, valid_id,
    valid_playlist_id, video_id,
};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, OnceCell, oneshot},
    task::JoinHandle,
};

const HISTORY: &str = "youtube-progress.json";
const LIBRARY_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Default)]
struct LibraryCache {
    liked: Option<(Instant, Vec<Track>)>,
    playlists: Option<(Instant, Vec<Playlist>)>,
    items: BTreeMap<String, (Instant, Vec<Track>)>,
}

impl LibraryCache {
    fn clear(&mut self) {
        self.liked = None;
        self.playlists = None;
        self.items.clear();
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    track: Track,
    pos: f64,
    dur: f64,
    updated: String,
    complete: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    Loading,
    Buffering,
    Playing,
    Paused,
}

#[derive(Default)]
struct State {
    phase: Phase,
    generation: u64,
    track: Track,
    pos: f64,
    stream: Option<Stream>,
    queue: VecDeque<Track>,
    related: VecDeque<Track>,
    history: BTreeMap<String, Saved>,
    liked_ids: HashSet<String>,
    liked_loaded: bool,
    saved: Option<Instant>,
    proposal: Option<(Instant, VecDeque<Track>)>,
    personal: bool,
    repeat: bool,
    error: String,
    task: Option<JoinHandle<()>>,
}

impl State {
    fn playing(&self) -> bool {
        matches!(self.phase, Phase::Playing | Phase::Buffering)
    }
    fn dur(&self) -> f64 {
        self.stream.as_ref().map_or(0.0, |v| v.duration)
    }
    fn proposals(&self) -> bool {
        self.proposal
            .as_ref()
            .is_some_and(|(until, tracks)| Instant::now() < *until && !tracks.is_empty())
    }
    fn current(&self) -> Value {
        let active = self.phase != Phase::Idle;
        let status = match self.phase {
            Phase::Idle if !self.error.is_empty() => self.error.clone(),
            Phase::Idle => "Сейчас ничего не играет".into(),
            Phase::Loading => "Загружаю YouTube".into(),
            Phase::Paused => format!("На паузе «{}»", self.track.title),
            Phase::Playing | Phase::Buffering => format!("Играет «{}»", self.track.title),
        };
        json!({
            "playing":self.playing(), "active":active, "paused":self.phase==Phase::Paused,
            "proposal":self.proposals(), "personal_search":self.personal, "repeat":self.repeat,
            "cur":self.track, "pos":(self.pos*10.0).round()/10.0, "status":status
        })
    }
    fn snapshot(&self, radio: bool) -> Value {
        let active = self.phase != Phase::Idle;
        let next: Vec<_> = self
            .queue
            .iter()
            .chain(self.related.iter().filter(|_| radio))
            .take(20)
            .collect();
        let mut history: Vec<_> = self.history.values().collect();
        history.sort_by_key(|entry| std::cmp::Reverse(&entry.updated));
        let history: Vec<_> = history.iter().take(30).map(|entry| &entry.track).collect();
        let liked = active && self.liked_ids.contains(&self.track.id);
        let liked_ids: Vec<_> = self.liked_ids.iter().cloned().collect();
        json!({
            "source":"youtube", "playing":self.playing(), "paused":self.phase==Phase::Paused,
            "loading":matches!(self.phase, Phase::Loading | Phase::Buffering), "track":if active{self.track.clone()}else{Track::default()},
            "pos":if active{self.pos}else{0.0}, "dur":if active{self.dur()}else{0.0},
            "client":self.stream.as_ref().map_or("",|v|v.client), "next":next, "queue":self.queue.len(),
            "radio":radio, "repeat":self.repeat, "liked":liked, "liked_ids":liked_ids, "history":history, "error":self.error
        })
    }
}

pub struct Youtube {
    host: Arc<Host>,
    account: Arc<Account>,
    remote: Arc<Remote>,
    state: Mutex<State>,
    ready: OnceCell<()>,
    request: AsyncMutex<()>,
    library_cache: AsyncMutex<LibraryCache>,
    save_lock: Arc<AsyncMutex<()>>,
    wake: Notify,
    closed: AtomicBool,
    gain: AtomicU32,
}

impl Youtube {
    pub fn new(host: Host) -> Self {
        let host = Arc::new(host);
        Self {
            remote: Arc::new(Remote::new(host.clone())),
            account: Arc::new(Account::new(host.clone())),
            host,
            state: Mutex::default(),
            ready: OnceCell::default(),
            request: AsyncMutex::default(),
            library_cache: AsyncMutex::default(),
            save_lock: Arc::new(AsyncMutex::default()),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            gain: AtomicU32::new(0.95f32.to_bits()),
        }
    }

    async fn init(&self) -> Result<(), String> {
        self.ready
            .get_or_try_init(|| async {
                let marker = self.host.data.join(".migrated");
                if !marker.is_file() {
                    for name in [COOKIES, HISTORY] {
                        let path = self.host.data.join(name);
                        if path.exists() {
                            continue;
                        }
                        let raw =
                            (self.host.services)("legacy".into(), json!({"name":name})).await?;
                        if let Some(encoded) = raw.as_str() {
                            save(
                                &path,
                                &STANDARD
                                    .decode(encoded)
                                    .map_err(|_| "Повреждены данные YouTube")?,
                            )?;
                        }
                    }
                    save(&marker, b"rust\n")?;
                }
                let history_path = self.host.data.join(HISTORY);
                let history = if history_path.is_file() {
                    load(&history_path).unwrap_or_else(|e| {
                        log::warn!("YouTube: {e}");
                        Value::Null
                    })
                } else {
                    Value::Null
                };
                self.state.lock().unwrap().history =
                    serde_json::from_value(history).unwrap_or_default();
                Ok(())
            })
            .await
            .map(|_| ())
    }

    pub fn watch(self: &Arc<Self>) {
        let warm_weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if let Some(service) = warm_weak.upgrade()
                && !service.closed.load(Ordering::Acquire)
            {
                service.remote.warm_background().await;
            }
        });
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut last = Value::Null;
            loop {
                let Some(service) = weak.upgrade() else { break };
                if service.closed.load(Ordering::Acquire) {
                    break;
                }
                let (mut state, playing) = {
                    let st = service.state.lock().unwrap();
                    (st.current(), st.playing())
                };
                state["pos"] = state["pos"].as_f64().unwrap_or_default().floor().into();
                if state != last {
                    if state["playing"] == true && last["playing"] != true {
                        service.event("music.play", Value::Null).await;
                    }
                    service.event("music.state", state.clone()).await;
                    last = state;
                }
                if playing {
                    tokio::select! {
                        _ = service.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                } else {
                    tokio::select! {
                        _ = service.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(1800)) => {
                            service.remote.evict_idle(Duration::from_secs(1800)).await;
                        }
                    }
                }
            }
        });
    }

    pub fn volume(&self, input: &Value) -> Value {
        if let Some(value) = input["v"].as_f64().filter(|v| (0.0..=1.0).contains(v)) {
            self.gain.store((value as f32).to_bits(), Ordering::Relaxed);
        }
        true.into()
    }

    pub async fn stop(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify_one();
        self.account.close();
        self.halt(false).await;
        self.emit_state().await;
        self.host.close();
    }

    async fn event(&self, method: &str, value: Value) {
        let _ = (self.host.services)(method.into(), value).await;
    }

    async fn emit_state(&self) {
        let state = self.state.lock().unwrap().current();
        self.event("music.state", state).await;
    }

    fn record_progress(&self, state: &mut State, complete: bool) -> Option<Vec<u8>> {
        if !valid_id(&state.track.id) || state.pos <= 0.0 {
            return None;
        }
        state.saved = Some(Instant::now());
        let previous = state.history.get(&state.track.id);
        if previous.is_some_and(|entry| entry.complete)
            && !complete
            && state.pos >= state.dur() - 20.0
        {
            return None;
        }
        let mut track = state.track.clone();
        if let Some(prev) = previous {
            if needs_title(&track) && !needs_title(&prev.track) {
                track.title = prev.track.title.clone();
            }
            if track.author.is_empty() && !prev.track.author.is_empty() {
                track.author = prev.track.author.clone();
            }
        }
        let entry = Saved {
            track,
            pos: state.pos,
            dur: state.dur(),
            updated: jiff::Timestamp::now().to_string(),
            complete,
        };
        state.history.insert(entry.track.id.clone(), entry);
        if state.history.len() > 100 {
            if let Some(id) = state
                .history
                .iter()
                .min_by_key(|(_, item)| &item.updated)
                .map(|(id, _)| id.clone())
            {
                state.history.remove(&id);
            }
        }
        match serde_json::to_vec(&state.history) {
            Ok(data) => Some(data),
            Err(error) => {
                log::warn!("YouTube: сохранить позицию: {error}");
                None
            }
        }
    }

    fn persist_progress_bg(self: &Arc<Self>, bytes: Vec<u8>) {
        let Ok(guard) = self.save_lock.clone().try_lock_owned() else {
            return;
        };
        let path = self.host.data.join(HISTORY);
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(error) = tokio::task::spawn_blocking(move || save(&path, &bytes))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            {
                log::warn!("YouTube: сохранить позицию: {error}");
            }
        });
    }

    async fn persist_progress_sync(&self, bytes: Vec<u8>) {
        let _guard = self.save_lock.lock().await;
        let path = self.host.data.join(HISTORY);
        if let Err(error) = tokio::task::spawn_blocking(move || save(&path, &bytes))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        {
            log::warn!("YouTube: сохранить позицию: {error}");
        }
    }

    async fn halt(&self, pause: bool) -> (bool, u64) {
        let (active, generation, task, save_bytes) = {
            let mut state = self.state.lock().unwrap();
            let active = state.phase != Phase::Idle;
            let save_bytes = if active {
                self.record_progress(&mut state, false)
            } else {
                None
            };
            state.generation = state.generation.wrapping_add(1);
            if pause && (state.stream.is_some() || valid_id(&state.track.id)) && active {
                state.phase = Phase::Paused;
            } else {
                state.phase = Phase::Idle;
                state.stream = None;
                state.track = Track::default();
                state.pos = 0.0;
            }
            if !pause {
                state.proposal = None;
                state.error.clear();
            }
            (active, state.generation, state.task.take(), save_bytes)
        };
        self.wake.notify_one();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        if let Some(bytes) = save_bytes {
            self.persist_progress_sync(bytes).await;
        }
        (active, generation)
    }

    async fn halt_transition(self: &Arc<Self>, pos: f64) -> u64 {
        let (generation, task, save_bytes) = {
            let mut state = self.state.lock().unwrap();
            let save_bytes = if state.phase != Phase::Idle {
                self.record_progress(&mut state, false)
            } else {
                None
            };
            state.generation = state.generation.wrapping_add(1);
            state.phase = Phase::Buffering;
            state.pos = pos;
            state.error.clear();
            (state.generation, state.task.take(), save_bytes)
        };
        self.wake.notify_one();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        if let Some(bytes) = save_bytes {
            self.persist_progress_bg(bytes);
        }
        generation
    }

    async fn radio(&self) -> bool {
        self.host
            .config()
            .await
            .ok()
            .is_some_and(|config| config["radio_off"] != true)
    }

    async fn search(&self, query: &str, personal: bool) -> Result<Vec<Track>, String> {
        let query = query.trim();
        if query.len() > 2048 {
            return Err("Слишком длинный запрос".into());
        }
        if query.is_empty() {
            return Err("Что включить?".into());
        }
        if let Some(id) = video_id(query).or_else(|| valid_id(query).then(|| query.into())) {
            return Ok(vec![Track::from_id(id)]);
        }
        if personal {
            let headers = self.account.web_headers();
            if !headers.is_empty() {
                match self.remote.search(query, headers).await {
                    Ok(results) if !results.is_empty() => return Ok(results),
                    Ok(_) => {}
                    Err(error) => log::debug!("YouTube: персональный поиск не удался: {error}"),
                }
            }
        }
        self.remote.search(query, HeaderMap::new()).await
    }

    async fn lookup_known_track(&self, id: &str) -> Option<Track> {
        let mut out = Track::from_id(id.to_string());
        let merge = |dst: &mut Track, src: &Track| {
            if needs_title(dst) && !needs_title(src) {
                dst.title = src.title.clone();
            }
            if dst.author.is_empty() && !src.author.is_empty() {
                dst.author = src.author.clone();
            }
        };
        {
            let state = self.state.lock().unwrap();
            if state.track.id == id {
                merge(&mut out, &state.track);
            }
            for t in state.queue.iter().chain(state.related.iter()) {
                if t.id == id {
                    merge(&mut out, t);
                }
            }
            if let Some(saved) = state.history.get(id) {
                merge(&mut out, &saved.track);
            }
        }
        if needs_title(&out) || out.author.is_empty() {
            let cache = self.library_cache.lock().await;
            if let Some((_, liked)) = &cache.liked {
                for t in liked {
                    if t.id == id {
                        merge(&mut out, t);
                    }
                }
            }
            for (_, (_, list)) in &cache.items {
                for t in list {
                    if t.id == id {
                        merge(&mut out, t);
                    }
                }
            }
        }
        (!needs_title(&out) || !out.author.is_empty()).then_some(out)
    }

    async fn resolve(&self, track: Track, prefer_tv: bool) -> Result<(Track, Stream), String> {
        let anonymous = self.remote.resolve(&track.id, HeaderMap::new(), prefer_tv).await;
        let (mut info, stream) = match anonymous {
            Ok(value) => value,
            Err(e) => {
                let auth = self.account.headers(false).await?;
                if auth.is_empty() {
                    return Err(e);
                }
                self.remote.resolve(&track.id, auth, true).await?
            }
        };
        if needs_title(&info) && !needs_title(&track) {
            info.title = track.title;
        }
        if info.author.is_empty() && !track.author.is_empty() {
            info.author = track.author;
        }
        if needs_title(&info) || info.author.is_empty() {
            if let Some(known) = self.lookup_known_track(&info.id).await {
                if needs_title(&info) && !needs_title(&known) {
                    info.title = known.title;
                }
                if info.author.is_empty() && !known.author.is_empty() {
                    info.author = known.author;
                }
            }
        }
        if (needs_title(&info) || info.author.is_empty())
            && let Ok(meta) = self.remote.oembed(&info.id).await
        {
            if needs_title(&info) && !needs_title(&meta) {
                info.title = meta.title;
            }
            if info.author.is_empty() && !meta.author.is_empty() {
                info.author = meta.author;
            }
        }
        if info.title.trim().is_empty() {
            info.title = info.id.clone();
        }
        Ok((info, stream))
    }

    fn launch(
        self: &Arc<Self>,
        generation: u64,
        tracks: Vec<Track>,
        start: f64,
        stream: Option<Stream>,
        keep_playing: bool,
    ) -> oneshot::Receiver<Result<Track, String>> {
        let (notify, result) = oneshot::channel();
        let mut state = self.state.lock().unwrap();
        if state.generation != generation || self.closed.load(Ordering::Acquire) {
            let _ = notify.send(Err("Воспроизведение отменено".into()));
            return result;
        }
        state.phase = if keep_playing {
            Phase::Buffering
        } else {
            Phase::Loading
        };
        state.track = tracks.first().cloned().unwrap_or_default();
        state.stream = stream.clone();
        state.pos = start;
        state.error.clear();
        state.saved = None;
        self.wake.notify_one();
        let service = self.clone();
        state.task = Some(tokio::spawn(async move {
            service
                .run(generation, tracks, start, stream, keep_playing, notify)
                .await;
        }));
        result
    }

    async fn run(
        self: Arc<Self>,
        generation: u64,
        tracks: Vec<Track>,
        mut start: f64,
        mut stream: Option<Stream>,
        mut keep_playing: bool,
        notify: oneshot::Sender<Result<Track, String>>,
    ) {
        let mut notify = Some(notify);
        self.emit_state().await;
        let mut pending: VecDeque<_> = tracks.into();
        let mut last = String::new();
        let mut retries = 0;
        let mut preplay_skips = 0usize;
        let mut force_tv = false;
        let mut finished_selection = None;
        while let Some(track) = pending.pop_front() {
            let reused_stream = stream.is_some();
            let resolved = if let Some(ready) = stream.take() {
                Ok((track.clone(), ready))
            } else {
                self.resolve(track.clone(), force_tv).await
            };
            let (track, source) = match resolved {
                Ok(value) => value,
                Err(e) => {
                    last = e;
                    force_tv = false;
                    if pending.is_empty()
                        && preplay_skips < 5
                        && let Some(next) = self.pop_next_queued(generation)
                    {
                        preplay_skips += 1;
                        pending.push_back(next);
                    }
                    continue;
                }
            };
            if source.duration > 0.0 {
                start = start.min((source.duration - 0.1).max(0.0));
            }
            let client = match self.host.client().await {
                Ok(client) => client,
                Err(e) => {
                    last = e;
                    break;
                }
            };
            let radio = self.radio().await;
            let needs_related = {
                let mut state = self.state.lock().unwrap();
                if state.generation != generation {
                    return;
                }
                let same_track = state.track.id == track.id;
                state.track = track.clone();
                state.stream = Some(source.clone());
                state.pos = start;
                state.phase = if keep_playing {
                    Phase::Buffering
                } else {
                    Phase::Loading
                };
                self.wake.notify_one();
                state.personal |= interesting("", &track);
                if !radio || state.personal {
                    state.related.clear();
                } else if !pending.is_empty() {
                    state.related = pending.clone();
                } else if !same_track {
                    state.related.clear();
                }
                radio
                    && !state.personal
                    && pending.is_empty()
                    && state.queue.len() < 20
                    && state.related.is_empty()
            };
            log::info!(
                "YouTube: аудио {} Opus itag={} {} кбит/с 48 кГц стерео",
                source.client,
                source.itag,
                source.bitrate / 1000
            );
            let repeat_source = source.clone();
            let mut sent = false;
            // The audio task owns aplay. Dropping it on pause or stop closes the pipe and kills the child.
            let outcome = media::play(
                client,
                source,
                start,
                |pos| {
                    let (first, save_bytes) = {
                        let mut state = self.state.lock().unwrap();
                        if state.generation != generation {
                            return;
                        }
                        let first = !sent;
                        sent = true;
                        state.phase = Phase::Playing;
                        state.pos = pos;
                        let save_bytes = if state
                            .saved
                            .is_none_or(|when| when.elapsed() >= Duration::from_secs(30))
                        {
                            self.record_progress(&mut state, false)
                        } else {
                            None
                        };
                        (first, save_bytes)
                    };
                    if let Some(bytes) = save_bytes {
                        self.persist_progress_bg(bytes);
                    }
                    if first {
                        self.wake.notify_one();
                        if !self.state.lock().unwrap().liked_loaded {
                            let service = self.clone();
                            tokio::spawn(async move {
                                service.ensure_liked_loaded().await;
                            });
                        }
                        if needs_related {
                            let service = self.clone();
                            let seed = track.id.clone();
                            tokio::spawn(async move {
                                let Ok(items) = service.related(&seed).await else {
                                    return;
                                };
                                let mut state = service.state.lock().unwrap();
                                if state.generation == generation
                                    && state.track.id == seed
                                    && state.playing()
                                {
                                    state.related = items.into();
                                }
                            });
                        }
                        if let Some(notify) = notify.take() {
                            let _ = notify.send(Ok(track.clone()));
                        }
                    }
                },
                || f32::from_bits(self.gain.load(Ordering::Relaxed)),
            )
            .await;
            match outcome {
                Err(error) => {
                    last = error.to_string();
                    log::warn!("YouTube: воспроизведение {last}");
                    // CDNs sometimes rotate a URL or reject ANDROID_VR at ~60s. Resume via TVHTML5 at the last audible position.
                    if (sent || reused_stream)
                        && retries < 3
                        && (last.contains("(403") || last.contains("оборвалось"))
                    {
                        retries += 1;
                        if sent {
                            start = self.state.lock().unwrap().pos;
                        }
                        if repeat_source.client == "ANDROID_VR" {
                            force_tv = true;
                        } else if last.contains("(403") {
                            self.remote.invalidate_player_cache();
                        }
                        keep_playing = true;
                        pending.push_front(track.clone());
                        continue;
                    }
                    if sent {
                        break;
                    }
                    force_tv = false;
                    if pending.is_empty()
                        && preplay_skips < 5
                        && let Some(next) = self.pop_next_queued(generation)
                    {
                        preplay_skips += 1;
                        pending.push_back(next);
                    }
                    continue;
                }
                Ok(()) => {
                    last.clear();
                    retries = 0;
                    preplay_skips = 0;
                    start = 0.0;
                    force_tv = false;
                    keep_playing = true;
                }
            }
            let (save_bytes, repeat_now) = {
                let mut state = self.state.lock().unwrap();
                if state.generation != generation {
                    return;
                }
                if let Some(stream) = state.stream.as_ref().filter(|st| st.duration > 0.0) {
                    state.pos = stream.duration;
                }
                (self.record_progress(&mut state, true), state.repeat)
            };
            if let Some(bytes) = save_bytes {
                self.persist_progress_sync(bytes).await;
            }
            if repeat_now {
                if repeat_source.ready.elapsed() < Duration::from_secs(1800) {
                    stream = Some(repeat_source);
                }
                pending.push_front(track);
                continue;
            }
            let radio = self.radio().await;
            let (next, related_end, candidates) = {
                let mut state = self.state.lock().unwrap();
                if state.generation != generation {
                    return;
                }
                if let Some(next) = state.queue.pop_front() {
                    (Some(next), false, VecDeque::new())
                } else if state.personal {
                    (None, true, VecDeque::new())
                } else if radio {
                    (
                        pending.pop_front(),
                        false,
                        std::mem::take(&mut state.related),
                    )
                } else {
                    (None, false, VecDeque::new())
                }
            };
            if let Some(next) = next {
                pending.push_front(next);
                continue;
            }
            if related_end {
                finished_selection = Some(track.id.clone());
            } else if radio {
                if !candidates.is_empty() {
                    pending = candidates;
                    continue;
                }
                match self.related(&track.id).await {
                    Ok(related) if !related.is_empty() => {
                        pending = related.into();
                        continue;
                    }
                    _ => {}
                }
            }
            break;
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            state.phase = Phase::Idle;
            state.task.take();
            state.related.clear();
            state.error = last.clone();
        }
        self.wake.notify_one();
        if let Some(notify) = notify {
            let _ = notify.send(Err(if last.is_empty() {
                "YouTube ничего не нашёл".into()
            } else {
                last
            }));
        }
        self.emit_state().await;
        if let Some(id) = finished_selection {
            self.event("music.related_end", id.into()).await;
        }
    }

    async fn related(&self, id: &str) -> Result<Vec<Track>, String> {
        let auth = self.account.web_headers();
        let tracks = match self.remote.related(id, auth.clone()).await {
            Ok(tracks) => tracks,
            Err(_) if !auth.is_empty() => self.remote.related(id, HeaderMap::new()).await?,
            Err(error) => return Err(error),
        };
        let state = self.state.lock().unwrap();
        Ok(tracks
            .into_iter()
            .filter(|t| !state.history.contains_key(&t.id))
            .take(12)
            .collect())
    }

    fn pop_next_queued(&self, generation: u64) -> Option<Track> {
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            state.queue.pop_front()
        } else {
            None
        }
    }

    async fn launch_query(self: &Arc<Self>, op: &str, args: &Value) -> Result<Value, String> {
        let lock = self.request.lock().await;
        let query = text(&args["query"]);
        let (tracks, start) = match op {
            "play" => {
                if let Some(pid) = playlist_id_from_url(&query) {
                    drop(lock);
                    let res = self.play_playlist_op(&json!({"id": pid})).await?;
                    return Ok(res["track"].clone());
                }
                let tracks = self.search(&query, false).await?;
                self.state.lock().unwrap().personal = false;
                (tracks, 0.0)
            }
            "play_id" => {
                let id = text(&args["id"]);
                let id = video_id(&id)
                    .or_else(|| valid_id(&id).then_some(id))
                    .ok_or("Неверный ID видео YouTube")?;
                let mut track = Track {
                    id: id.clone(),
                    title: text(&args["title"]).trim().to_string(),
                    author: text(&args["author"]).trim().to_string(),
                };
                if track.title == track.id {
                    track.title.clear();
                }
                if (needs_title(&track) || track.author.is_empty())
                    && let Some(known) = self.lookup_known_track(&id).await
                {
                    if needs_title(&track) && !needs_title(&known) {
                        track.title = known.title;
                    }
                    if track.author.is_empty() && !known.author.is_empty() {
                        track.author = known.author;
                    }
                }
                self.state.lock().unwrap().personal = false;
                (vec![track], 0.0)
            }
            "next" => {
                let radio = self.radio().await;
                let (queued, seed) = {
                    let mut state = self.state.lock().unwrap();
                    let next = state.queue.pop_front().or_else(|| {
                        if radio {
                            state.related.pop_front()
                        } else {
                            None
                        }
                    });
                    (next, state.track.id.clone())
                };
                let tracks = if let Some(next) = queued {
                    vec![next]
                } else if valid_id(&seed) && radio {
                    self.related(&seed).await?
                } else {
                    return Err("В очереди ничего нет".into());
                };
                (tracks, 0.0)
            }
            "resume_saved" => {
                let query = query.to_lowercase();
                let state = self.state.lock().unwrap();
                let entry = state
                    .history
                    .values()
                    .filter(|entry| {
                        !entry.complete
                            && entry.pos > 0.0
                            && (query.is_empty()
                                || entry.track.title.to_lowercase().contains(&query)
                                || entry.track.author.to_lowercase().contains(&query))
                    })
                    .max_by_key(|entry| &entry.updated)
                    .ok_or("Сохранённая позиция не найдена")?;
                (vec![entry.track.clone()], entry.pos)
            }
            _ => return Err("Неизвестная команда YouTube".into()),
        };
        if tracks.is_empty() {
            return Err("YouTube ничего не нашёл".into());
        }
        let (_, generation) = self.halt(false).await;
        let finished = self.launch(
            generation,
            tracks.into_iter().take(12).collect(),
            start,
            None,
            false,
        );
        drop(lock);
        let track = finished.await.map_err(|_| "Воспроизведение отменено")??;
        if op == "resume_saved" {
            Ok(format!("Продолжаю «{}» с сохранённого места.", track.title).into())
        } else {
            Ok(json!(track))
        }
    }

    async fn change_position(self: &Arc<Self>, op: &str, args: &Value) -> Result<Value, String> {
        let lock = self.request.lock().await;
        let (track, pos, stream, paused) = {
            let state = self.state.lock().unwrap();
            if state.phase == Phase::Idle {
                return Err("Сейчас ничего не играет".into());
            }
            if op == "resume" && state.phase != Phase::Paused {
                return Ok(Value::Null);
            }
            let delta = args["delta"].as_f64().unwrap_or(0.0);
            let position = if args["abs"] == true {
                delta
            } else {
                state.pos + delta
            };
            let max = if state.dur() > 0.0 {
                (state.dur() - 0.1).max(0.0)
            } else {
                f64::MAX
            };
            (
                state.track.clone(),
                position.clamp(0.0, max),
                state.stream.clone(),
                state.phase == Phase::Paused,
            )
        };
        if paused && op == "seek" {
            let (_, generation) = self.halt(true).await;
            let save_bytes = {
                let mut state = self.state.lock().unwrap();
                if state.generation == generation {
                    state.pos = pos;
                    self.record_progress(&mut state, false)
                } else {
                    None
                }
            };
            if let Some(bytes) = save_bytes {
                self.persist_progress_sync(bytes).await;
            }
        } else {
            let generation = self.halt_transition(pos).await;
            let stream = stream.filter(|st| st.ready.elapsed() < Duration::from_secs(1800));
            let started = self.launch(generation, vec![track], pos, stream, true);
            drop(lock);
            match tokio::time::timeout(Duration::from_millis(1200), started).await {
                Ok(Ok(Ok(_))) | Ok(Err(_)) | Err(_) => {}
                Ok(Ok(Err(err))) => {
                    if self.state.lock().unwrap().generation == generation {
                        return Err(err);
                    }
                }
            }
        }
        Ok(Value::Null)
    }

    async fn proposal(self: &Arc<Self>, op: &str, args: &Value) -> Result<Value, String> {
        let lock = self.request.lock().await;
        if op == "respond" {
            let reply = text(&args["text"]).to_lowercase();
            let words: Vec<_> = reply
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .collect();
            let clean = words.join(" ");
            let answer = {
                let mut state = self.state.lock().unwrap();
                if !state.proposals() || words.len() > 8 || words.is_empty() {
                    return Ok(json!({"ok":false}));
                }
                if ["отмена", "отбой", "забей", "не включай", "не сейчас"]
                    .iter()
                    .any(|w| clean.contains(w))
                {
                    state.proposal = None;
                    return Ok(json!({"ok":true,"answer":"Ладно, оставляю на паузе."}));
                }
                if ["нет", "дальше", "следующий", "другое", "неа"]
                    .iter()
                    .any(|w| words.contains(w))
                    && !["почему нет", "почему бы и нет"].contains(&clean.as_str())
                {
                    let (until, items) = state.proposal.as_mut().unwrap();
                    items.pop_front();
                    *until = Instant::now() + Duration::from_secs(180);
                    let answer = items
                        .front()
                        .map(describe)
                        .unwrap_or_else(|| "Варианты закончились. Назови другую тему.".into());
                    return Ok(json!({"ok":true,"answer":answer}));
                }
                if (words.contains(&"не") && !clean.contains("не против"))
                    || !words.iter().any(|w| {
                        [
                            "да",
                            "давай",
                            "ага",
                            "включай",
                            "погнали",
                            "окей",
                            "согласен",
                            "можно",
                        ]
                        .contains(w)
                    }) && !["не против", "почему нет", "почему бы и нет"]
                        .contains(&clean.as_str())
                {
                    return Ok(json!({"ok":false}));
                }
                state.proposal.take().unwrap().1.pop_front().unwrap()
            };
            let (_, generation) = self.halt(false).await;
            let started = self.launch(generation, vec![answer], 0.0, None, false);
            drop(lock);
            let track = started.await.map_err(|_| "Воспроизведение отменено")??;
            return Ok(json!({"ok":true,"answer":format!("Включил «{}».",track.title)}));
        }
        let generation = self.state.lock().unwrap().generation;
        let items = if op == "related" {
            let expected = text(&args["expected"]);
            let id = {
                let state = self.state.lock().unwrap();
                if !expected.is_empty() && (state.track.id != expected || state.phase != Phase::Idle) {
                    return Ok(Value::Null);
                }
                state.track.id.clone()
            };
            if !valid_id(&id) {
                return Err("Нет текущего видео YouTube".into());
            }
            self.related(&id).await?
        } else {
            let query = text(&args["query"]);
            let headers = self.account.headers(false).await?;
            let feed = self
                .remote
                .recommendations(headers)
                .await
                .unwrap_or_default();
            let (mut candidates, unplayed) = {
                let state = self.state.lock().unwrap();
                let unplayed: Vec<_> = feed
                    .into_iter()
                    .filter(|track| !state.history.contains_key(&track.id))
                    .collect();
                let matched: Vec<_> = unplayed
                    .iter()
                    .filter(|track| interesting(&query, track))
                    .cloned()
                    .collect();
                (matched, unplayed)
            };
            if candidates.is_empty() && !unplayed.is_empty() && genre_terms(&query).is_none() {
                candidates = unplayed;
            }
            if candidates.is_empty() {
                candidates = self.search(&query, true).await?;
            }
            candidates
        };
        let Some(first) = items.first() else {
            return Err("Подходящих видео YouTube нет".into());
        };
        let answer = describe(first);
        if self.state.lock().unwrap().generation != generation {
            return Ok(Value::Null);
        }
        let (_, generation) = self.halt(true).await;
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err("Подбор отменён".into());
        }
        state.personal = true;
        state.proposal = Some((Instant::now() + Duration::from_secs(180), items.into()));
        Ok(answer.into())
    }

    pub async fn music(self: &Arc<Self>, input: Value) -> Result<Value, String> {
        self.init().await?;
        let op = input["op"].as_str().unwrap_or_default();
        let result = self.command(op, &input["args"]).await;
        let state = self.state.lock().unwrap().current();
        match result {
            Ok(value) => Ok(json!({"value":value,"state":state})),
            Err(error) => {
                Ok(json!({"error":error,"login":remote::login_required(&error),"state":state}))
            }
        }
    }

    async fn command(self: &Arc<Self>, op: &str, args: &Value) -> Result<Value, String> {
        match op {
            "state" => Ok(Value::Null),
            "snapshot" => {
                let radio = self.radio().await;
                Ok(self.state.lock().unwrap().snapshot(radio))
            }
            "play" | "play_id" | "next" | "resume_saved" => self.launch_query(op, args).await,
            "library" => self.load_library(args["refresh"] == true).await,
            "playlist_items" => {
                let id = text(&args["id"]).trim().to_string();
                let tracks = self
                    .load_playlist_items(&id, args["refresh"] == true)
                    .await?;
                Ok(json!({"id": id, "tracks": tracks}))
            }
            "play_liked" => self.play_liked_op(args).await,
            "play_playlist" => self.play_playlist_op(args).await,
            "enqueue" => {
                let _lock = self.request.lock().await;
                let mut track = self
                    .search(&text(&args["query"]), false)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or("YouTube ничего не нашёл")?;
                let hint_title = text(&args["title"]).trim().to_string();
                let hint_author = text(&args["author"]).trim().to_string();
                if needs_title(&track) && !hint_title.is_empty() && hint_title != track.id {
                    track.title = hint_title;
                }
                if track.author.is_empty() && !hint_author.is_empty() {
                    track.author = hint_author;
                }
                if (needs_title(&track) || track.author.is_empty())
                    && let Some(known) = self.lookup_known_track(&track.id).await
                {
                    if needs_title(&track) && !needs_title(&known) {
                        track.title = known.title;
                    }
                    if track.author.is_empty() && !known.author.is_empty() {
                        track.author = known.author;
                    }
                }
                if (needs_title(&track) || track.author.is_empty())
                    && let Ok(meta) = self.remote.oembed(&track.id).await
                {
                    if needs_title(&track) && !needs_title(&meta) {
                        track.title = meta.title;
                    }
                    if track.author.is_empty() && !meta.author.is_empty() {
                        track.author = meta.author;
                    }
                }
                if track.title.is_empty() {
                    track.title = track.id.clone();
                }
                let mut state = self.state.lock().unwrap();
                if state.queue.len() >= 100 {
                    return Err("Очередь заполнена".into());
                }
                state.queue.push_back(track.clone());
                Ok(json!(track))
            }
            "like" => self.like_op(args).await,
            "search" => self.search_op(args).await,
            "unqueue" => {
                if let Some(index) = args["index"].as_u64().and_then(|v| usize::try_from(v).ok()) {
                    self.state.lock().unwrap().queue.remove(index);
                }
                Ok(Value::Null)
            }
            "stop" | "pause" => Ok(self.halt(op == "pause").await.0.into()),
            "resume" | "seek" => self.change_position(op, args).await,
            "propose" | "related" | "respond" => self.proposal(op, args).await,
            "radio_reset" => {
                let mut state = self.state.lock().unwrap();
                state.queue.clear();
                state.related.clear();
                state.personal = false;
                Ok(Value::Null)
            }
            "repeat" => {
                let mut state = self.state.lock().unwrap();
                state.repeat = args["on"].as_bool().unwrap_or(!state.repeat);
                self.wake.notify_one();
                Ok(state.repeat.into())
            }
            "warm" => {
                self.remote.warm().await?;
                Ok(Value::Null)
            }
            "cookies_set" => {
                self.account
                    .set_cookies(&self.remote, args["cookies"].as_str().unwrap_or_default())
                    .await?;
                self.account.clear_profile_cache().await;
                self.library_cache.lock().await.clear();
                let mut state = self.state.lock().unwrap();
                state.liked_ids.clear();
                state.liked_loaded = false;
                self.wake.notify_one();
                Ok(Value::Null)
            }
            "cookies_clear" => {
                self.account.clear_cookies()?;
                self.account.clear_profile_cache().await;
                self.library_cache.lock().await.clear();
                let mut state = self.state.lock().unwrap();
                state.liked_ids.clear();
                state.liked_loaded = false;
                self.wake.notify_one();
                Ok(Value::Null)
            }
            "account" => {
                Ok(json!({"set":self.account.has_cookies(),"account":self.account.state(&self.remote).await?}))
            }
            "account_begin" => self.account.begin(&self.remote).await,
            "account_logout" => {
                self.account.logout().await?;
                let _ = self.account.clear_cookies();
                self.library_cache.lock().await.clear();
                let mut state = self.state.lock().unwrap();
                state.liked_ids.clear();
                state.liked_loaded = false;
                self.wake.notify_one();
                Ok(Value::Null)
            }
            _ => Err("Неизвестная команда плеера".into()),
        }
    }

    async fn auth_headers(&self) -> Result<HeaderMap, String> {
        if !self.account.is_signed_in().await {
            return Ok(HeaderMap::new());
        }
        self.account.headers(false).await
    }

    async fn load_library(&self, refresh: bool) -> Result<Value, String> {
        let headers = self.auth_headers().await?;
        if headers.is_empty() {
            return Ok(json!({"signed_in": false, "liked": [], "playlists": []}));
        }
        let (cached_liked, cached_playlists) = if refresh {
            (None, None)
        } else {
            let cache = self.library_cache.lock().await;
            (
                cache
                    .liked
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < LIBRARY_CACHE_TTL)
                    .map(|(_, v)| v.clone()),
                cache
                    .playlists
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < LIBRARY_CACHE_TTL)
                    .map(|(_, v)| v.clone()),
            )
        };

        let liked_fut = async {
            match cached_liked {
                Some(v) => Ok(v),
                None => self.remote.liked_tracks(headers.clone(), 100).await,
            }
        };
        let playlists_fut = async {
            match cached_playlists {
                Some(v) => Ok(v),
                None => self.remote.playlists(headers.clone()).await,
            }
        };
        let (liked_res, playlists_res) = tokio::join!(liked_fut, playlists_fut);

        if let Err(ref e) = liked_res
            && e.contains("войти заново")
        {
            return Err(e.clone());
        }
        if let Err(ref e) = playlists_res
            && e.contains("войти заново")
        {
            return Err(e.clone());
        }

        let mut error = String::new();
        let liked = match liked_res {
            Ok(v) => {
                {
                    let mut st = self.state.lock().unwrap();
                    st.liked_ids = v.iter().map(|t| t.id.clone()).collect();
                    st.liked_loaded = true;
                    self.wake.notify_one();
                }
                let mut cache = self.library_cache.lock().await;
                cache.liked = Some((Instant::now(), v.clone()));
                v
            }
            Err(e) => {
                error = e;
                Vec::new()
            }
        };
        let playlists = match playlists_res {
            Ok(v) => {
                let mut cache = self.library_cache.lock().await;
                cache.playlists = Some((Instant::now(), v.clone()));
                v
            }
            Err(e) => {
                if error.is_empty() {
                    error = e;
                }
                Vec::new()
            }
        };
        if !error.is_empty() && liked.is_empty() && playlists.is_empty() {
            return Err(error);
        }
        let mut out = json!({
            "signed_in": true,
            "liked": liked,
            "playlists": playlists
        });
        if !error.is_empty() {
            out["error"] = error.into();
        }
        Ok(out)
    }

    async fn load_playlist_items(&self, id: &str, refresh: bool) -> Result<Vec<Track>, String> {
        if !valid_playlist_id(id) {
            return Err("Неверный ID плейлиста YouTube".into());
        }
        if !refresh {
            let cache = self.library_cache.lock().await;
            if let Some((at, tracks)) = cache.items.get(id)
                && at.elapsed() < LIBRARY_CACHE_TTL
            {
                return Ok(tracks.clone());
            }
        }
        let headers = self.auth_headers().await.unwrap_or_default();
        let tracks = self.remote.playlist_tracks(id, headers, 100).await?;
        let mut cache = self.library_cache.lock().await;
        if cache.items.len() >= 24 {
            cache.items.clear();
        }
        cache
            .items
            .insert(id.to_string(), (Instant::now(), tracks.clone()));
        Ok(tracks)
    }

    async fn play_liked_op(self: &Arc<Self>, args: &Value) -> Result<Value, String> {
        let lock = self.request.lock().await;
        let headers = self.auth_headers().await?;
        if headers.is_empty() {
            return Err("Чтобы слушать лайки, войдите в аккаунт YouTube в настройках.".into());
        }
        let cached = {
            let cache = self.library_cache.lock().await;
            cache
                .liked
                .as_ref()
                .filter(|(at, _)| at.elapsed() < LIBRARY_CACHE_TTL)
                .map(|(_, v)| v.clone())
        };
        let tracks = match cached {
            Some(v) => v,
            None => {
                let fresh = self.remote.liked_tracks(headers, 100).await?;
                {
                    let mut st = self.state.lock().unwrap();
                    st.liked_ids = fresh.iter().map(|t| t.id.clone()).collect();
                    st.liked_loaded = true;
                    self.wake.notify_one();
                }
                self.library_cache.lock().await.liked = Some((Instant::now(), fresh.clone()));
                fresh
            }
        };
        if tracks.is_empty() {
            return Err("В аккаунте YouTube пока нет понравившихся видео".into());
        }
        let index = args["index"].as_u64().unwrap_or(0) as usize;
        let shuffle = args["shuffle"] == true;
        let queue = args["queue"] == true;
        let (started, count) = self.play_track_list(lock, tracks, index, shuffle, queue).await?;
        Ok(json!({"track": started, "count": count}))
    }

    async fn play_playlist_op(self: &Arc<Self>, args: &Value) -> Result<Value, String> {
        let lock = self.request.lock().await;
        let raw_id = text(&args["id"]).trim().to_string();
        let raw_query = text(&args["query"]).trim().to_string();
        let index = args["index"].as_u64().unwrap_or(0) as usize;
        let shuffle = args["shuffle"] == true;
        let queue = args["queue"] == true;

        let id = if !raw_id.is_empty() {
            raw_id
        } else if let Some(parsed) = playlist_id_from_url(&raw_query) {
            parsed
        } else {
            String::new()
        };

        if !id.is_empty() {
            let tracks = self.load_playlist_items(&id, false).await?;
            if tracks.is_empty() {
                return Err("Плейлист пуст или недоступен".into());
            }
            let title = {
                let cache = self.library_cache.lock().await;
                cache
                    .playlists
                    .as_ref()
                    .and_then(|(_, pls)| pls.iter().find(|p| p.id == id))
                    .map(|p| p.title.clone())
                    .unwrap_or_default()
            };
            let (started, count) = self
                .play_track_list(lock, tracks, index, shuffle, queue)
                .await?;
            return Ok(json!({"track": started, "count": count, "playlist": title}));
        }

        if raw_query.is_empty() {
            return Err("Укажите название плейлиста".into());
        }

        let headers = self.auth_headers().await.unwrap_or_default();
        if !headers.is_empty() {
            let saved = {
                let cache = self.library_cache.lock().await;
                cache
                    .playlists
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < LIBRARY_CACHE_TTL)
                    .map(|(_, v)| v.clone())
            };
            let saved = match saved {
                Some(v) => v,
                None => match self.remote.playlists(headers.clone()).await {
                    Ok(v) => {
                        self.library_cache.lock().await.playlists =
                            Some((Instant::now(), v.clone()));
                        v
                    }
                    Err(_) => Vec::new(),
                },
            };
            if let Some(matched) = match_saved_playlist(&saved, &raw_query).cloned() {
                if let Ok(tracks) = self.load_playlist_items(&matched.id, false).await
                    && !tracks.is_empty()
                {
                    let (started, count) = self
                        .play_track_list(lock, tracks, index, shuffle, queue)
                        .await?;
                    return Ok(json!({
                        "track": started,
                        "count": count,
                        "playlist": matched.title
                    }));
                }
            }
        }

        if let Ok(found) = self.remote.search_playlists(&raw_query, HeaderMap::new()).await {
            for pl in found.into_iter().take(3) {
                if let Ok(tracks) = self.load_playlist_items(&pl.id, false).await
                    && !tracks.is_empty()
                {
                    let (started, count) = self
                        .play_track_list(lock, tracks, index, shuffle, queue)
                        .await?;
                    return Ok(json!({
                        "track": started,
                        "count": count,
                        "playlist": pl.title
                    }));
                }
            }
        }

        let tracks = self.search(&format!("{raw_query} плейлист"), false).await?;
        if tracks.is_empty() {
            return Err("Плейлист не найден на YouTube".into());
        }
        let (started, count) = self
            .play_track_list(lock, tracks, index, shuffle, queue)
            .await?;
        Ok(json!({"track": started, "count": count, "playlist": ""}))
    }

    async fn play_track_list(
        self: &Arc<Self>,
        lock: tokio::sync::MutexGuard<'_, ()>,
        mut tracks: Vec<Track>,
        start_index: usize,
        shuffle: bool,
        enqueue_only: bool,
    ) -> Result<(Track, usize), String> {
        if tracks.is_empty() {
            return Err("В списке нет доступных треков".into());
        }
        if shuffle {
            shuffle_tracks(&mut tracks);
        } else if start_index > 0 {
            if start_index >= tracks.len() {
                return Err("Трек не найден в списке".into());
            }
            tracks.drain(..start_index);
        }
        tracks.truncate(100);
        if enqueue_only {
            let first = tracks[0].clone();
            let mut state = self.state.lock().unwrap();
            let avail = 100usize.saturating_sub(state.queue.len());
            if avail == 0 {
                return Err("Очередь заполнена".into());
            }
            let added = tracks.len().min(avail);
            for t in tracks.into_iter().take(added) {
                state.queue.push_back(t);
            }
            return Ok((first, added));
        }
        let total = tracks.len();
        let first = tracks.remove(0);
        let (_, generation) = self.halt(false).await;
        {
            let mut state = self.state.lock().unwrap();
            if state.generation == generation {
                state.personal = false;
                state.related.clear();
                state.queue = tracks.into();
            }
        }
        let finished = self.launch(generation, vec![first], 0.0, None, false);
        drop(lock);
        let started = finished.await.map_err(|_| "Воспроизведение отменено")??;
        Ok((started, total))
    }

    async fn ensure_liked_loaded(&self) {
        if self.state.lock().unwrap().liked_loaded {
            return;
        }
        let Ok(headers) = self.auth_headers().await else {
            return;
        };
        if headers.is_empty() {
            return;
        }
        let cached = {
            let cache = self.library_cache.lock().await;
            cache
                .liked
                .as_ref()
                .filter(|(at, _)| at.elapsed() < LIBRARY_CACHE_TTL)
                .map(|(_, v)| v.clone())
        };
        let tracks = match cached {
            Some(v) => Ok(v),
            None => self.remote.liked_tracks(headers, 100).await,
        };
        if let Ok(tracks) = tracks {
            {
                let mut st = self.state.lock().unwrap();
                st.liked_ids = tracks.iter().map(|t| t.id.clone()).collect();
                st.liked_loaded = true;
                self.wake.notify_one();
            }
            self.library_cache.lock().await.liked = Some((Instant::now(), tracks));
        }
    }

    async fn like_op(&self, args: &Value) -> Result<Value, String> {
        let raw_id = text(&args["id"]).trim().to_string();
        let id = if !raw_id.is_empty() {
            video_id(&raw_id)
                .or_else(|| valid_id(&raw_id).then_some(raw_id))
                .ok_or("Неверный ID видео YouTube")?
        } else {
            let st = self.state.lock().unwrap();
            if st.phase != Phase::Idle && valid_id(&st.track.id) {
                st.track.id.clone()
            } else {
                return Err("Сейчас ничего не играет".into());
            }
        };
        let headers = self.auth_headers().await?;
        if headers.is_empty() {
            return Err("Войдите в аккаунт YouTube в настройках".into());
        }
        if args["liked"].as_bool().is_none() && !self.state.lock().unwrap().liked_loaded {
            self.ensure_liked_loaded().await;
        }
        let liked = args["liked"].as_bool().unwrap_or_else(|| {
            !self.state.lock().unwrap().liked_ids.contains(&id)
        });
        self.remote.set_like(&id, liked, headers).await?;

        let mut track = Track {
            id: id.clone(),
            title: text(&args["title"]).trim().to_string(),
            author: text(&args["author"]).trim().to_string(),
        };
        if track.title == track.id {
            track.title.clear();
        }
        if (needs_title(&track) || track.author.is_empty())
            && let Some(known) = self.lookup_known_track(&id).await
        {
            if needs_title(&track) && !needs_title(&known) {
                track.title = known.title;
            }
            if track.author.is_empty() && !known.author.is_empty() {
                track.author = known.author;
            }
        }
        if (needs_title(&track) || track.author.is_empty())
            && let Ok(meta) = self.remote.oembed(&id).await
        {
            if needs_title(&track) && !needs_title(&meta) {
                track.title = meta.title;
            }
            if track.author.is_empty() && !meta.author.is_empty() {
                track.author = meta.author;
            }
        }
        if track.title.is_empty() {
            track.title = id.clone();
        }

        {
            let mut st = self.state.lock().unwrap();
            if liked {
                st.liked_ids.insert(id.clone());
            } else {
                st.liked_ids.remove(&id);
            }
            st.liked_loaded = true;
            self.wake.notify_one();
        }
        {
            let mut cache = self.library_cache.lock().await;
            if let Some((at, list)) = cache.liked.as_mut() {
                list.retain(|t| t.id != id);
                if liked {
                    list.insert(0, track.clone());
                }
                *at = Instant::now();
            }
        }
        Ok(json!({"id": id, "liked": liked, "track": track}))
    }

    async fn search_op(&self, args: &Value) -> Result<Value, String> {
        let query = text(&args["query"]).trim().to_string();
        if query.is_empty() {
            return Err("Введите запрос для поиска".into());
        }
        if query.len() > 2048 {
            return Err("Слишком длинный запрос".into());
        }
        let kind = match text(&args["type"]).trim() {
            "playlist" => "playlist",
            "all" => "all",
            _ => "video",
        };
        if let Some(id) = video_id(&query).or_else(|| valid_id(&query).then(|| query.clone())) {
            let mut track = self
                .lookup_known_track(&id)
                .await
                .unwrap_or_else(|| Track::from_id(id.clone()));
            if (needs_title(&track) || track.author.is_empty())
                && let Ok(meta) = self.remote.oembed(&id).await
            {
                if needs_title(&track) && !needs_title(&meta) {
                    track.title = meta.title;
                }
                if track.author.is_empty() && !meta.author.is_empty() {
                    track.author = meta.author;
                }
            }
            if track.title.is_empty() {
                track.title = id.clone();
            }
            return Ok(json!({
                "query": query,
                "items": [SearchItem {
                    kind: "video".into(),
                    id: id.clone(),
                    title: track.title,
                    author: track.author,
                    thumb: format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
                    ..Default::default()
                }]
            }));
        }
        if let Some(pid) = playlist_id_from_url(&query) {
            let title = self
                .library_cache
                .lock()
                .await
                .playlists
                .as_ref()
                .and_then(|(_, list)| list.iter().find(|p| p.id == pid).map(|p| p.title.clone()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Плейлист YouTube".into());
            return Ok(json!({
                "query": query,
                "items": [SearchItem {
                    kind: "playlist".into(),
                    id: pid,
                    title,
                    ..Default::default()
                }]
            }));
        }
        let filter = SearchFilter {
            kind: kind.to_string(),
            sort: text(&args["sort"]).trim().to_string(),
            date: text(&args["date"]).trim().to_string(),
            duration: text(&args["duration"]).trim().to_string(),
            live: args["live"] == true,
        };
        let personal = self.account.web_headers();
        let items = if !personal.is_empty() {
            match self.remote.search_filtered(&query, &filter, personal).await {
                Ok(found) if !found.is_empty() => found,
                _ => self
                    .remote
                    .search_filtered(&query, &filter, HeaderMap::new())
                    .await?,
            }
        } else {
            self.remote
                .search_filtered(&query, &filter, HeaderMap::new())
                .await?
        };
        Ok(json!({"query": query, "items": items}))
    }
}

fn needs_title(track: &Track) -> bool {
    let t = track.title.trim();
    t.is_empty() || t == track.id
}

fn shuffle_tracks(tracks: &mut [Track]) {
    if tracks.len() < 2 {
        return;
    }
    for i in (1..tracks.len()).rev() {
        let j = match random::<8>() {
            Ok(bytes) => (u64::from_le_bytes(bytes) as usize) % (i + 1),
            Err(_) => break,
        };
        tracks.swap(i, j);
    }
}

pub(crate) fn match_saved_playlist<'a>(
    playlists: &'a [Playlist],
    query: &str,
) -> Option<&'a Playlist> {
    let q = normal(query);
    if q.is_empty() {
        return None;
    }
    if let Some(exact) = playlists.iter().find(|p| normal(&p.title) == q) {
        return Some(exact);
    }
    let q_words: Vec<&str> = q.split_whitespace().collect();
    let mut best: Option<(usize, &Playlist)> = None;
    for p in playlists {
        let title = normal(&p.title);
        if title.is_empty() {
            continue;
        }
        let matched = title.contains(&q)
            || (!q_words.is_empty()
                && q_words.iter().all(|w| {
                    title
                        .split_whitespace()
                        .any(|tw| tw == *w || (w.chars().count() >= 3 && tw.starts_with(w)))
                }));
        if matched {
            let diff = title.len().abs_diff(q.len());
            if best.is_none_or(|(best_diff, _)| diff < best_diff) {
                best = Some((diff, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn genre_terms(query: &str) -> Option<&'static [&'static str]> {
    let query = query.to_lowercase();
    for (want, terms) in [
        (
            &["стендап", "standup"][..],
            &["стендап", "standup", "stand up"][..],
        ),
        (&["апвоут", "upvote"][..], &["апвоут", "upvote"][..]),
        (
            &["музык", "песн", "трек", "рок"][..],
            &[
                "музык",
                "music",
                "альбом",
                "playlist",
                "концерт",
                "official audio",
            ][..],
        ),
    ] {
        if want.iter().any(|word| query.contains(word)) {
            return Some(terms);
        }
    }
    None
}

fn interesting(query: &str, track: &Track) -> bool {
    let title = format!("{} {}", track.title, track.author).to_lowercase();
    if let Some(terms) = genre_terms(query) {
        return terms.iter().any(|term| title.contains(term));
    }
    [
        "подкаст",
        "podcast",
        "интервью",
        "лекция",
        "история",
        "аудиокниг",
        "стендап",
        "разбор",
        "апвоут",
    ]
    .iter()
    .any(|word| title.contains(word))
}

fn describe(track: &Track) -> String {
    format!("Нашёл на YouTube «{}». Послушаем?", track.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "live YouTube probe"]
    async fn public_audio() {
        let host = Arc::new(Host::new(
            Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            std::env::temp_dir(),
        ));
        let remote = Remote::new(host.clone());
        remote.warm().await.unwrap();
        let (_, source) = remote
            .resolve("jNQXAC9IVRw", HeaderMap::new(), false)
            .await
            .unwrap();
        let mut bytes = 0;
        assert_eq!(source.client, "TVHTML5");
        eprintln!("Source {} itag {}", source.client, source.itag);
        let mut audio = media::Audio::open(host.client().await.unwrap(), source, 0.0, 1.0)
            .await
            .unwrap();
        while bytes < 96000 {
            let Some((_, pcm)) = audio.next(1.0).await.unwrap() else {
                break;
            };
            bytes += pcm.len();
        }
        assert!(bytes >= 96000);
    }

    #[tokio::test]
    async fn isolated_player_extracts_signature_and_rate_functions() {
        let fixture = r#"(function(){
            function makeUrl(url, key, sig) {
                let params = new Map([[key, sig]]);
                params.set("alr", "yes");
                return new Wrapped(params);
            }
            function Wrapped(params) { this.params = params; }
            Wrapped.prototype.set = function(k,v) { this.params.set(k,v); };
            Wrapped.prototype.get = function(k) { return this.params.get(k); };
            Wrapped.prototype.transform = function() {
                for (const k of ["s","n"]) {
                    const v=this.get(k);
                    if (v) this.set(k,v.split("").reverse().join(""));
                }
            };
            var config={signatureTimestamp:12345};
        }).call(this);"#;
        let prepared = Arc::new(player_script::Prepared::new(fixture.to_owned()).unwrap());
        assert_eq!(prepared.timestamp, 12345);
        assert_eq!(
            prepared.decode("abc".into(), "xyz".into()).await.unwrap(),
            ("cba".into(), "zyx".into())
        );
    }

    #[tokio::test]
    async fn repeat_command_toggles_and_persists_in_state() {
        let host = Host::new(
            Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            std::env::temp_dir().join("jarvis-yt-repeat-test"),
        );
        let yt = Arc::new(Youtube::new(host));
        assert_eq!(yt.state.lock().unwrap().current()["repeat"], false);
        assert_eq!(yt.state.lock().unwrap().snapshot(true)["repeat"], false);

        assert_eq!(yt.command("repeat", &json!({"on": true})).await.unwrap(), json!(true));
        assert_eq!(yt.state.lock().unwrap().current()["repeat"], true);
        assert_eq!(yt.state.lock().unwrap().snapshot(false)["repeat"], true);

        // Pausing / halting playback must preserve repeat setting.
        let _ = yt.halt(false).await;
        assert_eq!(yt.state.lock().unwrap().current()["repeat"], true);

        // Omitting `on` toggles repeat off.
        assert_eq!(yt.command("repeat", &json!({})).await.unwrap(), json!(false));
        assert_eq!(yt.state.lock().unwrap().current()["repeat"], false);
    }

    #[tokio::test]
    async fn buffering_transition_keeps_playing_track_and_related() {
        let host = Host::new(
            Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            std::env::temp_dir().join("jarvis-yt-buffering-test"),
        );
        let yt = Arc::new(Youtube::new(host));
        {
            let mut st = yt.state.lock().unwrap();
            st.phase = Phase::Playing;
            st.track = Track {
                id: "dQw4w9WgXcQ".into(),
                title: "Test Track".into(),
                author: "Tester".into(),
            };
            st.stream = Some(Stream {
                url: reqwest::Url::parse("https://rr1---sn-test.googlevideo.com/videoplayback")
                    .unwrap(),
                agent: "test",
                client: "TVHTML5",
                length: 1000,
                duration: 120.0,
                ready: Instant::now(),
                itag: 251,
                bitrate: 160_000,
            });
            st.related.push_back(Track {
                id: "jNQXAC9IVRw".into(),
                title: "Next Related".into(),
                author: "Someone".into(),
            });
        }

        let gen_after = yt.halt_transition(42.0).await;
        assert_eq!(gen_after, 1);

        let cur = yt.state.lock().unwrap().current();
        assert_eq!(cur["playing"], true);
        assert_eq!(cur["active"], true);
        assert_eq!(cur["paused"], false);

        let snap = yt.state.lock().unwrap().snapshot(true);
        assert_eq!(snap["playing"], true);
        assert_eq!(snap["paused"], false);
        assert_eq!(snap["loading"], true);
        assert_eq!(snap["pos"], 42.0);
        assert_eq!(snap["track"]["ID"], "dQw4w9WgXcQ");
        assert_eq!(snap["next"][0]["ID"], "jNQXAC9IVRw");
    }

    #[tokio::test]
    async fn pause_during_buffering_without_stream_stays_paused() {
        let host = Host::new(
            Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            std::env::temp_dir().join("jarvis-yt-pause-buffer-test"),
        );
        let yt = Arc::new(Youtube::new(host));
        {
            let mut st = yt.state.lock().unwrap();
            st.phase = Phase::Buffering;
            st.track = Track {
                id: "dQw4w9WgXcQ".into(),
                title: "Test Track".into(),
                author: "Tester".into(),
            };
            st.pos = 15.0;
            st.stream = None;
        }

        let (was_active, _) = yt.halt(true).await;
        assert!(was_active);
        let cur = yt.state.lock().unwrap().current();
        assert_eq!(cur["paused"], true);
        assert_eq!(cur["active"], true);
        assert_eq!(cur["cur"]["ID"], "dQw4w9WgXcQ");
    }

    #[tokio::test]
    async fn saved_playlist_matching_shuffle_and_signed_out_guards() {
        let pls = vec![
            Playlist {
                id: "PL1".into(),
                title: "Lo-Fi Beats".into(),
                ..Default::default()
            },
            Playlist {
                id: "PL2".into(),
                title: "Музыка в машину".into(),
                ..Default::default()
            },
            Playlist {
                id: "PL3".into(),
                title: "Synthwave 2026".into(),
                ..Default::default()
            },
            Playlist {
                id: "PL4".into(),
                title: "Музыка".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            match_saved_playlist(&pls, "lo-fi beats").map(|p| p.id.as_str()),
            Some("PL1")
        );
        assert_eq!(
            match_saved_playlist(&pls, "lo fi").map(|p| p.id.as_str()),
            Some("PL1")
        );
        assert_eq!(
            match_saved_playlist(&pls, "в машину").map(|p| p.id.as_str()),
            Some("PL2")
        );
        assert_eq!(
            match_saved_playlist(&pls, "synth").map(|p| p.id.as_str()),
            Some("PL3")
        );
        assert_eq!(
            match_saved_playlist(&pls, "музыка").map(|p| p.id.as_str()),
            Some("PL4")
        );
        assert!(match_saved_playlist(&pls, "макробер музыка").is_none());
        assert!(match_saved_playlist(&pls, "джаз").is_none());

        let mut tracks: Vec<Track> = (0..10)
            .map(|i| Track {
                id: format!("id_{i}"),
                title: format!("Title {i}"),
                author: String::new(),
            })
            .collect();
        shuffle_tracks(&mut tracks);
        let mut ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            (0..10).map(|i| format!("id_{i}")).collect::<Vec<_>>()
        );

        let dir = std::env::temp_dir().join("jarvis-yt-lib-guard-test");
        let _ = std::fs::remove_dir_all(&dir);
        let host = Host::new(
            Arc::new(|method, _| {
                Box::pin(async move {
                    if method == "config.get" {
                        Ok(json!({"auth": {"refresh": "", "name": ""}}))
                    } else {
                        Ok(json!({}))
                    }
                })
            }),
            dir,
        );
        let yt = Arc::new(Youtube::new(host));
        let lib = yt.command("library", &json!({})).await.unwrap();
        assert_eq!(lib["signed_in"], false);

        let err = yt.command("play_liked", &json!({})).await.unwrap_err();
        assert!(err.contains("войдите"), "{err}");

        let bad_pl = yt
            .command("playlist_items", &json!({"id": "bad/id"}))
            .await
            .unwrap_err();
        assert!(bad_pl.contains("Неверный ID"), "{bad_pl}");
    }

    #[tokio::test]
    async fn preserves_known_titles_in_play_id_and_tracks_liked_ids_in_snapshot() {
        let dir = std::env::temp_dir().join("jarvis-yt-like-meta-test");
        let _ = std::fs::remove_dir_all(&dir);
        let host = Host::new(
            Arc::new(|method, _| {
                Box::pin(async move {
                    if method == "config.get" {
                        Ok(json!({"auth": {"refresh": "", "name": ""}}))
                    } else {
                        Ok(json!({}))
                    }
                })
            }),
            dir,
        );
        let yt = Arc::new(Youtube::new(host));
        {
            let mut cache = yt.library_cache.lock().await;
            cache.liked = Some((
                Instant::now(),
                vec![Track {
                    id: "s0lMgCAmlgY".into(),
                    title: "ЗОМБИ".into(),
                    author: "Джарахов".into(),
                }],
            ));
            let mut st = yt.state.lock().unwrap();
            st.liked_ids.insert("s0lMgCAmlgY".into());
            st.liked_loaded = true;
            st.phase = Phase::Playing;
            st.track = Track {
                id: "s0lMgCAmlgY".into(),
                title: "ЗОМБИ".into(),
                author: "Джарахов".into(),
            };
        }
        let known = yt.lookup_known_track("s0lMgCAmlgY").await.unwrap();
        assert_eq!(known.title, "ЗОМБИ");
        assert_eq!(known.author, "Джарахов");

        let snap = yt.state.lock().unwrap().snapshot(true);
        assert_eq!(snap["liked"], true);
        assert_eq!(snap["liked_ids"], json!(["s0lMgCAmlgY"]));

        // When recording progress with missing title while history has real title, real title survives.
        {
            let mut st = yt.state.lock().unwrap();
            st.pos = 12.0;
            yt.record_progress(&mut st, false);
            st.track = Track {
                id: "s0lMgCAmlgY".into(),
                title: "s0lMgCAmlgY".into(),
                author: String::new(),
            };
            st.pos = 25.0;
            yt.record_progress(&mut st, false);
            let saved = &st.history["s0lMgCAmlgY"];
            assert_eq!(saved.track.title, "ЗОМБИ");
            assert_eq!(saved.track.author, "Джарахов");
        }
    }
}
