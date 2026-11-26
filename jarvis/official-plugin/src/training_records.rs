//! The optional training plugin owns the recording buffer, local dataset, and Yandex Disk sync.
use crate::{
    storage::{rand_hex, write_atomic},
    training_background::Recorder,
    training_yadisk::{RemoteClip, YandexDisk},
};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

const RATE: usize = 16_000;
pub(crate) const MAX_CLIP: u64 = 6 << 20;
pub const MAX_LIMIT_MB: i64 = 100;
const MIN_FREE_MB: u64 = 5;
const MIN_FREE_BYTES: u64 = MIN_FREE_MB << 20;
const SNAPSHOT_CLIPS_LIMIT: usize = 2500;
const LOCAL_KEEP_REVIEW: usize = 20;
const PREFETCH_CONCURRENCY: usize = 2;
pub(crate) const KINDS: [&str; 4] = ["pos", "false", "near", "bg"];
const CLIPS: &str = "rust-clips";
const OLD_CLIPS: &str = "clips";
const REVIEW_CACHE: &str = "review-cache";
const GZIP: &[u8] = b"\x1f\x8b";
const OFFLOAD_MAX_PER_RUN: usize = 50;
const ARCHIVE_CONCURRENCY: usize = 3;

#[derive(Clone, Serialize)]
pub struct Clip {
    pub kind: String,
    pub name: String,
    pub reviewed: bool,
    pub size: u64,
    pub time: String,
    pub storage: &'static str,
    pub remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_ms: Option<u32>,
    #[serde(skip)]
    dir: &'static str,
}

#[derive(Default)]
struct Observe {
    near: Option<Instant>,
    background: Option<Instant>,
}

#[derive(Default)]
struct PrefetchState {
    running: bool,
    next: Option<Vec<RemoteClip>>,
}

pub struct Records {
    root: PathBuf,
    files: Mutex<()>,
    sample: Mutex<Observe>,
    recent_wakes: Mutex<VecDeque<(String, String, String)>>,
    pending_reviews: Mutex<HashMap<String, String>>,
    save_lock: tokio::sync::Mutex<()>,
    pub yadisk: Arc<YandexDisk>,
    pub background: Arc<Recorder>,
    offload: Mutex<Offload>,
    prefetch: Mutex<PrefetchState>,
    write_pending: AtomicBool,
    stopped: AtomicBool,
}

#[derive(Default)]
struct Offload {
    running: bool,
    sent: usize,
    retry_at: Option<Instant>,
    error: String,
}

impl Records {
    pub fn new(root: PathBuf) -> Arc<Self> {
        let yadisk = YandexDisk::new(&root);
        Self::with_yadisk(root, yadisk)
    }

