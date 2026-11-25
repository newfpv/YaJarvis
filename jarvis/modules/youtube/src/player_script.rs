//! YouTube player script, evaluated without filesystem, process or network bindings.
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use jarvis_home_common::{bytes, save};
use regex_lite::Regex;
use reqwest::Client;
use rquickjs::{CatchResultExt, Context, Function, Object, Runtime};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const EJS: &str = include_str!("ejs.js");
const WEB_AGENT: &str = super::remote::WEB_AGENT;
const MAX_PARSE_HEAP: usize = 36 << 20;
const MAX_SOLVER_HEAP: usize = 16 << 20;
const MAX_CACHE_BYTES: usize = 2 << 20;
const MAX_CACHE_UNPACKED: usize = 6 << 20;
const JS_TIMEOUT: Duration = Duration::from_secs(180);
const SOLVER_LOAD_TIMEOUT: Duration = Duration::from_secs(15);
const DISK_VERIFY_TTL: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedPlayer {
    id: String,
    timestamp: u32,
    chunks: Vec<String>,
}

pub struct Prepared {
    context: Context,
    _runtime: Runtime,
    deadline: Arc<Mutex<Instant>>,
    pub timestamp: u32,
}
impl Prepared {
    fn extract_chunks(source: String) -> Result<(u32, Vec<String>), String> {
        let timestamp = Regex::new(r"signatureTimestamp\s*[=:]\s*(\d+)")
            .unwrap()
            .captures(&source)
            .and_then(|m| m[1].parse().ok())
            .ok_or("Нет версии подписи плеера YouTube")?;
        let body_start =
            Regex::new(r"^\s*(var\s+[\w$]+\s*=\s*\{\}\s*;\s*)?\(function\(([\w$]*)\)\{").unwrap();
        let body_end =
            Regex::new(r"\}\)(\.call\(this(,\s*[\w$]+)?\)|\([\w$]*\))\s*;?\s*$").unwrap();
        let head = body_start
            .captures(&source)
            .ok_or("Неизвестный формат плеера YouTube")?;
        let end = body_end
            .find(&source)
            .ok_or("Нет конца плеера YouTube")?
            .start();
        let body = source
            .get(head.get(0).unwrap().end()..end)
            .ok_or("Неверный формат плеера YouTube")?;
        let cuts = boundaries(body.as_bytes());
        // Run the heavy meriyah AST parser in a temporary Runtime and drop it immediately
        // so EJS (~15 MB) never stays resident in the solver Context.
        let runtime = Runtime::new().map_err(|_| "Не запустить движок плеера YouTube")?;
        runtime.set_memory_limit(MAX_PARSE_HEAP);
        runtime.set_max_stack_size(6 << 20);
        let deadline = Arc::new(Mutex::new(Instant::now() + JS_TIMEOUT));
        let cutoff = deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            Instant::now() >= *cutoff.lock().unwrap()
        })));
        let context = Context::full(&runtime).map_err(|_| "Не создать окружение плеера YouTube")?;
        let chunks = context
            .with(|ctx| {
                ctx.eval::<(), _>(EJS)
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                let begin: Function = ctx
                    .eval("JarvisEjs.begin")
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                let job: Object = begin
                    .call((&head[2], head.get(1).is_some()))
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                let add: Function = job.get("add").map_err(|e| e.to_string())?;
                let finish: Function = job.get("finish").map_err(|e| e.to_string())?;
                let mut start = 0;
                let mut index = 0;
                while index < cuts.len() {
                    let mut part = (index + 31).min(cuts.len() - 1);
                    loop {
                        let end = cuts[part];
                        if add
                            .call::<_, bool>((&body[start..end],))
                            .catch(&ctx)
                            .map_err(|e| format!("parse {start}..{end}: {e}"))?
                        {
                            start = end;
                            index = part + 1;
                            break;
                        }
                        if part == cuts.len() - 1 || end - start > 256 << 10 {
                            return Err("Неизвестный синтаксис плеера".to_string());
                        }
                        part = (part + 8).min(cuts.len() - 1);
                    }
                }
                finish
                    .call::<_, Vec<String>>(())
                    .catch(&ctx)
                    .map_err(|e| format!("prepare retained functions: {e}"))
            })
            .map_err(|e| format!("Не разобрать текущий плеер YouTube: {e}"))?;
        Ok((timestamp, chunks))
    }

    pub(crate) fn from_chunks(timestamp: u32, chunks: &[String]) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|_| "Не запустить движок плеера YouTube")?;
        runtime.set_memory_limit(MAX_SOLVER_HEAP);
        runtime.set_max_stack_size(2 << 20);
        let deadline = Arc::new(Mutex::new(Instant::now() + SOLVER_LOAD_TIMEOUT));
        let cutoff = deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            Instant::now() >= *cutoff.lock().unwrap()
        })));
        let context = Context::full(&runtime).map_err(|_| "Не создать окружение плеера YouTube")?;
        for (index, chunk) in chunks.iter().enumerate() {
            context
                .with(|ctx| {
                    ctx.eval::<(), _>(chunk.as_str())
                        .catch(&ctx)
                        .map_err(|e| e.to_string())
                })
                .map_err(|e| format!("Не запустить плеер YouTube, фрагмент {index}: {e}"))?;
        }
        context
            .with(|ctx| {
                ctx.globals()
                    .get::<_, Function>("__solve")
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| format!("В кэше плеера нет __solve: {e}"))?;
        runtime.set_max_stack_size(512 << 10);
        runtime.run_gc();
        Ok(Self {
            context,
            _runtime: runtime,
            deadline,
            timestamp,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(source: String) -> Result<Self, String> {
        let (timestamp, chunks) = Self::extract_chunks(source)?;
        Self::from_chunks(timestamp, &chunks)
    }
    pub async fn decode(
        self: &Arc<Self>,
        signature: String,
        n: String,
    ) -> Result<(String, String), String> {
        let prepared = self.clone();
        tokio::task::spawn_blocking(move || {
            prepared
                .context
                .with(|ctx| {
                    *prepared.deadline.lock().unwrap() = Instant::now() + Duration::from_secs(2);
                    let solve: Function = ctx.globals().get("__solve")?;
                    let mut output = [String::new(), String::new()];
                    for (i, (kind, value)) in [("sig", signature), ("n", n)].into_iter().enumerate()
                    {
                        if value.is_empty() {
                            continue;
                        }
                        output[i] = solve.call((kind, value))?;
                    }
                    let [signature, n] = output;
                    Ok::<_, rquickjs::Error>((signature, n))
                })
                .map_err(|_| "Не расшифровать ссылку YouTube".into())
        })
        .await
        .map_err(|_| "Движок плеера завершился")?
    }
}

#[derive(Default, Clone)]
pub struct Scripts(Arc<Mutex<ScriptCache>>);
#[derive(Default)]
struct ScriptCache {
    cache_path: Option<PathBuf>,
    current: Option<(Instant, Instant, String, Arc<Prepared>)>,
    failed: Option<(Instant, String)>,
    inflight: Option<tokio::sync::watch::Receiver<Option<Result<Arc<Prepared>, String>>>>,
}
struct ClearInflight<'a>(&'a Arc<Mutex<ScriptCache>>);
impl Drop for ClearInflight<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().inflight = None;
    }
}
enum ScriptLoad {
    Reused(Arc<Prepared>),
    Compiled(String, Arc<Prepared>),
}

