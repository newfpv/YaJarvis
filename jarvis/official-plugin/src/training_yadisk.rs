//! Yandex Disk OAuth 2.0 (Device Code Flow) + REST API client for isolated `app:/` storage.
use crate::{
    storage::{rand_hex, short, user_agent, write_atomic},
    training_records::{KINDS, MAX_CLIP, decode_clip_bytes, valid_name},
};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

const YANDEX_CLIENT_ID: &str = "bd393d368c4e46d2b31e41a5b2622862";
// gitleaks:allow -- public YaJarvis Yandex OAuth device-flow client
const YANDEX_CLIENT_SECRET: &str = "f3cddec0055a4f849ff48966a74e8ff6";
const DEFAULT_OAUTH_BASE: &str = "https://oauth.yandex.ru";
const DEFAULT_DISK_BASE: &str = "https://cloud-api.yandex.net/v1/disk";

const LIST_CACHE_TTL: Duration = Duration::from_secs(60);
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(300);
const CLIP_CACHE_CAP: usize = 8;
const MAX_LIST_PAGES: usize = 5;
const LIST_PAGE_LIMIT: usize = 1000;

#[derive(Clone, Debug)]
pub struct RemoteClip {
    pub kind: String,
    pub name: String,
    pub reviewed: bool,
    pub size: u64,
    pub time: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedAuth {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: i64,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    login: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    total_space: u64,
    #[serde(default)]
    used_space: u64,
}

#[derive(Clone)]
struct LoginFlow {
    code: String,
    url: String,
    expires_at: Instant,
    error: String,
}

struct ListCache {
    fetched_at: Instant,
    clips: Vec<RemoteClip>,
}

#[derive(Default)]
struct ClipCache {
    entries: VecDeque<((String, String), Vec<u8>)>,
}

impl ClipCache {
    fn get(&mut self, kind: &str, name: &str) -> Option<Vec<u8>> {
        let pos = self
            .entries
            .iter()
            .position(|((k, n), _)| k == kind && n == name)?;
        let entry = self.entries.remove(pos)?;
        let bytes = entry.1.clone();
        self.entries.push_back(entry);
        Some(bytes)
    }

    fn insert(&mut self, kind: String, name: String, bytes: Vec<u8>) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|((k, n), _)| k == &kind && n == &name)
        {
            self.entries.remove(pos);
        }
        while self.entries.len() >= CLIP_CACHE_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(((kind, name), bytes));
    }

    fn remove(&mut self, kind: &str, name: &str) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|((k, n), _)| k == kind && n == name)
        {
            self.entries.remove(pos);
        }
    }

    fn rename(&mut self, from_kind: &str, from_name: &str, to_kind: String, to_name: String) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|((k, n), _)| k == from_kind && n == from_name)
        {
            let (_, bytes) = self.entries.remove(pos).unwrap();
            self.insert(to_kind, to_name, bytes);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Default)]
struct State {
    auth: Option<SavedAuth>,
    login: Option<LoginFlow>,
    login_gen: u64,
    login_task: Option<JoinHandle<()>>,
    list_cache: Option<ListCache>,
    clip_cache: ClipCache,
    quota_at: Option<Instant>,
    closed: bool,
}

pub struct YandexDisk {
    config: PathBuf,
    legacy_config: PathBuf,
    oauth_base: String,
    disk_base: String,
    allow_insecure: bool,
    api: reqwest::Client,
    transfer: reqwest::Client,
    state: Mutex<State>,
    cancel: watch::Sender<u64>,
    refresh_lock: tokio::sync::Mutex<()>,
    on_connected: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

fn now_unix() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn valid_transfer_url(url: &Url, allow_insecure: bool) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    if allow_insecure && matches!(host.as_str(), "127.0.0.1" | "localhost") {
        return matches!(url.scheme(), "http" | "https");
    }
    if url.scheme() != "https" || url.port().is_some() {
        return false;
    }
    matches!(host.as_str(), "yandex.net" | "yandex.ru" | "yandex.com")
        || host.ends_with(".yandex.net")
        || host.ends_with(".yandex.ru")
        || host.ends_with(".yandex.com")
}

fn valid_operation_url(url: &Url, disk_base: &str, allow_insecure: bool) -> bool {
    let Ok(base) = Url::parse(disk_base) else {
        return false;
    };
    url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port() == base.port()
        && url.username().is_empty()
        && url.password().is_none()
        && (allow_insecure || url.scheme() == "https")
}

fn build_clients(allow_insecure: bool) -> (reqwest::Client, reqwest::Client) {
    let api = reqwest::Client::builder()
        .user_agent(user_agent())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(25))
        .build()
        .expect("клиент API Яндекс Диска");
    let transfer = reqwest::Client::builder()
        .user_agent(user_agent())
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("Слишком много перенаправлений Яндекс Диска")
            } else if valid_transfer_url(attempt.url(), allow_insecure) {
                attempt.follow()
            } else {
                attempt.error("Недопустимый адрес перенаправления Яндекс Диска")
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("клиент загрузки Яндекс Диска");
    (api, transfer)
}