    pub(crate) fn with_yadisk(root: PathBuf, yadisk: Arc<YandexDisk>) -> Arc<Self> {
        let background = Arc::new(Recorder::new(yadisk.clone()));
        let records = Arc::new(Self {
            root,
            files: Mutex::new(()),
            sample: Mutex::default(),
            recent_wakes: Mutex::default(),
            pending_reviews: Mutex::default(),
            save_lock: tokio::sync::Mutex::const_new(()),
            yadisk: yadisk.clone(),
            background,
            offload: Mutex::default(),
            prefetch: Mutex::default(),
            write_pending: false.into(),
            stopped: false.into(),
        });
        let weak = Arc::downgrade(&records);
        yadisk.set_on_connected(Arc::new(move || {
            if let Some(records) = weak.upgrade() {
                records.offload.lock().unwrap().retry_at = None;
                records.start_offload();
                let records_bg = records.clone();
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move {
                        if let Ok(remote) = records_bg.yadisk.list_clips(false).await {
                            records_bg.start_prefetch(remote);
                        }
                    });
                }
            }
        }));
        records
    }

    pub fn kick_startup_offload(self: &Arc<Self>) {
        if self.yadisk.is_connected() {
            self.start_offload();
            let records = self.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    if let Ok(remote) = records.yadisk.list_clips(false).await {
                        records.start_prefetch(remote);
                    }
                });
            }
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.background.close();
    }

    pub fn sample_kind(&self, hit: bool, prob: f32, loud: bool) -> Option<&'static str> {
        if hit {
            return Some("pos");
        }
        let mut s = self.sample.lock().unwrap();
        if (0.35..0.99).contains(&prob)
            && s.near
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(20))
        {
            s.near = Some(Instant::now());
            return Some("near");
        }
        if loud
            && prob < 0.15
            && s.background
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(420))
        {
            s.background = Some(Instant::now());
            return Some("bg");
        }
        None
    }

    fn remember_wake(&self, wake_id: &str, kind: &str, name: &str) {
        let mut recent = self.recent_wakes.lock().unwrap();
        recent.retain(|(id, _, _)| id != wake_id);
        recent.push_back((wake_id.to_owned(), kind.to_owned(), name.to_owned()));
        while recent.len() > 64 {
            recent.pop_front();
        }
    }

    fn find_wake(&self, wake_id: &str) -> Option<(String, String)> {
        self.recent_wakes
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _, _)| id == wake_id)
            .map(|(_, kind, name)| (kind.clone(), name.clone()))
    }

    pub fn save_soon(
        self: &Arc<Self>,
        rt: &tokio::runtime::Handle,
        kind: &'static str,
        pcm: Vec<i16>,
        prob: f32,
        trigger_ms: Option<u32>,
        limit_mb: i64,
        wake_id: Option<String>,
    ) {
        if self.write_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let records = self.clone();
        rt.spawn(async move {
            struct PendingReset<'a>(&'a AtomicBool);
            impl Drop for PendingReset<'_> {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _reset = PendingReset(&records.write_pending);
            let _guard = records.save_lock.lock().await;
            let mut actual_kind = kind.to_string();
            let mut reviewed = false;
            if let Some(wid) = &wake_id {
                if records.find_wake(wid).is_some() {
                    return;
                }
                if let Some(override_label) = records.pending_reviews.lock().unwrap().remove(wid) {
                    actual_kind = override_label;
                    reviewed = true;
                }
            }
            let files = records.clone();
            let save_kind = actual_kind.clone();
            let saved = tokio::task::spawn_blocking(move || {
                files.save_with_meta(
                    &save_kind,
                    &pcm,
                    prob,
                    trigger_ms,
                    limit_mb,
                    reviewed,
                    || crate::storage::free_bytes(&files.root),
                )
            })
            .await;
            match saved {
                Ok(Ok(Some(name))) => {
                    if let Some(wid) = &wake_id {
                        records.remember_wake(wid, &actual_kind, &name);
                    }
                    if records.yadisk.is_connected() {
                        records.start_offload();
                        let cached = records.yadisk.cached_clips();
                        if !cached.is_empty() {
                            records.start_prefetch(cached);
                        }
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => log::warn!("запись для обучения: {error}"),
                Err(error) => log::warn!("запись для обучения: {error}"),
            }
        });
    }

    pub fn start_offload(self: &Arc<Self>) {
        if !self.yadisk.is_connected() || self.stopped.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.offload.lock().unwrap();
        if state.running || state.retry_at.is_some_and(|at| at > Instant::now()) {
            return;
        }
        state.running = true;
        drop(state);
        let records = self.clone();
        tokio::spawn(async move {
            let result = records.offload_pending().await;
            let mut more = false;
            {
                let mut state = records.offload.lock().unwrap();
                state.running = false;
                match result {
                    Ok((_, has_more)) => {
                        state.error.clear();
                        state.retry_at = None;
                        more = has_more;
                    }
                    Err(error) => {
                        state.error = error;
                        state.retry_at = Some(Instant::now() + Duration::from_secs(60));
                    }
                }
            }
            if more && records.yadisk.is_connected() && !records.stopped.load(Ordering::Acquire) {
                records.start_offload();
            }
        });
    }

    async fn offload_pending(self: &Arc<Self>) -> Result<(usize, bool), String> {
        let mut sent = 0usize;
        for _ in 0..OFFLOAD_MAX_PER_RUN {
            if !self.yadisk.is_connected() || self.stopped.load(Ordering::Acquire) {
                return Ok((sent, false));
            }
            let records = self.clone();
            let next = tokio::task::spawn_blocking(move || records.next_offload_clip())
                .await
                .map_err(|e| e.to_string())??;
            let Some((clip, path, bytes)) = next else {
                return Ok((sent, false));
            };
            self.yadisk
                .upload_clip(&clip.kind, &clip.name, bytes, clip.time)
                .await?;
            let records = self.clone();
            tokio::task::spawn_blocking(move || {
                let _lock = records.files.lock().unwrap();
                if let Err(error) = fs::remove_file(&path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    return Err(error.to_string());
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())??;
            sent += 1;
            {
                let mut state = self.offload.lock().unwrap();
                state.sent += 1;
                state.error.clear();
            }
        }
        let records = self.clone();
        let has_more = tokio::task::spawn_blocking(move || {
            let _lock = records.files.lock().unwrap();
            records.has_offloadable()
        })
        .await
        .unwrap_or(false);
        Ok((sent, has_more))
    }

    fn has_offloadable(&self) -> bool {
        let clips = self.list_dir(CLIPS);
        let keep = keep_review_set(&clips);
        clips
            .iter()
            .any(|c| !keep.contains(&(c.kind.clone(), c.name.clone())))
    }

    fn next_offload_clip(&self) -> Result<Option<(Clip, PathBuf, Vec<u8>)>, String> {
        let _lock = self.files.lock().unwrap();
        let clips = self.list_dir(CLIPS);
        if clips.is_empty() {
            return Ok(None);
        }
        let keep = keep_review_set(&clips);
        let mut offloadable: Vec<Clip> = clips
            .into_iter()
            .filter(|c| !keep.contains(&(c.kind.clone(), c.name.clone())))
            .collect();
        offloadable.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.name.cmp(&b.name)));
        for clip in offloadable {
            let path = self.path(CLIPS, &clip.kind, &clip.name)?;
            match fs::read(&path) {
                Ok(bytes) if (bytes.len() as u64) <= MAX_CLIP => {
                    return Ok(Some((clip, path, bytes)));
                }
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(None)
    }

    pub fn start_prefetch(self: &Arc<Self>, remote: Vec<RemoteClip>) {
        if !self.yadisk.is_connected() || self.stopped.load(Ordering::Acquire) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut st = self.prefetch.lock().unwrap();
            if st.running {
                st.next = Some(remote);
                return;
            }
            st.running = true;
            st.next = None;
        }
        let records = self.clone();
        rt.spawn(async move {
            struct Guard(Arc<Records>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.prefetch.lock().unwrap().running = false;
                }
            }
            let _guard = Guard(records.clone());
            let mut batch = Some(remote);
            while let Some(remote_clips) = batch {
                if !records.yadisk.is_connected() || records.stopped.load(Ordering::Acquire) {
                    break;
                }
                records.prefetch_batch(remote_clips).await;
                batch = records.prefetch.lock().unwrap().next.take();
            }
        });
    }

    async fn prefetch_batch(self: &Arc<Self>, remote: Vec<RemoteClip>) {
        let records = self.clone();
        let Ok(missing) = tokio::task::spawn_blocking(move || {
            let _lock = records.files.lock().unwrap();
            let clips = records.list_dir(CLIPS);
            let old_clips = records.list_dir(OLD_CLIPS);
            let local_unreviewed = clips
                .iter()
                .filter(|c| needs_review_clip(&c.kind, c.reviewed))
                .count();
            let want = LOCAL_KEEP_REVIEW.saturating_sub(local_unreviewed);
            let local_keys: HashSet<(String, String)> = clips
                .into_iter()
                .chain(old_clips)
                .map(|c| (c.kind, c.name))
                .collect();

            let mut candidates: Vec<_> = remote
                .into_iter()
                .filter(|rc| {
                    needs_review_clip(&rc.kind, rc.reviewed)
                        && !local_keys.contains(&(rc.kind.clone(), rc.name.clone()))
                })
                .collect();
            candidates.sort_by(|a, b| {
                review_priority(&a.kind)
                    .cmp(&review_priority(&b.kind))
                    .then_with(|| b.time.cmp(&a.time))
                    .then_with(|| b.name.cmp(&a.name))
            });
            candidates.truncate(want);

            let target_set: HashSet<(String, String)> = candidates
                .iter()
                .map(|c| (c.kind.clone(), c.name.clone()))
                .collect();
            for cached in records.list_dir(REVIEW_CACHE) {
                if !target_set.contains(&(cached.kind.clone(), cached.name.clone()))
                    && let Ok(p) = records.path(REVIEW_CACHE, &cached.kind, &cached.name)
                {
                    let _ = fs::remove_file(p);
                }
            }

            let mut missing = Vec::new();
            for rc in candidates {
                if let Ok(p) = records.path(REVIEW_CACHE, &rc.kind, &rc.name)
                    && !p.is_file()
                {
                    missing.push(rc);
                }
            }
            missing
        })
        .await
        else {
            return;
        };

        if missing.is_empty() {
            return;
        }
        let mut iter = missing.into_iter();
        let mut inflight = tokio::task::JoinSet::new();
        loop {
            while inflight.len() < PREFETCH_CONCURRENCY {
                let Some(rc) = iter.next() else { break };
                let yadisk = self.yadisk.clone();
                inflight.spawn(async move {
                    let res = yadisk.download_clip(&rc.kind, &rc.name).await;
                    (rc.kind, rc.name, res)
                });
            }
            let Some(joined) = inflight.join_next().await else {
                break;
            };
            if self.stopped.load(Ordering::Acquire) || !self.yadisk.is_connected() {
                inflight.abort_all();
                break;
            }
            let Ok((kind, name, Ok(wav))) = joined else {
                continue;
            };
            let records = self.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let compressed = compress(&wav).unwrap_or_else(|_| wav.clone());
                let bytes = if compressed.len() < wav.len() {
                    compressed
                } else {
                    wav
                };
                let _lock = records.files.lock().unwrap();
                if let Some(free) = crate::storage::free_bytes(&records.root)
                    && free < MIN_FREE_BYTES.saturating_add(bytes.len() as u64)
                {
                    return;
                }
                if let Ok(path) = records.path(REVIEW_CACHE, &kind, &name) {
                    if let Some(parent) = path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = write_atomic(&path, &bytes, 0o600);
                }
            })
            .await;
        }
    }

    fn path(&self, dir: &str, kind: &str, name: &str) -> Result<PathBuf, String> {
        if !KINDS.contains(&kind) || !valid_name(name) {
            return Err("Неверное имя записи".into());
        }
        Ok(self.root.join(dir).join(kind).join(name))
    }

    fn locate(&self, kind: &str, name: &str) -> Result<(PathBuf, &'static str), String> {
        for dir in [CLIPS, OLD_CLIPS] {
            let path = self.path(dir, kind, name)?;
            if path.symlink_metadata().is_ok_and(|meta| meta.is_file()) {
                return Ok((path, dir));
            }
        }
        Err("Запись не найдена".into())
    }

    fn list_dir(&self, dir: &'static str) -> Vec<Clip> {
        let mut output = Vec::new();
        for kind in KINDS {
            let Ok(entries) = fs::read_dir(self.root.join(dir).join(kind)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !valid_name(&name) || !entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if meta.len() > MAX_CLIP {
                    continue;
                }
                let changed = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let time = jiff::Timestamp::try_from(changed)
                    .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
                    .to_string();
                output.push(Clip {
                    kind: kind.into(),
                    reviewed: name.ends_with("_r.wav"),
                    trigger_ms: parse_trigger_ms(&name),
                    name,
                    size: meta.len(),
                    time,
                    storage: "local",
                    remote: false,
                    dir,
                });
            }
        }
        output
    }

    fn list_local(&self) -> Vec<Clip> {
        let mut clips = self.list_dir(CLIPS);
        clips.extend(self.list_dir(OLD_CLIPS));
        clips.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.name.cmp(&a.name)));
        clips
    }

    pub async fn snapshot(
        self: &Arc<Self>,
        mode: bool,
        voice_review: bool,
        limit: i64,
        previous: bool,
        force_refresh: bool,
    ) -> Value {
        let records = self.clone();
        let (local_clips, used_clips, legacy_bytes, max_limit_mb, has_offloadable) =
            tokio::task::spawn_blocking(move || {
                let _lock = records.files.lock().unwrap();
                let clips = records.list_local();
                let used_dir = |dir| {
                    clips
                        .iter()
                        .filter(|c| c.dir == dir)
                        .map(|c| c.size)
                        .sum::<u64>()
                };
                let used_clips = used_dir(CLIPS);
                let legacy_bytes = used_dir(OLD_CLIPS);
                let max_limit_mb = max_limit_mb_for_disk(
                    used_clips,
                    crate::storage::free_bytes(&records.root),
                );
                let has_offloadable = records.has_offloadable();
                (clips, used_clips, legacy_bytes, max_limit_mb, has_offloadable)
            })
            .await
            .unwrap_or_else(|_| (Vec::new(), 0, 0, MAX_LIMIT_MB, false));

        let local_count = local_clips.len();
        let mut clips = local_clips;
        let mut seen: HashSet<(String, String)> = clips
            .iter()
            .map(|c| (c.kind.clone(), c.name.clone()))
            .collect();

        let mut remote_count = 0usize;
        let mut cloud_error = String::new();
        let connected = self.yadisk.is_connected();
        if connected {
            if has_offloadable {
                self.start_offload();
            }
            match self.yadisk.list_clips(force_refresh).await {
                Ok(remote) => {
                    self.start_prefetch(remote.clone());
                    for rc in remote {
                        if seen.insert((rc.kind.clone(), rc.name.clone())) {
                            remote_count += 1;
                            clips.push(Clip {
                                kind: rc.kind,
                                trigger_ms: parse_trigger_ms(&rc.name),
                                name: rc.name,
                                reviewed: rc.reviewed,
                                size: rc.size,
                                time: rc.time,
                                storage: "yadisk",
                                remote: true,
                                dir: "",
                            });
                        }
                    }
                }
                Err(err) => {
                    cloud_error = err;
                }
            }
        }

        clips.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.name.cmp(&a.name)));
        let counts = KINDS.map(|kind| (kind, clips.iter().filter(|c| c.kind == kind).count()));
        let offload = self.offload.lock().unwrap();
        let offload_error = if !offload.error.is_empty() {
            offload.error.clone()
        } else {
            cloud_error
        };
        json!({
            "mode": mode,
            "voice_review": voice_review,
            "limit_mb": limit.clamp(1, MAX_LIMIT_MB),
            "max_limit_mb": max_limit_mb,
            "prev_model": previous,
            "used_kb": used_clips >> 10,
            "legacy_kb": legacy_bytes >> 10,
            "local": local_count,
            "remote": remote_count,
            "offload": {
                "connected": connected,
                "running": offload.running,
                "sent": offload.sent,
                "error": offload_error,
            },
            "counts": counts.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
            "clips": clips.iter().take(SNAPSHOT_CLIPS_LIMIT).collect::<Vec<_>>(),
        })
    }

    fn save(
        &self,
        kind: &str,
        pcm: &[i16],
        prob: f32,
        trigger_ms: Option<u32>,
        limit_mb: i64,
    ) -> Result<bool, String> {
        self.save_with_free(kind, pcm, prob, trigger_ms, limit_mb, || {
            crate::storage::free_bytes(&self.root)
        })
    }

    fn save_with_free(
        &self,
        kind: &str,
        pcm: &[i16],
        prob: f32,
        trigger_ms: Option<u32>,
        limit_mb: i64,
        free_bytes: impl FnOnce() -> Option<u64>,
    ) -> Result<bool, String> {
        self.save_with_meta(kind, pcm, prob, trigger_ms, limit_mb, false, free_bytes)
            .map(|name| name.is_some())
    }

    fn save_with_meta(
        &self,
        kind: &str,
        pcm: &[i16],
        prob: f32,
        trigger_ms: Option<u32>,
        limit_mb: i64,
        reviewed: bool,
        free_bytes: impl FnOnce() -> Option<u64>,
    ) -> Result<Option<String>, String> {
        let _lock = self.files.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        let wav = pcm_wav(pcm);
        let compressed = compress(&wav).map_err(|e| e.to_string())?;
        let data = if compressed.len() < wav.len() {
            compressed
        } else {
            wav
        };
        let trigger_part = trigger_ms.map(|ms| format!("_t{ms}")).unwrap_or_default();
        let reviewed_part = if reviewed { "_r" } else { "" };
        let name = format!(
            "{}-{}_p{}{trigger_part}{reviewed_part}.wav",
            jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S"),
            rand_hex(5),
            (prob.clamp(0.0, 1.0) * 100.0) as u32
        );
        let path = self.path(CLIPS, kind, &name)?;
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut clips = self.list_dir(CLIPS);
        let mut used = clips.iter().map(|clip| clip.size).sum::<u64>();
        let free = free_bytes();
        let requested = (limit_mb.clamp(1, MAX_LIMIT_MB) as u64) << 20;
        let disk_budget =
            free.map(|bytes| used.saturating_add(bytes).saturating_sub(MIN_FREE_BYTES));
        let limit = disk_budget.map_or(requested, |budget| requested.min(budget));
        let data_len = data.len() as u64;
        clips.sort_by_key(|clip| {
            (
                clip.reviewed,
                !matches!(clip.kind.as_str(), "bg" | "near"),
                clip.time.clone(),
            )
        });
        for clip in clips {
            if used.saturating_add(data_len) <= limit {
                break;
            }
            if clip.reviewed {
                break;
            }
            if fs::remove_file(self.path(CLIPS, &clip.kind, &clip.name)?).is_ok() {
                used -= clip.size;
            }
        }
        if used.saturating_add(data_len) > limit {
            if disk_budget.is_some_and(|budget| used.saturating_add(data_len) > budget) {
                return Err(format!("на колонке осталось меньше {MIN_FREE_MB} МБ"));
            }
            return Ok(None);
        }
        write_atomic(&path, &data, 0o600).map_err(|e| e.to_string())?;
        Ok(Some(name))
    }

    pub async fn read(self: &Arc<Self>, kind: &str, name: &str) -> Result<Vec<u8>, String> {
        let records = self.clone();
        let kind_owned = kind.to_owned();
        let name_owned = name.to_owned();
        let local = tokio::task::spawn_blocking(move || {
            let _lock = records.files.lock().unwrap();
            if let Ok((path, _)) = records.locate(&kind_owned, &name_owned) {
                return read_clip(&path).map(Some);
            }
            if let Ok(cache_path) = records.path(REVIEW_CACHE, &kind_owned, &name_owned)
                && let Some(wav) = read_clip_if_present(&cache_path)?
            {
                return Ok(Some(wav));
            }
            Ok(None)
        })
        .await
        .map_err(|e| e.to_string())??;
        if let Some(wav) = local {
            return Ok(wav);
        }
        if self.yadisk.is_connected() {
            return self.yadisk.download_clip(kind, name).await;
        }
        Err("Запись не найдена".into())
    }

    pub async fn move_clip(
        self: &Arc<Self>,
        kind: &str,
        name: &str,
        to: &str,
    ) -> Result<(), String> {
        self.move_clip_returning_name(kind, name, to)
            .await
            .map(|_| ())
    }

    pub async fn move_clip_returning_name(
        self: &Arc<Self>,
        kind: &str,
        name: &str,
        to: &str,
    ) -> Result<String, String> {
        let records = self.clone();
        let kind_owned = kind.to_owned();
        let name_owned = name.to_owned();
        let to_owned = to.to_owned();
        let moved_local = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
            let _lock = records.files.lock().unwrap();
            if let Ok(cache_path) = records.path(REVIEW_CACHE, &kind_owned, &name_owned) {
                let _ = fs::remove_file(cache_path);
            }
            let Ok((source, dir)) = records.locate(&kind_owned, &name_owned) else {
                return Ok(None);
            };
            if to_owned == "delete" {
                fs::remove_file(source).map_err(|e| e.to_string())?;
                return Ok(Some(String::new()));
            }
            let stem = name_owned.trim_end_matches(".wav").trim_end_matches("_r");
            let mut target = records.path(dir, &to_owned, &format!("{stem}_r.wav"))?;
            if target == source {
                return Ok(Some(name_owned));
            }
            if target.exists() {
                target = records.path(
                    dir,
                    &to_owned,
                    &format!("{stem}_{}_r.wav", rand_hex(4)),
                )?;
            }
            let new_name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{stem}_r.wav"));
            fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::rename(source, target).map_err(|e| e.to_string())?;
            Ok(Some(new_name))
        })
        .await
        .map_err(|e| e.to_string())??;

        if let Some(new_name) = moved_local {
            if self.yadisk.is_connected() {
                if self.yadisk.has_cached_clip(kind, name) {
                    let _ = self.yadisk.move_clip(kind, name, to).await;
                }
                self.start_offload();
                let cached = self.yadisk.cached_clips();
                if !cached.is_empty() {
                    self.start_prefetch(cached);
                }
            }
            return Ok(new_name);
        }
        if self.yadisk.is_connected() {
            self.yadisk.move_clip(kind, name, to).await?;
            let cached = self.yadisk.cached_clips();
            if !cached.is_empty() {
                self.start_prefetch(cached);
            }
            let stem = name.trim_end_matches(".wav").trim_end_matches("_r");
            return Ok(if to == "delete" {
                String::new()
            } else {
                format!("{stem}_r.wav")
            });
        }
        Err("Запись не найдена".into())
    }

    /// Переместить (или сохранить с пометкой `_r`) запись конкретного wake-события по голосовой разметке.
    pub async fn review_wake(
        self: &Arc<Self>,
        wake_id: &str,
        label: &str,
        pcm: &[i16],
        prob: f32,
        limit_mb: i64,
    ) -> Result<Value, String> {
        if wake_id.is_empty()
            || wake_id.len() > 48
            || !wake_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Некорректный wake_id".into());
        }
        if !matches!(label, "false" | "pos") {
            return Err("Неверная метка разметки".into());
        }
        let _guard = self.save_lock.lock().await;
        if let Some((cur_kind, cur_name)) = self.find_wake(wake_id) {
            let new_name = self.move_clip_returning_name(&cur_kind, &cur_name, label).await?;
            self.remember_wake(wake_id, label, &new_name);
            return Ok(json!({
                "status": "updated",
                "kind": label,
                "name": new_name,
                "reviewed": true,
            }));
        }
        if pcm.len() < RATE / 2 {
            return Err("нет аудиофрагмента пробуждения".into());
        }
        {
            let mut pending = self.pending_reviews.lock().unwrap();
            if pending.len() >= 64 {
                pending.clear();
            }
            pending.insert(wake_id.to_owned(), label.to_owned());
        }
        let records = self.clone();
        let pcm_owned = pcm.to_vec();
        let label_owned = label.to_owned();
        let trigger_ms = Some((pcm.len() * 1000 / RATE) as u32);
        let saved = tokio::task::spawn_blocking(move || {
            records.save_with_meta(
                &label_owned,
                &pcm_owned,
                prob,
                trigger_ms,
                limit_mb,
                true,
                || crate::storage::free_bytes(&records.root),
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        let Some(name) = saved else {
            return Err("лимит хранилища записей заполнен проверенными файлами".into());
        };
        self.pending_reviews.lock().unwrap().remove(wake_id);
        self.remember_wake(wake_id, label, &name);
        if self.yadisk.is_connected() {
            self.start_offload();
        }
        Ok(json!({
            "status": "saved",
            "kind": label,
            "name": name,
            "reviewed": true,
        }))
    }

    pub async fn clear(self: &Arc<Self>) -> Result<(), String> {
        let records = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = records.files.lock().unwrap();
            for clip in records.list_local() {
                fs::remove_file(records.path(clip.dir, &clip.kind, &clip.name)?)
                    .map_err(|e| e.to_string())?;
            }
            let _ = fs::remove_dir_all(records.root.join(REVIEW_CACHE));
            Ok::<_, String>(())
        })
        .await
        .map_err(|e| e.to_string())??;
        if self.yadisk.is_connected() {
            self.yadisk.clear_clips().await?;
        }
        Ok(())
    }

    /// Compact the internal files without changing their logical names or WAV downloads.
    pub fn compact(&self) -> Result<Value, String> {
        let clips = {
            let _lock = self.files.lock().unwrap();
            self.list_local()
        };
        let mut changed = 0;
        let mut saved = 0;
        for clip in clips {
            let _lock = self.files.lock().unwrap();
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let path = self.path(clip.dir, &clip.kind, &clip.name)?;
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_CLIP {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            if bytes.starts_with(GZIP) || !bytes.starts_with(b"RIFF") {
                continue;
            }
            let smaller = compress(&bytes).map_err(|e| e.to_string())?;
            if smaller.len() >= bytes.len() {
                continue;
            }
            write_atomic(&path, &smaller, 0o600).map_err(|e| e.to_string())?;
            if let (Ok(file), Ok(time)) = (fs::File::open(&path), meta.modified()) {
                let _ = file.set_modified(time);
            }
            changed += 1;
            saved += bytes.len() - smaller.len();
        }
        Ok(json!({"changed":changed,"saved_bytes":saved}))
    }

    pub async fn archive<W: Write + Send + 'static>(
        self: &Arc<Self>,
        data_dir: &Path,
        sensitivity: i64,
        output: W,
    ) -> Result<(), String> {
        let root = self.root.clone();
        let data_dir = data_dir.to_path_buf();
        let model = tokio::task::spawn_blocking(move || {
            fs::read(root.join("wake.bin.tuned"))
                .or_else(|_| fs::read(data_dir.join("wake.bin")))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;

        let records = self.clone();
        let local_clips = tokio::task::spawn_blocking(move || {
            let _lock = records.files.lock().unwrap();
            records.list_local()
        })
        .await
        .map_err(|e| e.to_string())?;

        let remote_clips = if self.yadisk.is_connected() {
            self.yadisk.list_clips(true).await?
        } else {
            Vec::new()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(4);
        let writer_handle = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut zip = zip::ZipWriter::new(ArchiveWriter::new(output));
            zip.set_flush_on_finish_file(true);
            let options = archive_options();
            while let Some((name, data)) = rx.blocking_recv() {
                zip.start_file(&name, options).map_err(|e| e.to_string())?;
                zip.write_all(&data).map_err(|e| e.to_string())?;
            }
            let mut writer = zip.finish().map_err(|e| e.to_string())?;
            writer.flush().map_err(|e| e.to_string())
        });

        let produce: Result<(), String> = async {
            tx.send(("wake.bin".into(), model))
                .await
                .map_err(|_| "Поток архива закрыт".to_string())?;
            tx.send((
                "wake.json".into(),
                json!({"sensitivity": sensitivity}).to_string().into_bytes(),
            ))
            .await
            .map_err(|_| "Поток архива закрыт".to_string())?;
            tx.send((
                "finetune.py".into(),
                include_bytes!("../../modules/wake-train/assets/finetune.py").to_vec(),
            ))
            .await
            .map_err(|_| "Поток архива закрыт".to_string())?;

            let mut included = HashSet::<(String, String)>::new();
            for clip in local_clips {
                if self.stopped.load(Ordering::Acquire) {
                    return Err("Плагин остановлен".into());
                }
                let records = self.clone();
                let dir = clip.dir;
                let kind = clip.kind.clone();
                let name = clip.name.clone();
                let data = tokio::task::spawn_blocking(move || {
                    let path = records.path(dir, &kind, &name)?;
                    let _lock = records.files.lock().unwrap();
                    read_clip_if_present(&path)
                })
                .await
                .map_err(|e| e.to_string())??;
                let Some(data) = data else {
                    continue;
                };
                included.insert((clip.kind.clone(), clip.name.clone()));
                tx.send((format!("{}/{}", clip.kind, clip.name), data))
                    .await
                    .map_err(|_| "Поток архива закрыт".to_string())?;
            }

            let pending_remote: Vec<_> = remote_clips
                .into_iter()
                .filter(|rc| included.insert((rc.kind.clone(), rc.name.clone())))
                .collect();
            let mut inflight = VecDeque::new();
            let mut iter = pending_remote.into_iter();
            loop {
                while inflight.len() < ARCHIVE_CONCURRENCY {
                    let Some(rc) = iter.next() else { break };
                    let yadisk = self.yadisk.clone();
                    let kind = rc.kind;
                    let name = rc.name;
                    inflight.push_back(tokio::spawn(async move {
                        let res = yadisk.download_clip(&kind, &name).await;
                        (kind, name, res)
                    }));
                }
                let Some(task) = inflight.pop_front() else {
                    break;
                };
                if self.stopped.load(Ordering::Acquire) {
                    for rest in inflight {
                        rest.abort();
                    }
                    return Err("Плагин остановлен".into());
                }
                let (kind, name, res) = task.await.map_err(|e| e.to_string())?;
                match res {
                    Ok(data) => {
                        tx.send((format!("{kind}/{name}"), data))
                            .await
                            .map_err(|_| "Поток архива закрыт".to_string())?;
                    }
                    Err(err) if err.contains("не найдена") => continue,
                    Err(err) => {
                        for rest in inflight {
                            rest.abort();
                        }
                        return Err(err);
                    }
                }
            }
            Ok(())
        }
        .await;

        drop(tx);
        let written = writer_handle.await.map_err(|e| e.to_string())?;
        produce?;
        written
    }
}