fn read_disk_cache(path: &Path) -> Option<CachedPlayer> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_CACHE_BYTES as u64 {
        return None;
    }
    let raw = fs::read(path).ok()?;
    let bytes = if raw.starts_with(b"\x1f\x8b") {
        let mut out = Vec::new();
        GzDecoder::new(raw.as_slice())
            .take((MAX_CACHE_UNPACKED + 1) as u64)
            .read_to_end(&mut out)
            .ok()?;
        if out.len() > MAX_CACHE_UNPACKED {
            return None;
        }
        out
    } else {
        raw
    };
    let cached: CachedPlayer = serde_json::from_slice(&bytes).ok()?;
    if cached.id.is_empty() || cached.chunks.is_empty() {
        return None;
    }
    Some(cached)
}

fn disk_cache_fresh(path: &Path) -> bool {
    fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age < DISK_VERIFY_TTL)
}

fn write_disk_cache(path: &Path, entry: &CachedPlayer) {
    let Ok(json) = serde_json::to_vec(entry) else {
        return;
    };
    if json.len() > MAX_CACHE_UNPACKED {
        log::warn!("YouTube: кэш плеера слишком большой ({} КБ)", json.len() >> 10);
        return;
    }
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    if enc.write_all(&json).is_err() {
        return;
    }
    let Ok(compressed) = enc.finish() else {
        return;
    };
    if compressed.len() > MAX_CACHE_BYTES {
        log::warn!(
            "YouTube: сжатый кэш плеера слишком большой ({} КБ)",
            compressed.len() >> 10
        );
        return;
    }
    match save(path, &compressed) {
        Ok(()) => log::info!(
            "YouTube: кэш плеера {} сохранён ({} КБ)",
            entry.id,
            compressed.len() >> 10
        ),
        Err(error) => log::warn!("YouTube: не сохранить кэш плеера: {error}"),
    }
}