fn disk_error(status: StatusCode) -> String {
    match status.as_u16() {
        401 => "Токен Яндекс Диска истёк, подключите аккаунт заново".into(),
        403 => "Яндекс Диск не разрешил доступ к папке приложения".into(),
        404 => "Запись на Яндекс Диске не найдена".into(),
        409 => "Папка на Яндекс Диске недоступна".into(),
        413 => "Яндекс Диск отклонил файл по размеру".into(),
        423 | 429 => "Яндекс Диск временно ограничил частоту запросов".into(),
        507 => "На Яндекс Диске закончилось место".into(),
        code => format!("Яндекс Диск ответил {code}"),
    }
}

impl YandexDisk {
    pub fn new(root: &std::path::Path) -> Arc<Self> {
        Self::with_endpoints(
            root.join("yandex-disk.json"),
            root.join("background-cloud.json"),
            DEFAULT_OAUTH_BASE,
            DEFAULT_DISK_BASE,
            false,
        )
    }

    pub(crate) fn with_endpoints(
        config: PathBuf,
        legacy_config: PathBuf,
        oauth_base: &str,
        disk_base: &str,
        allow_insecure: bool,
    ) -> Arc<Self> {
        let auth = fs::read(&config)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SavedAuth>(&bytes).ok())
            .filter(|a| !a.access_token.is_empty());
        let (api, transfer) = build_clients(allow_insecure);
        Arc::new(Self {
            config,
            legacy_config,
            oauth_base: oauth_base.trim_end_matches('/').to_owned(),
            disk_base: disk_base.trim_end_matches('/').to_owned(),
            allow_insecure,
            api,
            transfer,
            state: Mutex::new(State {
                auth,
                ..State::default()
            }),
            cancel: watch::channel(0).0,
            refresh_lock: tokio::sync::Mutex::new(()),
            on_connected: Mutex::new(None),
        })
    }

    pub fn set_on_connected(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self.on_connected.lock().unwrap() = Some(callback);
    }

    pub fn is_connected(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.closed && s.auth.is_some()
    }

    pub fn status(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        if let Some(login) = s.login.as_mut()
            && !login.code.is_empty()
            && Instant::now() >= login.expires_at
        {
            login.code.clear();
            login.url.clear();
            login.error = "Время кода истекло. Нажмите «Подключить Яндекс Диск» ещё раз.".into();
        }
        let account = s.auth.as_ref().map(|a| {
            json!({
                "login": a.login,
                "name": a.display_name,
                "total_mb": a.total_space >> 20,
                "free_mb": a.total_space.saturating_sub(a.used_space) >> 20,
            })
        });
        let login = s.login.as_ref().map(|l| {
            json!({
                "code": l.code,
                "url": l.url,
                "error": l.error,
            })
        });
        json!({
            "connected": s.auth.is_some(),
            "provider": s.auth.as_ref().map(|_| "yandex_disk"),
            "account": account,
            "login": login,
        })
    }

    pub fn close(&self) {
        let task = {
            let mut s = self.state.lock().unwrap();
            s.closed = true;
            s.login_gen = s.login_gen.wrapping_add(1);
            s.login = None;
            s.login_task.take()
        };
        if let Some(task) = task {
            task.abort();
        }
        self.cancel.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub async fn begin_login(self: &Arc<Self>) -> Result<Value, String> {
        {
            let s = self.state.lock().unwrap();
            if s.closed {
                return Err("Плагин выключен".into());
            }
            if s.auth.is_some() {
                drop(s);
                return Ok(self.status());
            }
        }
        let device_id = format!("yajarvis-{}", rand_hex(8));
        let url = format!("{}/device/code", self.oauth_base);
        let response = self
            .api
            .post(&url)
            .form(&[
                ("client_id", YANDEX_CLIENT_ID),
                ("device_id", device_id.as_str()),
                ("device_name", "YaJarvis"),
            ])
            .send()
            .await
            .map_err(|e| format!("Нет связи с Яндекс ID: {}", short(&e)))?;
        if !response.status().is_success() {
            return Err(format!("Яндекс ID ответил {}", response.status().as_u16()));
        }
        let data: Value = response
            .json()
            .await
            .map_err(|e| format!("Неверный ответ Яндекс ID: {}", short(&e)))?;
        let device_code = data["device_code"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Яндекс ID не вернул device_code")?
            .to_owned();
        let user_code = data["user_code"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Яндекс ID не вернул код подтверждения")?
            .to_owned();
        let verification_url = data["verification_url"]
            .as_str()
            .filter(|s| s.starts_with("https://") || self.allow_insecure)
            .unwrap_or("https://ya.ru/device")
            .to_owned();
        let interval = data["interval"].as_u64().unwrap_or(5).clamp(1, 15);
        let expires_in = data["expires_in"].as_u64().unwrap_or(300).clamp(30, 900);
        let expires_at = Instant::now() + Duration::from_secs(expires_in);

        let generation = {
            let mut s = self.state.lock().unwrap();
            if s.closed || s.auth.is_some() {
                drop(s);
                return Ok(self.status());
            }
            if let Some(old) = s.login_task.take() {
                old.abort();
            }
            s.login_gen = s.login_gen.wrapping_add(1);
            s.login = Some(LoginFlow {
                code: user_code,
                url: verification_url,
                expires_at,
                error: String::new(),
            });
            s.login_gen
        };

        let disk = self.clone();
        let task = tokio::spawn(async move {
            disk.poll_login(generation, device_code, device_id, interval, expires_at)
                .await;
        });
        {
            let mut s = self.state.lock().unwrap();
            if s.login_gen == generation && !s.closed {
                s.login_task = Some(task);
            } else {
                task.abort();
            }
        }
        Ok(self.status())
    }

    async fn poll_login(
        self: Arc<Self>,
        generation: u64,
        device_code: String,
        device_id: String,
        mut interval: u64,
        expires_at: Instant,
    ) {
        let token_url = format!("{}/token", self.oauth_base);
        while Instant::now() < expires_at {
            let step = Duration::from_secs(interval);
            let mut cancel = self.cancel.subscribe();
            tokio::select! {
                biased;
                _ = cancel.changed() => return,
                _ = tokio::time::sleep(step) => {}
            }
            {
                let s = self.state.lock().unwrap();
                if s.closed || s.login_gen != generation {
                    return;
                }
            }
            let response = match self
                .api
                .post(&token_url)
                .form(&[
                    ("grant_type", "device_code"),
                    ("code", device_code.as_str()),
                    ("client_id", YANDEX_CLIENT_ID),
                    ("client_secret", YANDEX_CLIENT_SECRET),
                ])
                .send()
                .await
            {
                Ok(r) => r,
                Err(_) => continue,
            };
            let status = response.status();
            let body: Value = match response.json().await {
                Ok(v) => v,
                Err(_) => continue,
            };
            if status.is_success() {
                let Some(access) = body["access_token"].as_str().filter(|s| !s.is_empty()) else {
                    self.set_login_error(generation, "Яндекс ID не вернул токен доступа");
                    return;
                };
                let refresh = body["refresh_token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let expires_in = body["expires_in"].as_i64().unwrap_or(0);
                let expires_at_unix = if expires_in > 0 {
                    now_unix().saturating_add(expires_in)
                } else {
                    0
                };
                let (login, display_name, total_space, used_space) =
                    match self.fetch_disk_info(access).await {
                        Ok(info) => info,
                        Err(err) => {
                            self.set_login_error(generation, &err);
                            return;
                        }
                    };
                if let Err(err) = self.ensure_folders_with_token(access).await {
                    self.set_login_error(generation, &err);
                    return;
                }
                let saved = SavedAuth {
                    access_token: access.to_owned(),
                    refresh_token: refresh,
                    expires_at: expires_at_unix,
                    device_id,
                    login,
                    display_name,
                    total_space,
                    used_space,
                };
                if let Err(err) = self.persist_auth(&saved) {
                    self.set_login_error(generation, &err);
                    return;
                }
                let callback = {
                    let mut s = self.state.lock().unwrap();
                    if s.closed || s.login_gen != generation {
                        return;
                    }
                    s.auth = Some(saved);
                    s.login = None;
                    s.list_cache = None;
                    s.clip_cache.clear();
                    s.quota_at = Some(Instant::now());
                    self.on_connected.lock().unwrap().clone()
                };
                if let Some(cb) = callback {
                    cb();
                }
                return;
            }
            match body["error"].as_str().unwrap_or("") {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval = (interval + 5).min(20);
                    continue;
                }
                "access_denied" => {
                    self.set_login_error(generation, "Вход отменён в Яндекс ID");
                    return;
                }
                "expired_token" | "bad_verification_code" => {
                    self.set_login_error(
                        generation,
                        "Код истёк. Нажмите «Подключить Яндекс Диск» ещё раз.",
                    );
                    return;
                }
                other => {
                    let desc = body["error_description"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(other);
                    self.set_login_error(generation, &format!("Ошибка Яндекс ID: {desc}"));
                    return;
                }
            }
        }
        self.set_login_error(
            generation,
            "Время кода истекло. Нажмите «Подключить Яндекс Диск» ещё раз.",
        );
    }

    fn set_login_error(&self, generation: u64, message: &str) {
        let mut s = self.state.lock().unwrap();
        if s.login_gen != generation {
            return;
        }
        s.login = Some(LoginFlow {
            code: String::new(),
            url: String::new(),
            expires_at: Instant::now(),
            error: message.chars().take(200).collect(),
        });
    }

    fn persist_auth(&self, auth: &SavedAuth) -> Result<(), String> {
        let bytes = serde_json::to_vec(auth).map_err(|e| e.to_string())?;
        if let Some(parent) = self.config.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        write_atomic(&self.config, &bytes, 0o600).map_err(|e| e.to_string())
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        let task = {
            let mut s = self.state.lock().unwrap();
            s.login_gen = s.login_gen.wrapping_add(1);
            s.login = None;
            s.auth = None;
            s.list_cache = None;
            s.clip_cache.clear();
            s.quota_at = None;
            s.login_task.take()
        };
        if let Some(task) = task {
            task.abort();
        }
        self.cancel.send_modify(|n| *n = n.wrapping_add(1));
        for path in [&self.config, &self.legacy_config] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    async fn fetch_disk_info(&self, token: &str) -> Result<(String, String, u64, u64), String> {
        let response = self
            .api
            .get(&self.disk_base)
            .query(&[("fields", "total_space,used_space,user")])
            .header("Authorization", format!("OAuth {token}"))
            .send()
            .await
            .map_err(|e| format!("Нет связи с Яндекс Диском: {}", short(&e)))?;
        if !response.status().is_success() {
            return Err(disk_error(response.status()));
        }
        let info: Value = response
            .json()
            .await
            .map_err(|e| format!("Неверный ответ Яндекс Диска: {}", short(&e)))?;
        let login = info["user"]["login"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let display_name = info["user"]["display_name"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let total_space = info["total_space"].as_u64().unwrap_or(0);
        let used_space = info["used_space"].as_u64().unwrap_or(0);
        Ok((login, display_name, total_space, used_space))
    }

    async fn ensure_folders_with_token(&self, token: &str) -> Result<(), String> {
        let dirs = [
            "app:/clips",
            "app:/clips/pos",
            "app:/clips/false",
            "app:/clips/near",
            "app:/clips/bg",
            "app:/bg",
        ];
        for dir in dirs {
            let response = self
                .api
                .put(format!("{}/resources", self.disk_base))
                .query(&[("path", dir)])
                .header("Authorization", format!("OAuth {token}"))
                .send()
                .await
                .map_err(|e| format!("Нет связи с Яндекс Диском: {}", short(&e)))?;
            let status = response.status();
            if !status.is_success() && status != StatusCode::CONFLICT {
                return Err(disk_error(status));
            }
        }
        Ok(())
    }

    async fn ensure_folders(&self) -> Result<(), String> {
        let token = self.valid_token().await?;
        self.ensure_folders_with_token(&token).await
    }

    async fn valid_token(&self) -> Result<String, String> {
        let (token, needs_refresh) = {
            let s = self.state.lock().unwrap();
            if s.closed {
                return Err("Плагин выключен".into());
            }
            let auth = s
                .auth
                .as_ref()
                .ok_or_else(|| "Яндекс Диск не подключён".to_string())?;
            let needs_refresh = auth.expires_at > 0
                && !auth.refresh_token.is_empty()
                && now_unix() + 300 >= auth.expires_at;
            (auth.access_token.clone(), needs_refresh)
        };
        if needs_refresh {
            return self.refresh_token(&token).await;
        }
        Ok(token)
    }

    async fn refresh_token(&self, expired_token: &str) -> Result<String, String> {
        let _guard = self.refresh_lock.lock().await;
        let current = {
            let s = self.state.lock().unwrap();
            s.auth
                .clone()
                .ok_or_else(|| "Яндекс Диск не подключён".to_string())?
        };
        if current.access_token != expired_token {
            return Ok(current.access_token);
        }
        if current.refresh_token.is_empty() {
            return Err("Токен Яндекс Диска истёк, подключите аккаунт заново".into());
        }
        let response = self
            .api
            .post(format!("{}/token", self.oauth_base))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", current.refresh_token.as_str()),
                ("client_id", YANDEX_CLIENT_ID),
                ("client_secret", YANDEX_CLIENT_SECRET),
            ])
            .send()
            .await
            .map_err(|e| format!("Не обновить токен Яндекс ID: {}", short(&e)))?;
        if !response.status().is_success() {
            return Err("Токен Яндекс Диска истёк, подключите аккаунт заново".into());
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("Неверный ответ Яндекс ID: {}", short(&e)))?;
        let access = body["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Яндекс ID не вернул новый токен")?
            .to_owned();
        let refresh = body["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or(current.refresh_token);
        let expires_in = body["expires_in"].as_i64().unwrap_or(0);
        let expires_at = if expires_in > 0 {
            now_unix().saturating_add(expires_in)
        } else {
            0
        };
        let updated = SavedAuth {
            access_token: access.clone(),
            refresh_token: refresh,
            expires_at,
            ..current
        };
        let _ = self.persist_auth(&updated);
        let mut s = self.state.lock().unwrap();
        if s.auth.is_some() {
            s.auth = Some(updated);
        }
        Ok(access)
    }

    async fn api_request(
        &self,
        method: Method,
        subpath: &str,
        query: &[(&str, &str)],
    ) -> Result<reqwest::Response, String> {
        let mut cancel = self.cancel.subscribe();
        let url = format!("{}{subpath}", self.disk_base);
        let mut token = self.valid_token().await?;
        for attempt in 0..2 {
            let req = self
                .api
                .request(method.clone(), &url)
                .query(query)
                .header("Authorization", format!("OAuth {token}"))
                .send();
            let response = tokio::select! {
                biased;
                _ = cancel.changed() => return Err("Подключение к Яндекс Диску изменилось".into()),
                res = req => res.map_err(|e| format!("Яндекс Диск: {}", short(&e)))?,
            };
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                token = self.refresh_token(&token).await?;
                continue;
            }
            return Ok(response);
        }
        Err("Токен Яндекс Диска истёк, подключите аккаунт заново".into())
    }

    async fn upload_bytes(&self, disk_path: &str, bytes: Vec<u8>, content_type: &str) -> Result<(), String> {
        let mut cancel = self.cancel.subscribe();
        let mut href = None;
        for attempt in 0..2 {
            let resp = self
                .api_request(
                    Method::GET,
                    "/resources/upload",
                    &[("path", disk_path), ("overwrite", "true")],
                )
                .await?;
            if resp.status() == StatusCode::CONFLICT && attempt == 0 {
                self.ensure_folders().await?;
                continue;
            }
            if !resp.status().is_success() {
                return Err(disk_error(resp.status()));
            }
            let body: Value = resp
                .json()
                .await
                .map_err(|e| format!("Яндекс Диск: {}", short(&e)))?;
            let raw = body["href"]
                .as_str()
                .ok_or("Яндекс Диск не вернул ссылку загрузки")?;
            let parsed = Url::parse(raw).map_err(|_| "Неверная ссылка загрузки Яндекс Диска")?;
            if !valid_transfer_url(&parsed, self.allow_insecure) {
                return Err("Недопустимый адрес загрузки Яндекс Диска".into());
            }
            href = Some(parsed);
            break;
        }
        let url = href.ok_or("Не удалось получить ссылку загрузки")?;
        let put = self
            .transfer
            .put(url)
            .header("Content-Type", content_type)
            .body(bytes)
            .send();
        let resp = tokio::select! {
            biased;
            _ = cancel.changed() => return Err("Подключение к Яндекс Диску изменилось".into()),
            res = put => res.map_err(|e| format!("Яндекс Диск: {}", short(&e)))?,
        };
        if !resp.status().is_success() {
            return Err(disk_error(resp.status()));
        }
        Ok(())
    }

    pub async fn upload_bg(&self, archive_name: &str, zip: Vec<u8>) -> Result<(), String> {
        let path = format!("app:/bg/{archive_name}");
        self.upload_bytes(&path, zip, "application/zip").await
    }

    pub async fn upload_clip(
        &self,
        kind: &str,
        name: &str,
        bytes: Vec<u8>,
        time: String,
    ) -> Result<(), String> {
        if !KINDS.contains(&kind) || !valid_name(name) {
            return Err("Неверное имя записи".into());
        }
        let size = bytes.len() as u64;
        let path = format!("app:/clips/{kind}/{name}");
        self.upload_bytes(&path, bytes, "application/octet-stream")
            .await?;
        let mut s = self.state.lock().unwrap();
        if let Some(cache) = s.list_cache.as_mut() {
            cache
                .clips
                .retain(|c| !(c.kind == kind && c.name == name));
            cache.clips.push(RemoteClip {
                kind: kind.to_owned(),
                name: name.to_owned(),
                reviewed: name.ends_with("_r.wav"),
                size,
                time,
            });
        }
        Ok(())
    }

    async fn list_kind(&self, kind: &str) -> Result<Vec<RemoteClip>, String> {
        let path = format!("app:/clips/{kind}");
        let mut out = Vec::new();
        for page in 0..MAX_LIST_PAGES {
            let offset = (page * LIST_PAGE_LIMIT).to_string();
            let limit = LIST_PAGE_LIMIT.to_string();
            let resp = self
                .api_request(
                    Method::GET,
                    "/resources",
                    &[
                        ("path", path.as_str()),
                        (
                            "fields",
                            "_embedded.items.name,_embedded.items.size,_embedded.items.modified,_embedded.total",
                        ),
                        ("limit", limit.as_str()),
                        ("offset", offset.as_str()),
                    ],
                )
                .await?;
            if resp.status() == StatusCode::NOT_FOUND {
                break;
            }
            if !resp.status().is_success() {
                return Err(disk_error(resp.status()));
            }
            let body: Value = resp
                .json()
                .await
                .map_err(|e| format!("Яндекс Диск: {}", short(&e)))?;
            let items = body["_embedded"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let count = items.len();
            for item in items {
                let Some(name) = item["name"].as_str() else {
                    continue;
                };
                if !valid_name(name) {
                    continue;
                }
                let size = item["size"].as_u64().unwrap_or(0);
                if size > MAX_CLIP {
                    continue;
                }
                let time = item["modified"].as_str().unwrap_or_default().to_owned();
                out.push(RemoteClip {
                    kind: kind.to_owned(),
                    reviewed: name.ends_with("_r.wav"),
                    name: name.to_owned(),
                    size,
                    time,
                });
            }
            let total = body["_embedded"]["total"].as_u64().unwrap_or(0) as usize;
            if count < LIST_PAGE_LIMIT || (page + 1) * LIST_PAGE_LIMIT >= total {
                break;
            }
        }
        Ok(out)
    }

    async fn maybe_refresh_quota(&self) {
        let should = {
            let s = self.state.lock().unwrap();
            s.auth.is_some()
                && s.quota_at
                    .is_none_or(|at| at.elapsed() >= QUOTA_CACHE_TTL)
        };
        if !should {
            return;
        }
        let Ok(token) = self.valid_token().await else {
            return;
        };
        if let Ok((login, display_name, total_space, used_space)) =
            self.fetch_disk_info(&token).await
        {
            let updated = {
                let mut s = self.state.lock().unwrap();
                s.quota_at = Some(Instant::now());
                if let Some(auth) = s.auth.as_mut() {
                    if !login.is_empty() {
                        auth.login = login;
                    }
                    if !display_name.is_empty() {
                        auth.display_name = display_name;
                    }
                    auth.total_space = total_space;
                    auth.used_space = used_space;
                    Some(auth.clone())
                } else {
                    None
                }
            };
            if let Some(auth) = updated {
                let _ = self.persist_auth(&auth);
            }
        }
    }

    pub async fn list_clips(&self, force_refresh: bool) -> Result<Vec<RemoteClip>, String> {
        if !self.is_connected() {
            return Ok(Vec::new());
        }
        if !force_refresh {
            let s = self.state.lock().unwrap();
            if let Some(cache) = s.list_cache.as_ref()
                && cache.fetched_at.elapsed() < LIST_CACHE_TTL
            {
                return Ok(cache.clips.clone());
            }
        }
        let (pos, fls, near, bg) = tokio::join!(
            self.list_kind("pos"),
            self.list_kind("false"),
            self.list_kind("near"),
            self.list_kind("bg"),
        );
        let mut clips = Vec::new();
        clips.extend(pos?);
        clips.extend(fls?);
        clips.extend(near?);
        clips.extend(bg?);
        {
            let mut s = self.state.lock().unwrap();
            if s.auth.is_some() {
                s.list_cache = Some(ListCache {
                    fetched_at: Instant::now(),
                    clips: clips.clone(),
                });
            }
        }
        self.maybe_refresh_quota().await;
        Ok(clips)
    }

    pub fn has_cached_clip(&self, kind: &str, name: &str) -> bool {
        let s = self.state.lock().unwrap();
        s.list_cache
            .as_ref()
            .is_some_and(|c| c.clips.iter().any(|clip| clip.kind == kind && clip.name == name))
    }

    pub async fn download_clip(&self, kind: &str, name: &str) -> Result<Vec<u8>, String> {
        if !KINDS.contains(&kind) || !valid_name(name) {
            return Err("Неверное имя записи".into());
        }
        {
            let mut s = self.state.lock().unwrap();
            if let Some(cached) = s.clip_cache.get(kind, name) {
                return Ok(cached);
            }
        }
        let path = format!("app:/clips/{kind}/{name}");
        let resp = self
            .api_request(Method::GET, "/resources/download", &[("path", path.as_str())])
            .await?;
        if !resp.status().is_success() {
            return Err(disk_error(resp.status()));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Яндекс Диск: {}", short(&e)))?;
        let raw = body["href"]
            .as_str()
            .ok_or("Яндекс Диск не вернул ссылку скачивания")?;
        let url = Url::parse(raw).map_err(|_| "Неверная ссылка скачивания Яндекс Диска")?;
        if !valid_transfer_url(&url, self.allow_insecure) {
            return Err("Недопустимый адрес скачивания Яндекс Диска".into());
        }
        let mut cancel = self.cancel.subscribe();
        let req = self.transfer.get(url).send();
        let mut file_resp = tokio::select! {
            biased;
            _ = cancel.changed() => return Err("Подключение к Яндекс Диску изменилось".into()),
            res = req => res.map_err(|e| format!("Яндекс Диск: {}", short(&e)))?,
        };
        if !file_resp.status().is_success() {
            return Err(disk_error(file_resp.status()));
        }
        let mut data = Vec::new();
        while let Some(chunk) = file_resp
            .chunk()
            .await
            .map_err(|e| format!("Яндекс Диск: {}", short(&e)))?
        {
            if (data.len() + chunk.len()) as u64 > MAX_CLIP {
                return Err("Запись слишком большая".into());
            }
            data.extend_from_slice(&chunk);
        }
        let wav = decode_clip_bytes(data)?;
        let mut s = self.state.lock().unwrap();
        if s.auth.is_some() {
            s.clip_cache.insert(kind.to_owned(), name.to_owned(), wav.clone());
        }
        Ok(wav)
    }

    pub async fn move_clip(&self, kind: &str, name: &str, to: &str) -> Result<(), String> {
        if !KINDS.contains(&kind) || !valid_name(name) {
            return Err("Неверное имя записи".into());
        }
        let from_path = format!("app:/clips/{kind}/{name}");
        if to == "delete" {
            let resp = self
                .api_request(
                    Method::DELETE,
                    "/resources",
                    &[("path", from_path.as_str()), ("permanently", "true")],
                )
                .await?;
            if !resp.status().is_success() && resp.status() != StatusCode::NOT_FOUND {
                return Err(disk_error(resp.status()));
            }
            let mut s = self.state.lock().unwrap();
            s.clip_cache.remove(kind, name);
            if let Some(cache) = s.list_cache.as_mut() {
                cache.clips.retain(|c| !(c.kind == kind && c.name == name));
            }
            return Ok(());
        }
        if !KINDS.contains(&to) {
            return Err("Неверное имя записи".into());
        }
        let stem = name.trim_end_matches(".wav").trim_end_matches("_r");
        let new_name = format!("{stem}_r.wav");
        if !valid_name(&new_name) {
            return Err("Неверное имя записи".into());
        }
        if kind == to && name == new_name {
            return Ok(());
        }
        let to_path = format!("app:/clips/{to}/{new_name}");
        for attempt in 0..2 {
            let resp = self
                .api_request(
                    Method::POST,
                    "/resources/move",
                    &[
                        ("from", from_path.as_str()),
                        ("path", to_path.as_str()),
                        ("overwrite", "true"),
                    ],
                )
                .await?;
            if resp.status() == StatusCode::CONFLICT && attempt == 0 {
                self.ensure_folders().await?;
                continue;
            }
            if !resp.status().is_success() {
                return Err(disk_error(resp.status()));
            }
            break;
        }
        let mut s = self.state.lock().unwrap();
        s.clip_cache
            .rename(kind, name, to.to_owned(), new_name.clone());
        if let Some(cache) = s.list_cache.as_mut() {
            if let Some(clip) = cache
                .clips
                .iter_mut()
                .find(|c| c.kind == kind && c.name == name)
            {
                clip.kind = to.to_owned();
                clip.name = new_name;
                clip.reviewed = true;
            }
        }
        Ok(())
    }

    pub async fn clear_clips(&self) -> Result<(), String> {
        if !self.is_connected() {
            return Ok(());
        }
        let resp = self
            .api_request(
                Method::DELETE,
                "/resources",
                &[("path", "app:/clips"), ("permanently", "true")],
            )
            .await?;
        let status = resp.status();
        if status == StatusCode::ACCEPTED {
            if let Ok(body) = resp.json::<Value>().await
                && let Some(href) = body["href"].as_str()
                && let Ok(op_url) = Url::parse(href)
                && valid_operation_url(&op_url, &self.disk_base, self.allow_insecure)
            {
                let token = self.valid_token().await?;
                for _ in 0..30 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let Ok(poll) = self
                        .api
                        .get(op_url.clone())
                        .header("Authorization", format!("OAuth {token}"))
                        .send()
                        .await
                    else {
                        break;
                    };
                    if let Ok(op) = poll.json::<Value>().await
                        && matches!(op["status"].as_str(), Some("success" | "failed"))
                    {
                        break;
                    }
                }
            }
        } else if !status.is_success() && status != StatusCode::NOT_FOUND {
            return Err(disk_error(status));
        }
        let _ = self.ensure_folders().await;
        let mut s = self.state.lock().unwrap();
        s.clip_cache.clear();
        s.list_cache = Some(ListCache {
            fetched_at: Instant::now(),
            clips: Vec::new(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::training_records::pcm_wav;
    use axum::{
        Router,
        body::Bytes,
        extract::{Path as AxumPath, State},
        http::{HeaderMap, Uri, header},
        routing::{get, post, put},
    };
    use std::collections::BTreeMap;

    fn qmap(uri: &Uri) -> BTreeMap<String, String> {
        reqwest::Url::parse(&format!("http://x{uri}"))
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default()
    }

    fn jresp(
        status: StatusCode,
        value: Value,
    ) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
        (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            value.to_string(),
        )
    }

    #[derive(Default)]
    struct MockServer {
        poll_count: u32,
        token: String,
        files: BTreeMap<String, Vec<u8>>,
        dirs: Vec<String>,
        fail_first_401: bool,
    }

    async fn spawn_mock() -> (String, Arc<Mutex<MockServer>>, JoinHandle<()>) {
        let state = Arc::new(Mutex::new(MockServer {
            token: "tok-1".into(),
            ..Default::default()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let base_for_routes = base.clone();

        let app = Router::new()
            .route(
                "/device/code",
                post(|| async {
                    jresp(
                        StatusCode::OK,
                        json!({
                            "device_code": "dev-123",
                            "user_code": "1234-5678",
                            "verification_url": "https://ya.ru/device",
                            "interval": 1,
                            "expires_in": 60
                        }),
                    )
                }),
            )
            .route(
                "/token",
                post(
                    |State(st): State<Arc<Mutex<MockServer>>>, body: String| async move {
                        let mut s = st.lock().unwrap();
                        if body.contains("grant_type=refresh_token") {
                            s.token = "tok-2".into();
                            return jresp(
                                StatusCode::OK,
                                json!({
                                    "access_token": "tok-2",
                                    "refresh_token": "ref-2",
                                    "expires_in": 3600
                                }),
                            );
                        }
                        s.poll_count += 1;
                        if s.poll_count < 2 {
                            return jresp(
                                StatusCode::BAD_REQUEST,
                                json!({"error": "authorization_pending"}),
                            );
                        }
                        jresp(
                            StatusCode::OK,
                            json!({
                                "access_token": s.token,
                                "refresh_token": "ref-1",
                                "expires_in": 3600
                            }),
                        )
                    },
                ),
            )
            .route(
                "/v1/disk",
                get(
                    |State(st): State<Arc<Mutex<MockServer>>>, headers: HeaderMap| async move {
                        let s = st.lock().unwrap();
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if auth != format!("OAuth {}", s.token) {
                            return jresp(StatusCode::UNAUTHORIZED, json!({}));
                        }
                        jresp(
                            StatusCode::OK,
                            json!({
                                "total_space": 10u64 << 30,
                                "used_space": 2u64 << 30,
                                "user": {"login": "tester", "display_name": "Тест Юзер"}
                            }),
                        )
                    },
                ),
            )
            .route(
                "/v1/disk/resources",
                put(
                    |State(st): State<Arc<Mutex<MockServer>>>, uri: Uri| async move {
                        let q = qmap(&uri);
                        let mut s = st.lock().unwrap();
                        if let Some(p) = q.get("path") {
                            s.dirs.push(p.clone());
                        }
                        StatusCode::CREATED
                    },
                )
                .get(
                    |State(st): State<Arc<Mutex<MockServer>>>, headers: HeaderMap, uri: Uri| async move {
                        let q = qmap(&uri);
                        let mut s = st.lock().unwrap();
                        if s.fail_first_401 {
                            s.fail_first_401 = false;
                            return jresp(StatusCode::UNAUTHORIZED, json!({}));
                        }
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if auth != format!("OAuth {}", s.token) {
                            return jresp(StatusCode::UNAUTHORIZED, json!({}));
                        }
                        let prefix = format!("{}/", q.get("path").cloned().unwrap_or_default());
                        let items: Vec<Value> = s
                            .files
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
                        jresp(
                            StatusCode::OK,
                            json!({"_embedded": {"items": items, "total": total}}),
                        )
                    },
                )
                .delete(
                    |State(st): State<Arc<Mutex<MockServer>>>, uri: Uri| async move {
                        let q = qmap(&uri);
                        let mut s = st.lock().unwrap();
                        let target = q.get("path").cloned().unwrap_or_default();
                        if target == "app:/clips" {
                            s.files.retain(|k, _| !k.starts_with("app:/clips/"));
                        } else {
                            s.files.remove(&target);
                        }
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1/disk/resources/upload",
                get({
                    let base = base_for_routes.clone();
                    move |uri: Uri| {
                        let base = base.clone();
                        async move {
                            let q = qmap(&uri);
                            let path = q.get("path").cloned().unwrap_or_default();
                            let encoded = path.trim_start_matches("app:/").replace('/', "__");
                            jresp(
                                StatusCode::OK,
                                json!({"href": format!("{base}/transfer/{encoded}")}),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1/disk/resources/download",
                get({
                    let base = base_for_routes.clone();
                    move |uri: Uri| {
                        let base = base.clone();
                        async move {
                            let q = qmap(&uri);
                            let path = q.get("path").cloned().unwrap_or_default();
                            let encoded = path.trim_start_matches("app:/").replace('/', "__");
                            jresp(
                                StatusCode::OK,
                                json!({"href": format!("{base}/transfer/{encoded}")}),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1/disk/resources/move",
                post(
                    |State(st): State<Arc<Mutex<MockServer>>>, uri: Uri| async move {
                        let q = qmap(&uri);
                        let mut s = st.lock().unwrap();
                        let from = q.get("from").cloned().unwrap_or_default();
                        let to = q.get("path").cloned().unwrap_or_default();
                        if let Some(data) = s.files.remove(&from) {
                            s.files.insert(to, data);
                            StatusCode::CREATED
                        } else {
                            StatusCode::NOT_FOUND
                        }
                    },
                ),
            )
            .route(
                "/transfer/{key}",
                put(
                    |State(st): State<Arc<Mutex<MockServer>>>,
                     AxumPath(key): AxumPath<String>,
                     data: Bytes| async move {
                        let path = format!("app:/{}", key.replace("__", "/"));
                        st.lock().unwrap().files.insert(path, data.to_vec());
                        StatusCode::CREATED
                    },
                )
                .get(
                    |State(st): State<Arc<Mutex<MockServer>>>,
                     AxumPath(key): AxumPath<String>| async move {
                        let path = format!("app:/{}", key.replace("__", "/"));
                        match st.lock().unwrap().files.get(&path).cloned() {
                            Some(bytes) => (StatusCode::OK, bytes),
                            None => (StatusCode::NOT_FOUND, Vec::new()),
                        }
                    },
                ),
            )
            .with_state(state.clone());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, state, handle)
    }

    #[tokio::test]
    async fn full_device_login_upload_list_download_move_refresh_and_clear() {
        let (base, server_state, server) = spawn_mock().await;
        let dir = std::env::temp_dir().join(format!("jarvis-yd-{}", rand_hex(5)));
        let disk = YandexDisk::with_endpoints(
            dir.join("yandex-disk.json"),
            dir.join("background-cloud.json"),
            &base,
            &format!("{base}/v1/disk"),
            true,
        );
        let status = disk.begin_login().await.unwrap();
        assert_eq!(status["connected"], false);
        assert_eq!(status["login"]["code"], "1234-5678");

        // Wait for background device-code poll to succeed (2nd poll after ~2s).
        for _ in 0..40 {
            if disk.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(disk.is_connected());
        let st = disk.status();
        assert_eq!(st["account"]["login"], "tester");
        assert_eq!(st["account"]["name"], "Тест Юзер");
        assert_eq!(st["account"]["free_mb"], 8192);

        // Upload compressed clip and list.
        let wav = pcm_wav(&[42; 16_000]);
        disk.upload_clip("near", "clip1.wav", wav.clone(), "2026-11-25T10:00:00Z".into())
            .await
            .unwrap();
        // Trigger 401 refresh path on list.
        server_state.lock().unwrap().fail_first_401 = true;
        let clips = disk.list_clips(true).await.unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "clip1.wav");

        // Download & verify WAV bytes.
        let got = disk.download_clip("near", "clip1.wav").await.unwrap();
        assert_eq!(got, wav);

        // Move to pos -> becomes clip1_r.wav.
        disk.move_clip("near", "clip1.wav", "pos").await.unwrap();
        let clips = disk.list_clips(false).await.unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].kind, "pos");
        assert_eq!(clips[0].name, "clip1_r.wav");
        assert!(clips[0].reviewed);

        // Clear all remote clips.
        disk.clear_clips().await.unwrap();
        assert!(disk.list_clips(true).await.unwrap().is_empty());

        // Disconnect removes persisted credentials.
        disk.disconnect().await.unwrap();
        assert!(!disk.is_connected());
        assert!(!dir.join("yandex-disk.json").exists());

        server.abort();
        let _ = fs::remove_dir_all(dir);
    }
}