pub(crate) fn needs_review_clip(kind: &str, reviewed: bool) -> bool {
    !reviewed && kind != "bg"
}

pub(crate) fn review_priority(kind: &str) -> u8 {
    match kind {
        "near" => 0,
        "pos" => 1,
        "false" => 2,
        _ => 3,
    }
}

pub(crate) fn parse_trigger_ms(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".wav")?;
    let stem = stem.strip_suffix("_r").unwrap_or(stem);
    let (_, tail) = stem.rsplit_once("_t")?;
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    tail.parse().ok()
}

fn keep_review_set(clips: &[Clip]) -> HashSet<(String, String)> {
    let mut review: Vec<&Clip> = clips
        .iter()
        .filter(|c| needs_review_clip(&c.kind, c.reviewed))
        .collect();
    review.sort_by(|a, b| {
        review_priority(&a.kind)
            .cmp(&review_priority(&b.kind))
            .then_with(|| b.time.cmp(&a.time))
            .then_with(|| b.name.cmp(&a.name))
    });
    review
        .into_iter()
        .take(LOCAL_KEEP_REVIEW)
        .map(|c| (c.kind.clone(), c.name.clone()))
        .collect()
}

fn max_limit_mb_for_disk(used: u64, free: Option<u64>) -> i64 {
    match free {
        Some(free) => {
            let budget_mb =
                (used.saturating_add(free).saturating_sub(MIN_FREE_BYTES) >> 20) as i64;
            budget_mb.clamp(1, MAX_LIMIT_MB)
        }
        None => MAX_LIMIT_MB,
    }
}