fn spawn_from_chunks(
    timestamp: u32,
    chunks: Vec<String>,
) -> tokio::sync::oneshot::Receiver<Result<Prepared, String>> {
    let (send, result) = tokio::sync::oneshot::channel();
    if std::thread::Builder::new()
        .name("youtube-script-cache".into())
        .stack_size(3 << 20)
        .spawn(move || {
            let _ = send.send(Prepared::from_chunks(timestamp, &chunks));
        })
        .is_err()
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(Err("Не запустить движок плеера".into()));
        return rx;
    }
    result
}

impl Scripts {
    pub fn with_cache(path: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(ScriptCache {
            cache_path: Some(path),
            ..ScriptCache::default()
        })))
    }

    #[cfg(test)]
    pub fn cached(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .current
            .as_ref()
            .is_some_and(|(at, _, _, _)| at.elapsed() < Duration::from_secs(1800))
    }

    pub fn has_warm_cache(&self) -> bool {
        let state = self.0.lock().unwrap();
        if state
            .current
            .as_ref()
            .is_some_and(|(at, _, _, _)| at.elapsed() < Duration::from_secs(1800))
        {
            return true;
        }
        state.cache_path.as_deref().is_some_and(|path| {
            fs::metadata(path)
                .ok()
                .is_some_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_CACHE_BYTES as u64)
        })
    }

    pub fn invalidate(&self) {
        let path = {
            let mut state = self.0.lock().unwrap();
            state.current.take();
            state.failed.take();
            state.cache_path.clone()
        };
        if let Some(path) = path {
            let _ = fs::remove_file(path);
        }
    }

    pub async fn evict_if_idle(&self, idle: Duration) {
        let mut state = self.0.lock().unwrap();
        if state.inflight.is_none()
            && state
                .current
                .as_ref()
                .is_some_and(|(_, used, _, _)| used.elapsed() >= idle)
        {
            state.current.take();
        }
    }
    fn spawn_load<F, Fut>(
        &self,
        state: &mut ScriptCache,
        loader: F,
    ) -> tokio::sync::watch::Receiver<Option<Result<Arc<Prepared>, String>>>
    where
        F: FnOnce(Arc<Mutex<ScriptCache>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<ScriptLoad, (bool, String)>> + Send + 'static,
    {
        if let Some(rx) = &state.inflight {
            return rx.clone();
        }
        let (tx, rx) = tokio::sync::watch::channel(None);
        state.inflight = Some(rx.clone());
        let shared = self.0.clone();
        tokio::spawn(async move {
            let guard = ClearInflight(&shared);
            let outcome = loader(shared.clone()).await;
            let result = {
                let mut state = shared.lock().unwrap();
                state.inflight = None;
                match outcome {
                    Ok(ScriptLoad::Reused(prepared)) => {
                        state.failed = None;
                        Ok(prepared)
                    }
                    Ok(ScriptLoad::Compiled(id, prepared)) => {
                        let now = Instant::now();
                        state.current = Some((now, now, id, prepared.clone()));
                        state.failed = None;
                        Ok(prepared)
                    }
                    Err((deterministic, message)) => {
                        if deterministic {
                            state.failed = Some((Instant::now(), message.clone()));
                        }
                        Err(message)
                    }
                }
            };
            std::mem::forget(guard);
            let _ = tx.send(Some(result));
        });
        rx
    }
    async fn load_or_wait<F, Fut>(&self, loader: F) -> Result<Arc<Prepared>, String>
    where
        F: FnOnce(Arc<Mutex<ScriptCache>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<ScriptLoad, (bool, String)>> + Send + 'static,
    {
        let mut rx = {
            let mut state = self.0.lock().unwrap();
            if let Some((at, used, _, prepared)) = state.current.as_mut()
                && at.elapsed() < Duration::from_secs(1800)
            {
                *used = Instant::now();
                return Ok(prepared.clone());
            }
            if let Some((at, error)) = &state.failed
                && at.elapsed() < Duration::from_secs(120)
            {
                return Err(error.clone());
            }
            self.spawn_load(&mut state, loader)
        };
        rx.wait_for(Option::is_some)
            .await
            .map_err(|_| "Движок плеера завершился".to_owned())?
            .clone()
            .unwrap()
    }
    pub fn warm_background(&self, client: Client) {
        let mut state = self.0.lock().unwrap();
        if state
            .current
            .as_ref()
            .is_some_and(|(at, _, _, _)| at.elapsed() < Duration::from_secs(1800))
            || state
                .failed
                .as_ref()
                .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(120))
            || state.inflight.is_some()
        {
            return;
        }
        let cache_path = state.cache_path.clone();
        let _ = self.spawn_load(&mut state, move |shared| async move {
            load_script(&client, &shared, cache_path, true).await
        });
    }
    pub async fn get(&self, client: &Client) -> Result<Arc<Prepared>, String> {
        let (client, cache_path) = {
            let state = self.0.lock().unwrap();
            (client.clone(), state.cache_path.clone())
        };
        let scripts = self.clone();
        let refresh_client = client.clone();
        let check_path = cache_path.clone();
        let prepared = self
            .load_or_wait(move |shared| async move {
                load_script(&client, &shared, cache_path, false).await
            })
            .await?;
        // If we served immediately from an older disk cache, verify iframe_api in background.
        if let Some(path) = check_path
            && !disk_cache_fresh(&path)
        {
            let mut state = scripts.0.lock().unwrap();
            if state.inflight.is_none() {
                let _ = scripts.spawn_load(&mut state, move |shared| async move {
                    load_script(&refresh_client, &shared, Some(path), true).await
                });
            }
        }
        Ok(prepared)
    }
}
async fn load_script(
    client: &Client,
    shared: &Arc<Mutex<ScriptCache>>,
    cache_path: Option<PathBuf>,
    verify_remote: bool,
) -> Result<ScriptLoad, (bool, String)> {
    // Fast path for live playback: if a valid disk cache exists and we don't need
    // to force a remote check, restore the tiny solver chunks in ~0.2s with zero network wait.
    if !verify_remote
        && let Some(path) = cache_path.as_deref()
        && let Some(cached) = read_disk_cache(path)
    {
        let started = Instant::now();
        match spawn_from_chunks(cached.timestamp, cached.chunks)
            .await
            .map_err(|_| "Движок плеера завершился".to_owned())
            .and_then(|r| r)
        {
            Ok(prepared) => {
                log::info!(
                    "YouTube: плеер {} загружен из кэша за {:.2} с",
                    cached.id,
                    started.elapsed().as_secs_f64()
                );
                return Ok(ScriptLoad::Compiled(cached.id, Arc::new(prepared)));
            }
            Err(error) => {
                log::debug!("YouTube: кэш плеера повреждён ({error}), пересобираю");
                let _ = fs::remove_file(path);
            }
        }
    }
    let iframe = fetch(client, "https://www.youtube.com/iframe_api", WEB_AGENT, 1 << 20)
        .await
        .map_err(|e| (false, e))?;
    let id = Regex::new(r"player\\?/([0-9A-Za-z_-]{8})\\?/")
        .unwrap()
        .captures(&iframe)
        .map(|capture| capture[1].to_owned())
        .ok_or_else(|| (false, "Не найдена версия плеера YouTube".to_owned()))?;
    {
        let mut state = shared.lock().unwrap();
        if let Some((at, used, old, prepared)) = state.current.as_mut()
            && *old == id
        {
            let now = Instant::now();
            *at = now;
            *used = now;
            if let Some(path) = cache_path.as_deref()
                && let Ok(file) = fs::OpenOptions::new().write(true).open(path)
            {
                let _ = file.set_modified(std::time::SystemTime::now());
            }
            return Ok(ScriptLoad::Reused(prepared.clone()));
        }
        state.current.take();
    }
    // Check if disk cache already holds this exact player ID (e.g. after reboot or idle eviction).
    if let Some(path) = cache_path.as_deref()
        && let Some(cached) = read_disk_cache(path)
        && cached.id == id
    {
        let started = Instant::now();
        match spawn_from_chunks(cached.timestamp, cached.chunks)
            .await
            .map_err(|_| "Движок плеера завершился".to_owned())
            .and_then(|r| r)
        {
            Ok(prepared) => {
                if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
                    let _ = file.set_modified(std::time::SystemTime::now());
                }
                log::info!(
                    "YouTube: плеер {id} загружен из кэша за {:.2} с",
                    started.elapsed().as_secs_f64()
                );
                return Ok(ScriptLoad::Compiled(id, Arc::new(prepared)));
            }
            Err(error) => {
                log::debug!("YouTube: кэш плеера {id} повреждён ({error}), пересобираю");
                let _ = fs::remove_file(path);
            }
        }
    }
    let tv_url = format!("https://www.youtube.com/s/player/{id}/tv-player-ias.vflset/tv-player-ias.js");
    let source = match fetch(client, &tv_url, super::remote::TV_AGENT, 8 << 20).await {
        Ok(source) => source,
        Err(_) => {
            let web_url = format!("https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js");
            fetch(client, &web_url, WEB_AGENT, 8 << 20)
                .await
                .map_err(|e| (false, e))?
        }
    };
    let started = Instant::now();
    let (send, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("youtube-script".into())
        .stack_size(8 << 20)
        .spawn(move || {
            let parsed = Prepared::extract_chunks(source).and_then(|(ts, chunks)| {
                let prepared = Prepared::from_chunks(ts, &chunks)?;
                Ok((ts, chunks, prepared))
            });
            let _ = send.send(parsed);
        })
        .map_err(|_| (false, "Не запустить движок плеера".to_owned()))?;
    let (timestamp, chunks, prepared) = match result
        .await
        .map_err(|_| "Движок плеера завершился".to_owned())
        .and_then(|result| result)
    {
        Ok((ts, chunks, prepared)) => (ts, chunks, Arc::new(prepared)),
        Err(error) => {
            log::debug!("YouTube: {error}");
            #[cfg(test)]
            eprintln!("Player parser: {error}");
            return Err((true, "Не разобрать текущий плеер YouTube".to_owned()));
        }
    };
    if let Some(path) = cache_path.as_deref() {
        write_disk_cache(
            path,
            &CachedPlayer {
                id: id.clone(),
                timestamp,
                chunks,
            },
        );
    }
    log::info!(
        "YouTube: плеер {id} готов за {:.2} с",
        started.elapsed().as_secs_f64()
    );
    Ok(ScriptLoad::Compiled(id, prepared))
}
async fn fetch(client: &Client, url: &str, agent: &str, limit: usize) -> Result<String, String> {
    let resp = client
        .get(url)
        .header("User-Agent", agent)
        .send()
        .await
        .map_err(|_| "Не скачать плеер YouTube")?;
    String::from_utf8(bytes(resp, limit).await?)
        .map_err(|_| "Неверная кодировка плеера YouTube".into())
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

// Pick safe boundaries for the minified wrapper. Each slice still goes through the JS parser.
fn boundaries(bytes: &[u8]) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut depth = 0i32;
    let mut prev = 0;
    let mut word = &bytes[..0];
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        i += 1;
        match byte {
            b if b.is_ascii_whitespace() => continue,
            b'/' if bytes.get(i) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i) == Some(&b'*') => {
                i = bytes[i + 1..]
                    .windows(2)
                    .position(|part| part == b"*/")
                    .map_or(bytes.len(), |n| i + n + 3);
                continue;
            }
            b'\'' | b'"' | 0x60 => {
                while let Some(&next) = bytes.get(i) {
                    i += 1;
                    if next == byte {
                        break;
                    }
                    if next == b'\\' {
                        i += 1;
                    }
                }
            }
            b'/' if prev == 0
                || b"(,=:[!&|?{};+-*%<>~^".contains(&prev)
                || matches!(
                    word,
                    b"return"
                        | b"typeof"
                        | b"case"
                        | b"do"
                        | b"else"
                        | b"in"
                        | b"of"
                        | b"new"
                        | b"delete"
                        | b"void"
                        | b"throw"
                        | b"instanceof"
                        | b"yield"
                        | b"await"
                ) =>
            {
                let mut in_class = false;
                while let Some(&next) = bytes.get(i) {
                    i += 1;
                    match next {
                        b'\\' => i += 1,
                        b'[' => in_class = true,
                        b']' => in_class = false,
                        b'/' if !in_class => break,
                        b'\n' => break,
                        _ => {}
                    }
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth == 0 => {
                let tail = &bytes[i..];
                let k = tail
                    .iter()
                    .position(|b| !b.is_ascii_whitespace())
                    .unwrap_or(tail.len());
                let tail = &tail[k..];
                if ![&b"else"[..], b"catch", b"finally", b"while"]
                    .iter()
                    .any(|w| {
                        tail.starts_with(w) && !tail.get(w.len()).is_some_and(|b| word_byte(*b))
                    })
                {
                    cuts.push(i);
                }
            }
            b if word_byte(b) => {
                let start = i - 1;
                while bytes.get(i).is_some_and(|b| word_byte(*b)) {
                    i += 1;
                }
                word = &bytes[start..i];
                prev = bytes[i - 1];
                continue;
            }
            _ => {}
        }
        prev = byte;
        word = &bytes[..0];
    }
    if cuts.last() != Some(&bytes.len()) {
        cuts.push(bytes.len());
    }
    cuts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    const FIXTURE: &str = r#"(function(){
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

    #[tokio::test]
    async fn single_flight_dedup_and_cancel_safety() {
        let scripts = Scripts::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        let spawn_call = |scripts: Scripts, calls: Arc<AtomicUsize>, entered: Arc<Notify>, release: Arc<Notify>| {
            tokio::spawn(async move {
                scripts
                    .load_or_wait(move |_shared| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                        release.notified().await;
                        let prepared = Arc::new(Prepared::new(FIXTURE.to_owned()).map_err(|e| (true, e))?);
                        Ok(ScriptLoad::Compiled("test1234".into(), prepared))
                    })
                    .await
            })
        };

        let task1 = spawn_call(scripts.clone(), calls.clone(), entered.clone(), release.clone());
        entered.notified().await;
        let task2 = spawn_call(scripts.clone(), calls.clone(), entered.clone(), release.clone());
        tokio::task::yield_now().await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task1.abort();
        let _ = task1.await;

        release.notify_one();
        let res = task2.await.unwrap().unwrap();
        assert_eq!(res.timestamp, 12345);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(scripts.cached());

        // Subsequent call hits fresh cache instead of spawning again.
        let calls_before = calls.load(Ordering::SeqCst);
        let cached = spawn_call(scripts.clone(), calls.clone(), entered, release).await.unwrap().unwrap();
        assert_eq!(cached.timestamp, 12345);
        assert_eq!(calls.load(Ordering::SeqCst), calls_before);
    }

    #[tokio::test]
    async fn disk_cache_reloads_solver_without_reparsing_or_network() {
        let dir = std::env::temp_dir().join(format!("jarvis-yt-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache_file = dir.join("player-cache.json");

        let (ts, chunks) = Prepared::extract_chunks(FIXTURE.to_owned()).unwrap();
        assert_eq!(ts, 12345);
        write_disk_cache(
            &cache_file,
            &CachedPlayer {
                id: "abcd1234".into(),
                timestamp: ts,
                chunks,
            },
        );

        let scripts = Scripts::with_cache(cache_file.clone());
        assert!(!scripts.cached());
        assert!(scripts.has_warm_cache());

        // Fast path loads straight from disk cache with a dummy client (no network).
        let client = Client::builder().no_proxy().build().unwrap();
        let loaded = scripts.get(&client).await.unwrap();
        assert_eq!(loaded.timestamp, 12345);
        assert_eq!(
            loaded.decode("abc".into(), "xyz".into()).await.unwrap(),
            ("cba".into(), "zyx".into())
        );
        assert!(scripts.cached());

        // Invalidate removes both RAM and disk cache.
        scripts.invalidate();
        assert!(!scripts.cached());
        assert!(!scripts.has_warm_cache());
        assert!(!cache_file.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