// ZipWriter patches the current entry's header before flushing it. Keep just
// that entry (or the final directory) seekable while sending completed bytes.
struct ArchiveWriter<W> {
    output: W,
    pending: Cursor<Vec<u8>>,
    sent: u64,
}

impl<W> ArchiveWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            pending: Cursor::new(Vec::new()),
            sent: 0,
        }
    }
}

impl<W> Read for ArchiveWriter<W> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.pending.read(bytes)
    }
}

impl<W: Write> Write for ArchiveWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.write_all(self.pending.get_ref())?;
        self.sent += self.pending.get_ref().len() as u64;
        self.pending.get_mut().clear();
        self.pending.set_position(0);
        self.output.flush()
    }
}

impl<W> Seek for ArchiveWriter<W> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let position = match position {
            SeekFrom::Start(offset) => {
                SeekFrom::Start(offset.checked_sub(self.sent).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot seek into a completed ZIP entry",
                    )
                })?)
            }
            relative => relative,
        };
        self.pending.seek(position).map(|offset| self.sent + offset)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.sent + self.pending.position())
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.len() <= 112
        && name.ends_with(".wav")
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn decode_clip_bytes(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.len() as u64 > MAX_CLIP {
        return Err("Запись слишком большая".into());
    }
    if !bytes.starts_with(GZIP) {
        if !bytes.starts_with(b"RIFF") {
            return Err("Повреждена запись".into());
        }
        return Ok(bytes);
    }
    let mut wav = Vec::new();
    GzDecoder::new(bytes.as_slice())
        .take(MAX_CLIP + 1)
        .read_to_end(&mut wav)
        .map_err(|e| e.to_string())?;
    if wav.len() as u64 > MAX_CLIP || !wav.starts_with(b"RIFF") {
        return Err("Повреждена сжатая запись".into());
    }
    Ok(wav)
}

fn read_clip(path: &Path) -> Result<Vec<u8>, String> {
    read_clip_if_present(path)?.ok_or_else(|| "Запись не найдена".into())
}

fn read_clip_if_present(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match path.symlink_metadata() {
        Ok(m) if m.is_file() && m.len() <= MAX_CLIP => {}
        Ok(_) => return Err("Запись не найдена".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let mut bytes = Vec::new();
    match fs::File::open(path).and_then(|f| f.take(MAX_CLIP + 1).read_to_end(&mut bytes)) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    if bytes.len() as u64 > MAX_CLIP {
        return Err("Запись слишком большая".into());
    }
    if !bytes.starts_with(GZIP) {
        return Ok(Some(bytes));
    }
    decode_clip_bytes(bytes).map(Some)
}

fn compress(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes)?;
    encoder.finish()
}

pub fn pcm_wav(pcm: &[i16]) -> Vec<u8> {
    let size = (pcm.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + size as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    bytes.extend_from_slice(&(RATE as u32).to_le_bytes());
    bytes.extend_from_slice(&((RATE * 2) as u32).to_le_bytes());
    bytes.extend_from_slice(b"\x02\0\x10\0data");
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend(pcm.iter().flat_map(|sample| sample.to_le_bytes()));
    bytes
}

fn archive_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        extract::{Path as AxumPath, State},
        http::{StatusCode, Uri, header},
        routing::{get, post, put},
    };
    use std::collections::BTreeMap;

    fn qmap(uri: &Uri) -> BTreeMap<String, String> {
        reqwest::Url::parse(&format!("http://x{uri}"))
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default()
    }

    fn jbody(value: Value) -> ([(header::HeaderName, &'static str); 1], String) {
        ([(header::CONTENT_TYPE, "application/json")], value.to_string())
    }

    #[tokio::test]
    async fn yadisk_offloads_local_clips_serves_playback_sorting_and_unified_zip() {
        let files = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let fail_upload = Arc::new(AtomicBool::new(true));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let base_u = base.clone();
        let base_d = base.clone();
        let fail_c = fail_upload.clone();
        let app = Router::new()
            .route(
                "/v1/disk",
                get(|| async {
                    jbody(json!({
                        "total_space": 10u64 << 30,
                        "used_space": 1u64 << 30,
                        "user": {"login": "u", "display_name": "U"}
                    }))
                }),
            )
            .route(
                "/v1/disk/resources",
                put(|| async { StatusCode::CREATED }).get(
                    |State(st): State<Arc<Mutex<BTreeMap<String, Vec<u8>>>>>, uri: Uri| async move {
                        let q = qmap(&uri);
                        let prefix = format!("{}/", q.get("path").cloned().unwrap_or_default());
                        let s = st.lock().unwrap();
                        let items: Vec<Value> = s
                            .iter()
                            .filter_map(|(k, v)| {
                                let name = k.strip_prefix(&prefix)?;
                                if name.contains('/') {
                                    return None;
                                }
                                Some(json!({
                                    "name": name,
                                    "size": v.len(),
                                    "modified": "2026-11-25T10:00:00+00:00"
                                }))
                            })
                            .collect();
                        let total = items.len();
                        jbody(json!({"_embedded": {"items": items, "total": total}}))
                    },
                ),
            )
            .route(
                "/v1/disk/resources/upload",
                get(move |uri: Uri| {
                    let base = base_u.clone();
                    async move {
                        let q = qmap(&uri);
                        let encoded = q
                            .get("path")
                            .cloned()
                            .unwrap_or_default()
                            .trim_start_matches("app:/")
                            .replace('/', "__");
                        jbody(json!({"href": format!("{base}/t/{encoded}")}))
                    }
                }),
            )
            .route(
                "/v1/disk/resources/download",
                get(move |uri: Uri| {
                    let base = base_d.clone();
                    async move {
                        let q = qmap(&uri);
                        let encoded = q
                            .get("path")
                            .cloned()
                            .unwrap_or_default()
                            .trim_start_matches("app:/")
                            .replace('/', "__");
                        jbody(json!({"href": format!("{base}/t/{encoded}")}))
                    }
                }),
            )
            .route(
                "/v1/disk/resources/move",
                post(
                    |State(st): State<Arc<Mutex<BTreeMap<String, Vec<u8>>>>>, uri: Uri| async move {
                        let q = qmap(&uri);
                        let mut s = st.lock().unwrap();
                        let from = q.get("from").cloned().unwrap_or_default();
                        let to = q.get("path").cloned().unwrap_or_default();
                        if let Some(d) = s.remove(&from) {
                            s.insert(to, d);
                            StatusCode::CREATED
                        } else {
                            StatusCode::NOT_FOUND
                        }
                    },
                ),
            )
            .route(
                "/t/{key}",
                put(
                    move |State(st): State<Arc<Mutex<BTreeMap<String, Vec<u8>>>>>,
                          AxumPath(key): AxumPath<String>,
                          data: Bytes| {
                        let fail = fail_c.clone();
                        async move {
                            if fail.load(Ordering::Relaxed) {
                                return StatusCode::SERVICE_UNAVAILABLE;
                            }
                            let path = format!("app:/{}", key.replace("__", "/"));
                            st.lock().unwrap().insert(path, data.to_vec());
                            StatusCode::CREATED
                        }
                    },
                )
                .get(
                    |State(st): State<Arc<Mutex<BTreeMap<String, Vec<u8>>>>>,
                     AxumPath(key): AxumPath<String>| async move {
                        let path = format!("app:/{}", key.replace("__", "/"));
                        match st.lock().unwrap().get(&path).cloned() {
                            Some(b) => (StatusCode::OK, b),
                            None => (StatusCode::NOT_FOUND, Vec::new()),
                        }
                    },
                ),
            )
            .with_state(files.clone());

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let root = std::env::temp_dir().join(format!("jarvis-yd-offload-{}", rand_hex(5)));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("yandex-disk.json"),
            json!({"access_token": "tok"}).to_string(),
        )
        .unwrap();
        let yadisk = YandexDisk::with_endpoints(
            root.join("yandex-disk.json"),
            root.join("background-cloud.json"),
            &base,
            &format!("{base}/v1/disk"),
            true,
        );
        let records = Records::with_yadisk(root.clone(), yadisk);

        let wav = pcm_wav(&[241; 16_000]);
        let compressed = compress(&wav).unwrap();
        let near = root.join(CLIPS).join("near");
        let pos = root.join(CLIPS).join("pos");
        let bg = root.join(CLIPS).join("bg");
        fs::create_dir_all(&near).unwrap();
        fs::create_dir_all(&pos).unwrap();
        fs::create_dir_all(&bg).unwrap();
        fs::write(near.join("c1.wav"), &compressed).unwrap();
        fs::write(pos.join("done_r.wav"), &compressed).unwrap();
        fs::write(bg.join("b1.wav"), &compressed).unwrap();
        fs::write(root.join("wake.bin"), b"model").unwrap();

        // While Yandex Disk returns 503, local offloadable clips (bg & reviewed) stay untouched.
        assert!(records.offload_pending().await.is_err());
        assert_eq!(records.list_dir(CLIPS).len(), 3);

        // Once upload succeeds, bg and reviewed clips offload, while <=20 unreviewed stay local on speaker!
        fail_upload.store(false, Ordering::Relaxed);
        let (sent, has_more) = records.offload_pending().await.unwrap();
        assert_eq!(sent, 2);
        assert!(!has_more);
        let remaining = records.list_dir(CLIPS);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "c1.wav");

        // Snapshot shows 1 local + 2 remote clips with storage="yadisk" & remote=true.
        let snap = records.snapshot(true, false, 10, false, true).await;
        assert_eq!(snap["local"], 1);
        assert_eq!(snap["remote"], 2);
        assert_eq!(snap["offload"]["sent"], 2);
        assert!(snap["clips"].as_array().unwrap().iter().any(|c| c["remote"] == true && c["storage"] == "yadisk"));

        // Local unreviewed clip and remote reviewed clip are both readable.
        assert_eq!(records.read("near", "c1.wav").await.unwrap(), wav);
        assert_eq!(records.read("pos", "done_r.wav").await.unwrap(), wav);

        // Sorting local unreviewed clip marks it reviewed and triggers background offload;
        // add one legacy clip and verify unified ZIP contains both local + remote clips.
        records.move_clip("near", "c1.wav", "pos").await.unwrap();
        let (sent_after_review, _) = records.offload_pending().await.unwrap();
        assert_eq!(sent_after_review, 1);
        assert_eq!(records.list_dir(CLIPS).len(), 0);

        let old = root.join(OLD_CLIPS).join("bg").join("local_bg.wav");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, &wav).unwrap();

        let out_zip = root.join("all.zip");
        records
            .archive(&root, 8, fs::File::create(&out_zip).unwrap())
            .await
            .unwrap();
        let mut zip = zip::ZipArchive::new(fs::File::open(&out_zip).unwrap()).unwrap();
        assert!(zip.by_name("wake.bin").is_ok());
        assert!(zip.by_name("pos/c1_r.wav").is_ok());
        assert!(zip.by_name("pos/done_r.wav").is_ok());
        assert!(zip.by_name("bg/b1.wav").is_ok());
        assert!(zip.by_name("bg/local_bg.wav").is_ok());
        drop(zip);

        // Now add 23 unreviewed clips (10 near, 10 pos, 3 false) + verify 3 lowest-priority (false) offload
        // while 20 stay on speaker, and prefetch caches those remote unreviewed clips up to 20 after reviewing one!
        for i in 0..10 {
            fs::write(near.join(format!("n{i:02}_t2800.wav")), &compressed).unwrap();
            fs::write(pos.join(format!("p{i:02}_t2900.wav")), &compressed).unwrap();
        }
        let false_dir = root.join(CLIPS).join("false");
        fs::create_dir_all(&false_dir).unwrap();
        for i in 0..3 {
            fs::write(false_dir.join(format!("f{i:02}_t2500.wav")), &compressed).unwrap();
        }
        assert_eq!(records.list_dir(CLIPS).len(), 23);
        let (sent_overflow, has_more_overflow) = records.offload_pending().await.unwrap();
        assert_eq!(sent_overflow, 3);
        assert!(!has_more_overflow);
        let left_local = records.list_dir(CLIPS);
        assert_eq!(left_local.len(), 20);
        assert!(left_local.iter().all(|c| c.kind != "false"));
        assert!(left_local.iter().all(|c| c.trigger_ms.is_some()));

        // Fetch remote list & review 1 local clip -> remaining local = 19 -> prefetch fills 1 false clip into review-cache.
        records.move_clip("near", "n00_t2800.wav", "pos").await.unwrap();
        let _ = records.offload_pending().await.unwrap();
        assert_eq!(records.list_dir(CLIPS).len(), 19);
        let snap2 = records.snapshot(true, false, 10, false, true).await;
        assert_eq!(snap2["local"], 20); // 19 in CLIPS + 1 legacy local_bg.wav in OLD_CLIPS
        for _ in 0..40 {
            if records.list_dir(REVIEW_CACHE).len() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(records.list_dir(REVIEW_CACHE).len(), 1);
        // Disable server after prefetch so reading proves it comes from local review-cache without network!
        fail_upload.store(true, Ordering::Relaxed);
        assert_eq!(records.read("false", "f02_t2500.wav").await.unwrap(), wav);

        server.abort();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn keeps_collecting_when_the_archive_is_above_five_megabytes() {
        let root = std::env::temp_dir().join(format!("jarvis-training-limit-{}", rand_hex(5)));
        let records = Records::new(root.clone());
        let reviewed = root.join(CLIPS).join("pos").join("large_r.wav");
        fs::create_dir_all(reviewed.parent().unwrap()).unwrap();
        fs::File::create(&reviewed).unwrap().set_len(5 << 20).unwrap();

        records
            .save_with_free("near", &[241; 16_000], 0.49, Some(2850), 100, || Some(200 << 20))
            .unwrap();

        let snapshot = records.snapshot(true, true, 100, false, false).await;
        assert_eq!(snapshot["voice_review"], true);
        assert_eq!(snapshot["limit_mb"], 100);
        assert_eq!(snapshot["local"], 2);
        let saved_clip = snapshot["clips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["kind"] == "near")
            .unwrap();
        assert_eq!(saved_clip["trigger_ms"], 2850);
        assert!(reviewed.is_file());
        assert_eq!(
            records.snapshot(true, false, 101, false, false).await["limit_mb"],
            100
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enforces_100_mib_max_and_keeps_5_mib_free_on_disk() {
        assert_eq!(max_limit_mb_for_disk(10 << 20, Some(45 << 20)), 50);
        assert_eq!(max_limit_mb_for_disk(0, Some(200 << 20)), 100);
        assert_eq!(max_limit_mb_for_disk(0, Some(3 << 20)), 1);

        let root = std::env::temp_dir().join(format!("jarvis-training-free-{}", rand_hex(5)));
        let records = Records::new(root.clone());
        let near = root.join(CLIPS).join("near");
        let pos = root.join(CLIPS).join("pos");
        fs::create_dir_all(&near).unwrap();
        fs::create_dir_all(&pos).unwrap();

        let old_path = near.join("old.wav");
        fs::File::create(&old_path).unwrap().set_len(200 << 10).unwrap();
        records
            .save_with_free("near", &[241; 16_000], 0.49, None, 100, || Some(5 << 20))
            .unwrap();
        assert!(!old_path.exists());
        assert_eq!(records.list_dir(CLIPS).len(), 1);

        fs::remove_file(near.join(records.list_dir(CLIPS)[0].name.as_str())).unwrap();
        let keep_path = pos.join("keep_r.wav");
        fs::File::create(&keep_path).unwrap().set_len(200 << 10).unwrap();

        let err = records
            .save_with_free("near", &[241; 16_000], 0.49, None, 100, || Some(4 << 20))
            .unwrap_err();
        assert!(err.contains("5 МБ"), "unexpected err: {err}");
        assert!(keep_path.is_file());
        assert_eq!(records.list_dir(CLIPS).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn exports_existing_and_new_recordings_with_labels_and_model() {
        let root = std::env::temp_dir().join(format!("jarvis-dataset-{}", rand_hex(5)));
        let records = Records::new(root.clone());
        let old = root.join(OLD_CLIPS).join("near").join("legacy.wav");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, vec![0; 1 << 20]).unwrap();
        let pcm: Vec<_> = (0..16000).map(|i| (i % 100) as i16).collect();
        records.save("near", &pcm, 0.49, Some(950), 1).unwrap();
        assert!(old.is_file());
        let clip = records.list_dir(CLIPS).pop().unwrap();
        assert!(!clip.reviewed);
        assert_eq!(clip.trigger_ms, Some(950));
        assert!(records.read("near", "../wake.bin").await.is_err());
        records.move_clip("near", &clip.name, "pos").await.unwrap();
        let reviewed = records.list_dir(CLIPS).pop().unwrap();
        assert!(reviewed.reviewed);
        assert_eq!(
            &records.read("pos", &reviewed.name).await.unwrap()[..4],
            b"RIFF"
        );
        fs::write(root.join("wake.bin"), b"model").unwrap();
        let out_zip = root.join("export.zip");
        records
            .archive(&root, 8, fs::File::create(&out_zip).unwrap())
            .await
            .unwrap();
        let mut zip = zip::ZipArchive::new(fs::File::open(&out_zip).unwrap()).unwrap();
        assert!(zip.by_name("finetune.py").is_ok());
        assert!(zip.by_name("near/legacy.wav").is_ok());
        assert!(zip.by_name(&format!("pos/{}", reviewed.name)).is_ok());
        assert!(zip.by_name("wake.bin").is_ok());
        drop(zip);
        let raw = pcm_wav(&pcm);
        fs::write(&old, &raw).unwrap();
        assert_eq!(records.compact().unwrap()["changed"], 1);
        assert_eq!(records.compact().unwrap()["changed"], 0);
        assert_eq!(records.read("near", "legacy.wav").await.unwrap(), raw);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn voice_review_moves_relabels_and_prevents_duplicates_without_dropping_errors() {
        let root = std::env::temp_dir().join(format!("jarvis-voice-review-{}", rand_hex(5)));
        let records = Records::new(root.clone());
        let pcm: Vec<i16> = (0..16_000).map(|i| (i % 200) as i16).collect();
        let rt = tokio::runtime::Handle::current();

        // 1) Auto-saved pos clip is moved in-place to false/*_r.wav (no duplicate left in pos).
        records.save_soon(&rt, "pos", pcm.clone(), 0.91, Some(1500), 10, Some("w111111".into()));
        for _ in 0..40 {
            if records.list_dir(CLIPS).len() == 1 && !records.write_pending.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(records.list_dir(CLIPS).len(), 1);
        assert_eq!(records.list_dir(CLIPS)[0].kind, "pos");
        assert!(!records.list_dir(CLIPS)[0].reviewed);

        let moved = records.review_wake("w111111", "false", &pcm, 0.91, 10).await.unwrap();
        assert_eq!(moved["status"], "updated");
        assert_eq!(moved["kind"], "false");
        assert_eq!(moved["reviewed"], true);
        let clips = records.list_dir(CLIPS);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].kind, "false");
        assert!(clips[0].reviewed);

        // 2) Repeated idempotent call and switching false -> pos keep 1 clip without duplicates.
        let again = records.review_wake("w111111", "false", &pcm, 0.91, 10).await.unwrap();
        assert_eq!(again["kind"], "false");
        assert_eq!(records.list_dir(CLIPS).len(), 1);

        let relabeled = records.review_wake("w111111", "pos", &pcm, 0.91, 10).await.unwrap();
        assert_eq!(relabeled["kind"], "pos");
        let clips = records.list_dir(CLIPS);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].kind, "pos");
        assert!(clips[0].reviewed);

        // 3) Direct reviewed save when train_mode=false + late save_soon for same wake_id does not duplicate.
        let direct = records.review_wake("w222222", "false", &pcm, 0.88, 10).await.unwrap();
        assert_eq!(direct["status"], "saved");
        assert_eq!(direct["kind"], "false");
        assert_eq!(records.list_dir(CLIPS).len(), 2);

        records.save_soon(&rt, "pos", pcm.clone(), 0.88, Some(1500), 10, Some("w222222".into()));
        for _ in 0..20 {
            if !records.write_pending.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(records.list_dir(CLIPS).len(), 2);

        let snap = records.snapshot(false, true, 10, false, false).await;
        assert_eq!(snap["mode"], false);
        assert_eq!(snap["voice_review"], true);

        // 4) Low free disk space and full reviewed budget surface errors instead of returning Ok.
        let low_disk = records
            .save_with_meta("false", &pcm, 0.9, Some(1500), 100, true, || Some(4 << 20))
            .unwrap_err();
        assert!(low_disk.contains("5 МБ"));

        let keep_path = root.join(CLIPS).join("pos").join("full_r.wav");
        fs::File::create(&keep_path).unwrap().set_len(1 << 20).unwrap();
        let full_err = records.review_wake("w999999", "false", &pcm, 0.95, 1).await.unwrap_err();
        assert!(full_err.contains("лимит") || full_err.contains("5 МБ"));

        let _ = fs::remove_dir_all(root);
    }
}
