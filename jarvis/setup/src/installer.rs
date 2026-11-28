use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use jarvis_station::backup::{
    BackupManifest, DATA_PREFIX, MANIFEST_NAME, MAX_SINGLE_FILE, MAX_UNPACKED, MAX_ZIP,
    RESTORE_PLUGINS_FILE, pending_rust_plugins, validate_manifest, verify_and_prepare_file,
};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, ErrorKind, Read, Seek},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

mod adb;
mod discovery;

include!(concat!(env!("OUT_DIR"), "/ui_files.rs"));

const REPO: &str = match option_env!("JARVIS_REPO") {
    Some(s) => s,
    None => "newfpv/YaJarvis",
};
const VERSION: &str = match option_env!("JARVIS_VERSION") {
    Some(s) => s,
    None => "dev",
};
const STAGE: &str = "/tmp/jarvis-setup-stage";
const AUTOSTART_MANUAL: &str = "jarvis-autostart-manual";
const TAP_CHANGED: &str = "jarvis-tap-changed";
const MAX_PAYLOAD: usize = 40 << 20;
const INSTALL_RESERVE_KIB: u64 = 1024;
const DEFAULT_ADB_PORT: u16 = 5555;
const INVALID_ADB_ADDRESS: &str =
    "Укажите IPv4-адрес колонки. Порт от 1 до 65535 можно добавить через двоеточие, например 192.168.1.50:5556";
const MODELS: [&str; 3] = ["wake.bin", "oww.bin", "wake_oww.bin"];
const REQUIRED: &[&str] = &[
    "jarvisd",
    "jarvis-supervisor",
    "jarvisd.sh",
    "S98jarvisd",
    "wake.bin",
    "oww.bin",
    "wake_oww.bin",
    "vqe_tap-armhf.so",
    "vqe_tap-arm64.so",
];
type App = Arc<Setup>;

#[derive(Clone)]
struct StagedBackup {
    id: String,
    manifest: BackupManifest,
    files: Vec<(String, Vec<u8>)>,
}

struct Setup {
    token: String,
    address: SocketAddr,
    skip_panel_check: bool,
    job: Mutex<Job>,
    staged_backup: Mutex<Option<StagedBackup>>,
    adb: tokio::sync::OnceCell<PathBuf>,
    scanning: tokio::sync::Mutex<()>,
}

#[derive(Default, Serialize)]
struct Job {
    lines: Vec<String>,
    done: bool,
    error: String,
    warning: String,
    url: String,
    panel_check_skipped: bool,
    reboot_needed: bool,
    reboot_reason: String,
    adb_address: String,
    #[serde(skip)]
    busy: bool,
}

#[derive(Default, Serialize)]
struct Speaker {
    ip: String,
    adb_address: String,
    model: String,
    arch: String,
    userland: String,
    installed: String,
    root: bool,
    android: bool,
    free_mb: u64,
    staging_mb: u64,
    ram_mb: u64,
    need_mb: Option<u64>,
    space_ok: Option<bool>,
    problems: Vec<String>,
    #[serde(skip)]
    storage: Storage,
    #[serde(skip)]
    mini1_pcm: bool,
}

#[derive(Default)]
struct Storage {
    flash_kib: u64,
    previous_kib: u64,
    temp_kib: u64,
    present_models: [bool; MODELS.len()],
}

struct Footprint {
    flash_kib: u64,
    temp_kib: u64,
}

impl Footprint {
    fn from_files(files: &BTreeMap<&str, u64>, userland: &str, present_models: &[bool; MODELS.len()], mini1_pcm: bool) -> Self {
        let tap = format!("vqe_tap-{userland}.so");
        let mut result = Self { flash_kib: INSTALL_RESERVE_KIB, temp_kib: INSTALL_RESERVE_KIB };
        for (&name, &bytes) in files {
            if name == "mini1-pcm" && !mini1_pcm {
                continue;
            }
            let kib = bytes.div_ceil(4096) * 4;
            result.temp_kib += kib;
            let kept = MODELS.iter().position(|model| *model == name).is_some_and(|i| present_models[i]);
            let unused_tap = matches!(name, "vqe_tap-armhf.so" | "vqe_tap-arm64.so") && name != tap;
            if !unused_tap && !kept {
                result.flash_kib += kib;
            }
        }
        result
    }
}

impl Speaker {
    fn check_space(&mut self, files: &BTreeMap<&str, u64>) -> Result<(), String> {
        if self.mini1_pcm && !files.contains_key("mini1-pcm") {
            return Err("Для этой Мини 1 нужен пакет с проигрывателем mini1-pcm".into());
        }
        let needed = Footprint::from_files(files, &self.userland, &self.storage.present_models, self.mini1_pcm);
        self.need_mb = Some(needed.flash_kib.div_ceil(1024));
        let flash_ok = self.storage.flash_kib + self.storage.previous_kib >= needed.flash_kib;
        let temp_ok = self.storage.temp_kib >= needed.temp_kib;
        self.space_ok = Some(flash_ok && temp_ok);
        if !flash_ok {
            return Err(format!("На колонке нужно {} МиБ для этого пакета", needed.flash_kib.div_ceil(1024)));
        }
        if !temp_ok {
            return Err(format!("Для распаковки на колонке нужно {} МиБ в /tmp", needed.temp_kib.div_ceil(1024)));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Address {
    #[serde(rename = "ip", deserialize_with = "deserialize_adb_address")]
    adb_address: SocketAddrV4,
}

#[derive(Deserialize)]
struct Install {
    #[serde(rename = "ip", deserialize_with = "deserialize_adb_address")]
    adb_address: SocketAddrV4,
    #[serde(default)]
    password: String,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    backup_id: String,
}

#[derive(Deserialize)]
struct Restore {
    #[serde(rename = "ip", deserialize_with = "deserialize_adb_address")]
    adb_address: SocketAddrV4,
    backup_id: String,
}

fn deserialize_adb_address<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SocketAddrV4, D::Error> {
    let input = String::deserialize(deserializer)?;
    let input = input.trim();
    if let Ok(ip) = input.parse::<Ipv4Addr>() {
        return Ok(SocketAddrV4::new(ip, DEFAULT_ADB_PORT));
    }
    input
        .parse::<SocketAddrV4>()
        .ok()
        .filter(|address| address.port() != 0)
        .ok_or_else(|| de::Error::custom(INVALID_ADB_ADDRESS))
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("jarvis-setup-{}", token()?));
        std::fs::create_dir(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn run(skip_panel_check: bool) -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::var("JARVIS_SETUP_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let address = listener.local_addr()?;
    let app = Arc::new(Setup {
        token: token()?,
        address,
        skip_panel_check,
        job: Mutex::default(),
        staged_backup: Mutex::default(),
        adb: tokio::sync::OnceCell::new(),
        scanning: tokio::sync::Mutex::new(()),
    });
    let api = Router::new()
        .route("/api/scan", get(scan))
        .route("/api/check", post(check))
        .route("/api/job", get(job))
        .route("/api/install", post(install))
        .route("/api/restore", post(restore))
        .route("/api/reboot", post(reboot))
        .route(
            "/api/backup/inspect",
            post(inspect_backup).layer(DefaultBodyLimit::max(MAX_ZIP + 1)),
        )
        .route("/api/uninstall", post(uninstall))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize));
    let router = Router::new()
        .merge(api)
        .route("/token.js", get(token_js))
        .fallback(get(ui))
        .layer(middleware::from_fn_with_state(app.clone(), loopback))
        .with_state(app);
    let url = format!("http://{address}/");
    println!("Установщик Джарвиса {VERSION}: {url}");
    if std::env::var_os("JARVIS_SETUP_NOBROWSER").is_none() {
        let (cmd, args) = if cfg!(windows) {
            (
                "rundll32",
                vec!["url.dll,FileProtocolHandler", url.as_str()],
            )
        } else if cfg!(target_os = "macos") {
            ("open", vec![url.as_str()])
        } else {
            ("xdg-open", vec![url.as_str()])
        };
        let _ = std::process::Command::new(cmd).args(args).spawn();
    }
    axum::serve(listener, router).await?;
    Ok(())
}

async fn authorize(State(app): State<App>, request: Request, next: Next) -> Response {
    if request
        .headers()
        .get("X-Token")
        .and_then(|v| v.to_str().ok())
        != Some(app.token.as_str())
    {
        return reply(
            StatusCode::FORBIDDEN,
            json!({"error":"Доступ только из установщика"}),
        );
    }
    next.run(request).await
}

async fn loopback(State(app): State<App>, request: Request, next: Next) -> Response {
    if request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        != Some(app.address.to_string().as_str())
    {
        return reply(
            StatusCode::FORBIDDEN,
            json!({"error":"Откройте адрес из окна установщика"}),
        );
    }
    let mut res = next.run(request).await;
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    res.headers_mut().insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static("default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'"));
    res
}

fn token() -> Result<String, String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(hex::encode(bytes))
}

async fn token_js(State(app): State<App>) -> Response {
    let text = format!(
        "window.TOKEN={};window.VERSION={};",
        json!(app.token),
        json!(VERSION)
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
        .body(Body::from(text))
        .unwrap()
}

async fn ui(request: Request) -> Response {
    let path = request.uri().path().strip_prefix('/').unwrap_or("");
    let name = if path.is_empty() { "index.html" } else { path };
    let Some((_, content)) = UI.iter().find(|(asset, _)| *asset == name) else {
        return Response::builder().status(404).body(Body::empty()).unwrap();
    };
    let mime = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from(*content))
        .unwrap()
}

fn reply(status: StatusCode, value: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}
fn bad(message: impl Into<String>) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({"error":message.into()}))
}

impl Setup {
    async fn adb(&self, args: &[&str], seconds: u64) -> Result<String, String> {
        let path = self.adb.get_or_try_init(adb::prepare).await?;
        let mut cmd = tokio::process::Command::new(path);
        let result = tokio::time::timeout(
            Duration::from_secs(seconds),
            cmd.args(args).kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| "ADB не ответил вовремя")?
        .map_err(|error| format!("Не удалось запустить ADB: {error}"))?;
        if !result.status.success() {
            return Err(format!(
                "Команда ADB завершилась с ошибкой {}",
                result.status
            ));
        }
        Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
    }

    async fn shell(&self, target: SocketAddrV4, script: &str) -> Result<String, String> {
        self.adb(&["-s", &target.to_string(), "shell", script], 40)
            .await
    }

    async fn inspect(&self, target: SocketAddrV4) -> Result<Speaker, String> {
        if !target.ip().is_private() {
            return Err("Нужен IP вашей колонки в домашней сети".into());
        }
        self.adb(&["connect", &target.to_string()], 10).await?;
        let info = self.shell(target, r#"PATH="/usr/sbin:/usr/bin:/sbin:/bin:$PATH"; export PATH; id -u; uname -m; head -c 160 /proc/device-tree/model 2>/dev/null | tr -d '\000\n'; printf '\n'; df -k /data 2>/dev/null | awk 'END{print $4}'; awk '/MemTotal/{print $2}' /proc/meminfo; b=$({ dd if=/system/vendor/quasar/maind bs=1 count=20 2>/dev/null | od -An -tu1 2>/dev/null || od -An -tu1 -N20 /system/vendor/quasar/maind 2>/dev/null || hexdump -v -n 20 -e '1/1 "%u "' /system/vendor/quasar/maind 2>/dev/null || dd if=/system/vendor/quasar/maind bs=1 count=20 2>/dev/null | od -t u1 2>/dev/null; } | tr '\n' ' '); if [ -z "$(printf '%s' "$b" | tr -d ' \t')" ] && [ -f /system/vendor/quasar/maind ]; then if [ -r /lib/ld-linux-aarch64.so.1 ] && [ ! -r /lib/ld-linux-armhf.so.3 ]; then b=elf_arm64; elif [ -r /lib/ld-linux-armhf.so.3 ] && [ ! -r /lib/ld-linux-aarch64.so.1 ]; then b=elf_armhf; fi; fi; printf '%s\n' "$b"; m=""; test -d /etc/init.d || m="${m}init.d,"; command -v start-stop-daemon >/dev/null || m="${m}start-stop-daemon,"; if [ -z "$m" ]; then echo ready; else echo "missing:${m%,}"; fi
d=/data/opt/jarvisd
if [ -f "$d/jarvisd.prev-rust" ]; then du -k "$d/jarvisd.prev-rust" 2>/dev/null | awk '{print $1}'; else echo 0; fi
df -k /tmp 2>/dev/null | awk 'END{print $4}'
for model in wake.bin oww.bin wake_oww.bin; do if [ -e "$d/$model" ]; then echo 1; else echo 0; fi; done
if command -v aplay >/dev/null; then echo aplay; else echo missing; fi
if [ -r /lib/ld-linux-armhf.so.3 ] && [ -r /system/vendor/quasar/libs/libasound.so.2 ]; then
    tr -d '\000' 2>/dev/null </proc/device-tree/amlogic-dt-id || true
fi"#).await?;
        let fields: Vec<_> = info.lines().collect();
        let field = |i: usize| fields.get(i).map_or("", |s| s.trim());
        let root = field(0) == "0";
        let arch = match field(1) {
            "aarch64" | "arm64" => "arm64",
            "armv7l" | "armv8l" => "armv7",
            _ => "",
        };
        let userland = parse_elf_userland(field(5));
        let installed = self
            .shell(
                target,
                "test ! -x /data/opt/jarvisd/jarvisd || /data/opt/jarvisd/jarvisd version",
            )
            .await
            .unwrap_or_default();
        let free_kb = field(3).parse::<u64>().unwrap_or(0);
        let rollback_kb = field(7).parse::<u64>().unwrap_or(0);
        let mut sp = Speaker {
            ip: target.ip().to_string(),
            adb_address: target.to_string(),
            model: field(2).into(),
            arch: arch.into(),
            userland: userland.into(),
            installed,
            root,
            mini1_pcm: userland == "armhf" && field(13) == "axg_s420_v03",
            free_mb: free_kb / 1024,
            staging_mb: (free_kb + rollback_kb) / 1024,
            ram_mb: field(4).parse::<u64>().unwrap_or(0) / 1024,
            storage: Storage {
                flash_kib: free_kb,
                previous_kib: rollback_kb,
                temp_kib: field(8).parse().unwrap_or_default(),
                present_models: std::array::from_fn(|i| field(9 + i) == "1"),
            },
            ..Default::default()
        };
        let profile = self
            .shell(target, "cat /system/vendor/quasar/launch.json 2>/dev/null")
            .await
            .unwrap_or_default();
        if !root {
            sp.problems
                .push("На выбранной колонке нужны разрешённый ADB и root".into());
        }
        sp.problems
            .extend(detect_compat_problems(field(1), arch, userland, field(6), &profile));
        if field(12) != "aplay" && !sp.mini1_pcm {
            sp.problems.push("На колонке не найден проигрыватель aplay".into());
        }
        if sp.problems.is_empty()
            && let Some(dir) = payload_dir().0
            && let Ok(file) = std::fs::File::open(dir.join(format!("station-{}.zip", sp.arch)))
            && let Ok(files) = package_sizes(file)
            && let Err(error) = sp.check_space(&files)
        {
            sp.problems.push(error);
        }
        Ok(sp)
    }

    fn start(
        self: &Arc<Self>,
        work: impl std::future::Future<Output = Result<String, String>> + Send + 'static,
    ) -> Response {
        let mut job = self.job.lock().unwrap();
        if job.busy {
            return bad("Уже выполняется другая операция");
        }
        *job = Job {
            busy: true,
            ..Default::default()
        };
        let app = self.clone();
        tokio::spawn(async move {
            let result = work.await;
            let mut job = app.job.lock().unwrap();
            job.busy = false;
            job.done = true;
            match result {
                Ok(url) => job.url = url,
                Err(error) => job.error = error,
            }
        });
        reply(StatusCode::OK, json!({"ok":true}))
    }

    fn log(&self, text: &str) {
        self.job.lock().unwrap().lines.push(text.into());
    }
    async fn push(&self, target: SocketAddrV4, path: &Path, remote_path: &str) -> Result<(), String> {
        let path = path.to_str().ok_or("Недопустимый путь к файлу")?;
        self.adb(&["-s", &target.to_string(), "push", path, remote_path], 120)
            .await?;
        Ok(())
    }
}

fn adb_device_targets(devices: &str) -> (BTreeSet<Ipv4Addr>, Vec<SocketAddrV4>) {
    let mut known_default_addresses = BTreeSet::new();
    let mut connected_targets = Vec::new();
    for line in devices.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(target) = fields
            .next()
            .and_then(|serial| serial.parse::<SocketAddrV4>().ok())
            .filter(|address| address.port() != 0)
        else {
            continue;
        };
        if target.port() == DEFAULT_ADB_PORT {
            known_default_addresses.insert(*target.ip());
        }
        if fields.next() == Some("device") {
            connected_targets.push(target);
        }
    }
    (known_default_addresses, connected_targets)
}

async fn scan(State(app): State<App>) -> Response {
    let Ok(_scanning) = app.scanning.try_lock() else {
        return bad("Поиск колонок уже идёт");
    };
    let devices = match app.adb(&["devices"], 10).await {
        Ok(d) => d,
        Err(e) => return bad(e),
    };
    let (known_default_addresses, connected_targets) = adb_device_targets(&devices);
    let nearby = discovery::nearby(&known_default_addresses).await;
    let candidates = connected_targets.into_iter().map(|target| (target, false)).chain(
        nearby
            .into_iter()
            .take(16)
            .map(|ip| (SocketAddrV4::new(ip, DEFAULT_ADB_PORT), true)),
    );
    let mut probes = tokio::task::JoinSet::new();
    for (target, new) in candidates {
        let app = app.clone();
        probes.spawn(async move {
            let found = tokio::time::timeout(Duration::from_secs(9), app.inspect(target)).await
                .ok().and_then(Result::ok).filter(|speaker| !new || !speaker.userland.is_empty());
            if new && found.is_none() {
                let _ = app.adb(&["disconnect", &target.to_string()], 2).await;
            }
            found
        });
    }
    let mut speakers = Vec::new();
    while let Some(probe) = probes.join_next().await {
        if let Ok(Some(speaker)) = probe { speakers.push(speaker); }
    }
    speakers.sort_by(|a, b| a.ip.cmp(&b.ip).then_with(|| a.adb_address.cmp(&b.adb_address)));
    reply(StatusCode::OK, json!({"speakers":speakers}))
}
async fn check(State(app): State<App>, input: Result<Json<Address>, JsonRejection>) -> Response {
    let Ok(Json(input)) = input else {
        return bad(INVALID_ADB_ADDRESS);
    };
    match app.inspect(input.adb_address).await {
        Ok(s) => reply(StatusCode::OK, json!(s)),
        Err(e) => bad(e),
    }
}
async fn job(State(app): State<App>) -> Response {
    reply(StatusCode::OK, json!(*app.job.lock().unwrap()))
}
async fn install(State(app): State<App>, Json(input): Json<Install>) -> Response {
    let work = app.clone();
    app.start(async move { install_on(&work, input).await })
}
async fn uninstall(State(app): State<App>, Json(input): Json<Address>) -> Response {
    let work = app.clone();
    app.start(async move {
        let speaker = work.inspect(input.adb_address).await?;
        if !speaker.root || speaker.installed.is_empty() {
            return Err("Джарвис на колонке не найден".into());
        }
        work.shell(
            input.adb_address,
            "start-stop-daemon -S -b -x /bin/sh -- /data/opt/jarvisd/jarvisd.sh uninstall",
        )
        .await?;
        Ok(String::new())
    })
}

async fn inspect_backup(State(app): State<App>, body: Bytes) -> Response {
    let parsed = tokio::task::spawn_blocking(move || parse_and_verify_backup(&body))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    let (manifest, files) = match parsed {
        Ok(v) => v,
        Err(error) => return bad(error),
    };
    let id = match token() {
        Ok(id) => id,
        Err(error) => return bad(error),
    };
    let summary = json!({
        "id": id,
        "app_version": manifest.app_version,
        "schema_version": manifest.schema_version,
        "created_at": manifest.created_at,
        "model": manifest.model,
        "files": files.len(),
        "plugins": manifest.plugins.len(),
    });
    *app.staged_backup.lock().unwrap() = Some(StagedBackup {
        id,
        manifest,
        files,
    });
    reply(StatusCode::OK, summary)
}

async fn restore(
    State(app): State<App>,
    input: Result<Json<Restore>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = input else {
        return bad(INVALID_ADB_ADDRESS);
    };
    let backup = match get_staged_backup(&app, &input.backup_id) {
        Ok(b) => b,
        Err(error) => return bad(error),
    };
    let work = app.clone();
    app.start(async move { restore_on(&work, input.adb_address, backup).await })
}

async fn reboot(
    State(app): State<App>,
    input: Result<Json<Address>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = input else {
        return bad(INVALID_ADB_ADDRESS);
    };
    if !input.adb_address.ip().is_private() {
        return bad("Нужен IP вашей колонки в домашней сети");
    }
    if let Err(error) = app.adb(&["connect", &input.adb_address.to_string()], 10).await {
        return bad(error);
    }
    match app
        .shell(
            input.adb_address,
            "start-stop-daemon -S -b -x /bin/sh -- -c 'sync; sleep 1; reboot'",
        )
        .await
    {
        Ok(_) => reply(StatusCode::OK, json!({"ok": true})),
        Err(error) => bad(error),
    }
}

fn get_staged_backup(app: &Setup, id: &str) -> Result<StagedBackup, String> {
    if id.is_empty() {
        return Err("Не выбран файл бэкапа".into());
    }
    let guard = app.staged_backup.lock().unwrap();
    let Some(staged) = guard.as_ref().filter(|b| b.id == id) else {
        return Err("Выбранный бэкап больше не загружен — выберите файл заново".into());
    };
    Ok(staged.clone())
}

fn parse_and_verify_backup(
    raw: &[u8],
) -> Result<(BackupManifest, Vec<(String, Vec<u8>)>), String> {
    if raw.len() > MAX_ZIP {
        return Err(format!("Архив бэкапа больше {} МБ", MAX_ZIP >> 20));
    }
    let mut zr =
        zip::ZipArchive::new(Cursor::new(raw)).map_err(|_| "Это не zip-архив бэкапа".to_string())?;
    let mut manifest_idx = None;
    let mut data_entries = BTreeMap::new();
    for i in 0..zr.len() {
        let f = zr.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().replace('\\', "/");
        if f.is_dir() {
            continue;
        }
        if f.is_symlink() || !f.is_file() {
            return Err(format!("В бэкапе недопустимый файл: {name}"));
        }
        if name == MANIFEST_NAME {
            if manifest_idx.is_some() {
                return Err("В бэкапе несколько backup.json".into());
            }
            manifest_idx = Some(i);
            continue;
        }
        if let Some(rel) = name.strip_prefix(DATA_PREFIX)
            && data_entries.insert(rel.to_string(), i).is_some()
        {
            return Err(format!("Повторяющийся файл в архиве: {name}"));
        }
    }
    let manifest_idx = manifest_idx.ok_or_else(|| "В архиве отсутствует backup.json".to_string())?;
    let mut mf_bytes = Vec::new();
    {
        let mut mf = zr.by_index(manifest_idx).map_err(|e| e.to_string())?;
        mf.by_ref()
            .take((1 << 20) + 1)
            .read_to_end(&mut mf_bytes)
            .map_err(|e| e.to_string())?;
    }
    if mf_bytes.len() > (1 << 20) {
        return Err("Манифест бэкапа слишком большой".into());
    }
    let manifest: BackupManifest =
        serde_json::from_slice(&mf_bytes).map_err(|_| "Неверный формат backup.json".to_string())?;
    validate_manifest(&manifest)?;

    let mut total = 0usize;
    let mut verified = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        let Some(&idx) = data_entries.get(&entry.path) else {
            return Err(format!("В архиве отсутствует файл {}", entry.path));
        };
        let mut raw_file = Vec::new();
        {
            let mut zf = zr.by_index(idx).map_err(|e| e.to_string())?;
            zf.by_ref()
                .take((MAX_SINGLE_FILE + 1) as u64)
                .read_to_end(&mut raw_file)
                .map_err(|e| e.to_string())?;
        }
        total = total
            .checked_add(raw_file.len())
            .ok_or_else(|| "Превышен размер распакованного бэкапа".to_string())?;
        if total > MAX_UNPACKED {
            return Err("Распакованный бэкап превышает допустимый размер".into());
        }
        let processed = verify_and_prepare_file(entry, &raw_file, &manifest)?;
        verified.push((entry.path.clone(), processed));
    }
    Ok((manifest, verified))
}

fn verify_payload(bytes: &[u8], sums: &str, name: &str) -> Result<(), String> {
    let wanted = jarvis_station::checksum(sums, name)
        .ok_or_else(|| format!("В SHA256SUMS нет контрольной суммы для {name}"))?;
    if format!("{:x}", Sha256::digest(bytes)) != wanted {
        return Err(format!("Контрольная сумма {name} не совпала"));
    }
    Ok(())
}

fn local_payload(dir: &Path, name: &str, require_checksum: bool) -> Result<Option<Vec<u8>>, String> {
    let path = dir.join(name);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Не удалось открыть {name}: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_PAYLOAD as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Не удалось прочитать {name}: {error}"))?;
    if bytes.len() > MAX_PAYLOAD {
        return Err(format!("{name} больше допустимого размера"));
    }
    match std::fs::read_to_string(dir.join("SHA256SUMS")) {
        Ok(sums) => verify_payload(&bytes, &sums, name)?,
        Err(error) if !require_checksum && error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Не удалось прочитать SHA256SUMS рядом с {name}: {error}")),
    }
    Ok(Some(bytes))
}

fn payload_dir() -> (Option<PathBuf>, bool) {
    match std::env::var_os("JARVIS_SETUP_PAYLOAD") {
        Some(path) => (Some(PathBuf::from(path)), true),
        None => (
            std::env::current_exe().ok().and_then(|path| path.parent().map(Path::to_path_buf)),
            false,
        ),
    }
}

async fn payload(app: &Setup, arch: &str) -> Result<Vec<u8>, String> {
    let name = format!("station-{arch}.zip");
    let (dir, explicit) = payload_dir();
    if let Some(dir) = dir {
        let local_name = name.clone();
        let local = tokio::task::spawn_blocking(move || local_payload(&dir, &local_name, !explicit))
            .await
            .map_err(|error| error.to_string())??;
        if let Some(bytes) = local {
            app.log(&format!("Использую локальную прошивку {name}"));
            return Ok(bytes);
        }
        if explicit {
            return Err(format!("В указанной папке нет {name}"));
        }
    }
    app.log("Скачиваю прошивку с GitHub…");
    let url = if let Some(url) = option_env!("JARVIS_SETUP_ASSETS") {
        url.to_owned()
    } else if VERSION == "dev" {
        format!("https://github.com/{REPO}/releases/latest/download")
    } else {
        format!("https://github.com/{REPO}/releases/download/v{VERSION}")
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(100))
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .get(format!("{url}/{name}"))
        .send()
        .await
        .map_err(|e| format!("Не удалось скачать {name}: {e}"))?
        .error_for_status()
        .map_err(|_| "Пакет для вашей колонки недоступен")?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Скачивание {name} с GitHub оборвалось: {e}"))?
    {
        if bytes.len() + chunk.len() > MAX_PAYLOAD {
            return Err("Пакет больше допустимого размера".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let sums = client
        .get(format!("{url}/SHA256SUMS"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|_| "Контрольные суммы пакета недоступны")?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    verify_payload(&bytes, &sums, &name)?;
    Ok(bytes)
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn accepts_default_and_custom_adb_ports_in_requests() {
        for (input, expected_port) in [
            ("192.168.1.50", DEFAULT_ADB_PORT),
            ("192.168.1.50:5555", DEFAULT_ADB_PORT),
            (" 192.168.1.50:5556 ", 5556),
            ("192.168.1.50:65535", 65535),
        ] {
            let expected = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), expected_port);
            let address: Address = serde_json::from_value(json!({"ip": input})).unwrap();
            let install: Install = serde_json::from_value(json!({"ip": input})).unwrap();
            assert_eq!(address.adb_address, expected);
            assert_eq!(install.adb_address, expected);
        }
    }

    #[test]
    fn rejects_invalid_adb_addresses() {
        for input in [
            "",
            "192.168.1.50:",
            "192.168.1.50:0",
            "192.168.1.50:65536",
            "192.168.1.50:not-a-port",
            "speaker.local:5556",
            "[::1]:5556",
        ] {
            assert!(serde_json::from_value::<Address>(json!({"ip": input})).is_err());
            assert!(serde_json::from_value::<Install>(json!({"ip": input})).is_err());
        }
    }

    #[test]
    fn scan_keeps_connected_custom_ports_and_only_skips_known_default_ports() {
        let (known_default, connected) = adb_device_targets(concat!(
            "List of devices attached\n",
            "192.168.1.50:5556\tdevice\n",
            "192.168.1.51:5555\tdevice\n",
            "192.168.1.52:5555\toffline\n",
            "192.168.1.53:5556\tunauthorized\n",
            "emulator-5554\tdevice\n",
        ));
        assert_eq!(
            known_default,
            BTreeSet::from([Ipv4Addr::new(192, 168, 1, 51), Ipv4Addr::new(192, 168, 1, 52)])
        );
        assert_eq!(
            connected,
            [
                SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 5556),
                SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 51), DEFAULT_ADB_PORT),
            ]
        );
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    #[test]
    fn offline_bundle_uses_only_the_verified_local_archive() {
        let scratch = Scratch::new().unwrap();
        let dir = &scratch.0;
        let name = "station-arm64.zip";
        assert!(local_payload(dir, name, true).unwrap().is_none());
        let bytes = b"test payload";
        std::fs::write(dir.join(name), bytes).unwrap();
        assert!(local_payload(dir, name, true).is_err());
        assert_eq!(local_payload(dir, name, false).unwrap().unwrap(), bytes);
        let digest = format!("{:x}", Sha256::digest(bytes));
        std::fs::write(dir.join("SHA256SUMS"), format!("{digest}  {name}\n")).unwrap();
        assert_eq!(local_payload(dir, name, true).unwrap().unwrap(), bytes);
        std::fs::write(dir.join(name), b"corrupt").unwrap();
        assert!(local_payload(dir, name, true).is_err());
    }

    #[test]
    fn footprint_distinguishes_flash_ram_and_existing_models() {
        let files = BTreeMap::from([
            ("jarvisd", 6_039_752),
            ("jarvis-supervisor", 441_992),
            ("vqe_tap-arm64.so", 5_600),
            ("vqe_tap-armhf.so", 4_648),
            ("spotcmd", 6_908),
            ("wake.bin", 175_678),
            ("oww.bin", 1_348_172),
            ("wake_oww.bin", 627_900),
            ("jarvisd.sh", 2_750),
            ("S98jarvisd", 355),
        ]);
        let clean = Footprint::from_files(&files, "armhf", &[false; 3], false);
        let update = Footprint::from_files(&files, "armhf", &[true; 3], false);
        assert_eq!(clean.flash_kib.div_ceil(1024), 10);
        assert_eq!(update.flash_kib.div_ceil(1024), 8);
        assert_eq!(clean.temp_kib, update.temp_kib);
        let mut speaker = Speaker {
            userland: "armhf".into(),
            storage: Storage { flash_kib: clean.flash_kib, temp_kib: clean.temp_kib - 1, ..Default::default() },
            ..Default::default()
        };
        assert!(speaker.check_space(&files).unwrap_err().contains("/tmp"));
        speaker.storage.temp_kib = clean.temp_kib;
        assert!(speaker.check_space(&files).is_ok());
        speaker.mini1_pcm = true;
        assert!(speaker.check_space(&files).unwrap_err().contains("mini1-pcm"));
        let mut files = files;
        files.insert("mini1-pcm", 1024);
        speaker.storage.flash_kib += 4;
        speaker.storage.temp_kib += 4;
        assert!(speaker.check_space(&files).is_ok());
    }
}

fn package_entries<R: Read + Seek>(
    reader: R,
    mut visit: impl FnMut(&'static str, zip::read::ZipFile<'_, R>) -> Result<(), String>,
) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;
    for name in REQUIRED.iter().copied().chain(["spotcmd", "mini1-pcm"]) {
        let entry = match zip.by_name(name) {
            Ok(e) => e,
            Err(zip::result::ZipError::FileNotFound) if matches!(name, "spotcmd" | "mini1-pcm") => continue,
            Err(_) => return Err(format!("В пакете отсутствует {name}")),
        };
        if !entry.is_file() || entry.is_symlink() || entry.size() > jarvis_station::MAX_BINARY {
            return Err(format!("Некорректный файл {name}"));
        }
        visit(name, entry)?;
    }
    Ok(())
}

fn package_sizes<R: Read + Seek>(reader: R) -> Result<BTreeMap<&'static str, u64>, String> {
    let mut sizes = BTreeMap::new();
    package_entries(reader, |name, entry| {
        sizes.insert(name, entry.size());
        Ok(())
    })?;
    Ok(sizes)
}

fn unpack(bytes: Vec<u8>, dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut sums = BTreeMap::new();
    package_entries(Cursor::new(bytes), |name, mut entry| {
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(jarvis_station::MAX_BINARY + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() as u64 > jarvis_station::MAX_BINARY {
            return Err(format!("Слишком большой файл {name}"));
        }
        let path = dir.join(name);
        std::fs::write(&path, &data).map_err(|e| e.to_string())?;
        sums.insert(name.to_string(), format!("{:x}", Sha256::digest(&data)));
        Ok(())
    })?;
    Ok(sums)
}

async fn install_on(app: &Setup, input: Install) -> Result<String, String> {
    let target = input.adb_address;
    let backup = if input.backup_id.is_empty() {
        None
    } else {
        Some(get_staged_backup(app, &input.backup_id)?)
    };
    app.log("Подключаюсь к выбранной колонке…");
    let mut sp = app.inspect(target).await?;
    if !sp.problems.is_empty() {
        return Err(sp.problems.join(". "));
    }
    if backup.is_none()
        && (sp.installed.is_empty() || !input.password.is_empty())
        && !(6..=72).contains(&input.password.len())
    {
        return Err("Пароль панели должен содержать от 6 до 72 байт".into());
    }
    if backup.is_some()
        && !input.password.is_empty()
        && !(6..=72).contains(&input.password.len())
    {
        return Err("Пароль панели должен содержать от 6 до 72 байт".into());
    }
    app.log(&format!("Определена архитектура колонки: {}", sp.arch));
    let bytes = payload(app, &sp.arch).await?;
    let files = package_sizes(Cursor::new(&bytes))?;
    sp.check_space(&files)?;
    if let Some(need) = sp.need_mb {
        app.log(&format!("Проверено место на колонке. Пакету нужно до {need} МиБ на флеше"));
    }
    let temp = Scratch::new()?;
    let sums = unpack(bytes, &temp.0)?;
    let stage = format!("{STAGE}-{}", token()?);
    app.shell(target, &format!("umask 077; mkdir {stage}")).await?;
    let result = stage_install(app, input, &sp, &stage, &temp.0, &sums, backup.as_ref()).await;
    let _ = app.shell(target, &format!("rm -rf {stage}")).await;
    result
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn stage_backup_files(
    app: &Setup,
    target: SocketAddrV4,
    stage: &str,
    temp: &Path,
    backup: &StagedBackup,
) -> Result<(), String> {
    app.log(&format!(
        "Переношу бэкап ({} файлов, от версии {})…",
        backup.files.len(),
        backup.manifest.app_version
    ));
    let local_root = temp.join("backup");
    std::fs::create_dir_all(&local_root).map_err(|e| e.to_string())?;
    let mut dirs = BTreeSet::new();
    let mut files_to_push: Vec<(String, PathBuf)> = Vec::with_capacity(backup.files.len() + 1);
    for (rel, data) in &backup.files {
        let local = local_root.join(rel);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&local, data).map_err(|e| e.to_string())?;
        if let Some((parent_rel, _)) = rel.rsplit_once('/') {
            dirs.insert(format!("{stage}/backup/{parent_rel}"));
        }
        files_to_push.push((format!("{stage}/backup/{rel}"), local));
    }
    let pending = pending_rust_plugins(&backup.manifest);
    if !pending.is_empty() {
        let data = serde_json::to_vec_pretty(&pending).map_err(|e| e.to_string())?;
        let local = local_root.join(RESTORE_PLUGINS_FILE);
        std::fs::write(&local, data).map_err(|e| e.to_string())?;
        files_to_push.push((format!("{stage}/backup/{RESTORE_PLUGINS_FILE}"), local));
    }

    let mut mkdir_cmd = format!("umask 077; mkdir -p {}", sh_quote(&format!("{stage}/backup")));
    for dir in dirs {
        mkdir_cmd.push(' ');
        mkdir_cmd.push_str(&sh_quote(&dir));
    }
    app.shell(target, &mkdir_cmd).await?;

    for (remote, local) in files_to_push {
        app.push(target, &local, &remote).await?;
    }
    Ok(())
}

async fn restore_on(
    app: &Setup,
    target: SocketAddrV4,
    backup: StagedBackup,
) -> Result<String, String> {
    app.log("Подключаюсь к выбранной колонке…");
    let speaker = app.inspect(target).await?;
    if !speaker.root {
        return Err("На выбранной колонке нужны разрешённый ADB и root".into());
    }
    if speaker.installed.is_empty() {
        return Err("Сначала установите Джарвиса на колонку".into());
    }
    let temp = Scratch::new()?;
    let stage = format!("{STAGE}-{}", token()?);
    app.shell(target, &format!("umask 077; mkdir {stage}")).await?;
    let result = stage_restore(app, target, &stage, &temp.0, &backup).await;
    let _ = app.shell(target, &format!("rm -rf {stage}")).await;
    result
}

async fn stage_restore(
    app: &Setup,
    target: SocketAddrV4,
    stage: &str,
    temp: &Path,
    backup: &StagedBackup,
) -> Result<String, String> {
    stage_backup_files(app, target, stage, temp, backup).await?;
    app.log("Применяю данные на колонке…");
    let script = format!(
        r#"set -eu
d=/data/opt/jarvisd; s={stage}
if [ -x "$d/jarvisd.sh" ]; then "$d/jarvisd.sh" stop; sleep 1; fi
cp -rf "$s/backup/." "$d/"
chmod 600 "$d/settings.json" "$d/plugins.json" "$d/restore-plugins.json" 2>/dev/null || true
chmod 600 "$d"/plugins-data/*.json "$d"/plugins/*/data.json 2>/dev/null || true
sync
"$d/jarvisd.sh" start"#
    );
    app.shell(target, &script).await?;
    let mut port = 8080u16;
    let mut url = format!("http://{}:{port}/", target.ip());
    if app.skip_panel_check {
        app.job.lock().unwrap().panel_check_skipped = true;
        app.log("Готово. Бэкап применён (проверка по HTTP пропущена)");
        return Ok(String::new());
    }
    app.log("Проверяю запуск панели…");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    for _ in 0..25 {
        let detected = read_panel_port(app, target).await;
        if detected != port {
            port = detected;
            url = format!("http://{}:{port}/", target.ip());
            app.log(&format!("Порт 8080 занят на колонке, проверяю панель на порту {port}…"));
        }
        if let Ok(response) = client.get(format!("{url}healthz")).send().await
            && response
                .json::<Value>()
                .await
                .is_ok_and(|status| status["ready"] == true)
        {
            app.log("Готово. Бэкап восстановлен");
            return Ok(url);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err("Панель не ответила после восстановления бэкапа".into())
}

async fn stage_install(
    app: &Setup,
    input: Install,
    sp: &Speaker,
    stage: &str,
    temp: &Path,
    sums: &BTreeMap<String, String>,
    backup: Option<&StagedBackup>,
) -> Result<String, String> {
    let target = input.adb_address;
    for (name, digest) in sums {
        if name == "mini1-pcm" && !sp.mini1_pcm {
            continue;
        }
        app.log(&format!("Копирую {name}"));
        app.push(target, &temp.join(name), &format!("{stage}/{name}"))
            .await?;
        let actual = app.shell(target, &format!("sha256sum {stage}/{name}")).await?;
        if actual.split_whitespace().next() != Some(digest.as_str()) {
            return Err(format!("Файл {name} повреждён при передаче"));
        }
    }
    app.shell(
        target,
        &format!("chmod 700 {stage}/jarvisd {stage}/jarvis-supervisor"),
    )
    .await?;
    let image = app.shell(target, &format!("{stage}/jarvisd core-info")).await?;
    let image: jarvis_station::Image =
        serde_json::from_str(&image).map_err(|_| "Колонка не запускает Rust-сборку")?;
    if image.format != jarvis_station::IDENTITY || image.arch != sp.arch {
        return Err("Архитектура сборки не совпадает".into());
    }
    let mut required = std::collections::BTreeSet::new();
    if !sp.installed.is_empty() {
        let manifests = app.shell(target, r#"printf '['; comma=; for f in /data/opt/jarvisd/plugins/*/plugin.json; do if [ -f "$f" ]; then printf '%s' "$comma"; cat "$f"; comma=,; fi; done; printf ']'"#).await?;
        let manifests: Vec<Value> = serde_json::from_str(&manifests)
            .map_err(|_| "Не удалось проверить установленные модули")?;
        required.extend(
            manifests
                .iter()
                .filter(|m| m["kind"] == "module")
                .filter_map(|m| m["id"].as_str().map(str::to_owned)),
        );
    }
    let shipped: std::collections::BTreeSet<_> = image.modules.iter().cloned().collect();
    if !required.is_subset(&shipped) || (!sp.installed.is_empty() && required != shipped) {
        return Err("Для этого пакета сначала удалите прежнюю версию. Затем поставьте нужные плагины в панели.".into());
    }
    for id in &image.modules {
        if !(2..=32).contains(&id.len())
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("Неверный идентификатор модуля в пакете".into());
        }
        let manifest = app
            .shell(target, &format!("{stage}/jarvisd module-manifest {id}"))
            .await?;
        let filename = format!("module-{id}.json");
        let path = temp.join(&filename);
        tokio::fs::write(&path, manifest)
            .await
            .map_err(|e| e.to_string())?;
        app.push(target, &path, &format!("{stage}/{filename}")).await?;
    }
    let password_hash = if input.password.is_empty() {
        String::new()
    } else {
        bcrypt::hash(input.password, 10).map_err(|e| e.to_string())?
    };
    let seed = temp.join("seed.json");
    jarvis_station::atomic(&seed, json!({"password_hash":password_hash,"timezone":input.timezone}).to_string().as_bytes()).map_err(|e| e.to_string())?;
    app.push(target, &seed, &format!("{stage}/seed.json")).await?;
    if let Some(backup) = backup {
        stage_backup_files(app, target, stage, temp, backup).await?;
    }
    app.shell(
        target,
        &format!(
            "chmod 700 {stage}/jarvisd {stage}/jarvis-supervisor; chmod 600 {stage}/seed.json"
        ),
    )
    .await?;
    app.log("Добавляю автозапуск…");
    let output = app
        .shell(
            target,
            &install_script(stage, &sp.userland, &sums["jarvisd"], &image.version, backup.is_some())?,
        )
        .await?;
    let manual_autostart = output.lines().any(|line| line.trim() == AUTOSTART_MANUAL);
    let tap_changed = output.lines().any(|line| line.trim() == TAP_CHANGED);
    if manual_autostart {
        app.log("Скрипт для ручной настройки автозапуска сохранён в /data/opt/jarvisd/S98jarvisd");
    }
    let mut port = 8080u16;
    let mut url = format!("http://{}:{port}/", target.ip());
    let client = if app.skip_panel_check {
        app.log("Проверяю завершение установки по ADB…");
        None
    } else {
        app.log("Проверяю запуск панели…");
        Some(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .no_proxy()
                .build()
                .map_err(|e| e.to_string())?,
        )
    };
    for _ in 0..35 {
        let detected = read_panel_port(app, target).await;
        if detected != port {
            port = detected;
            url = format!("http://{}:{port}/", target.ip());
            app.log(&format!("Порт 8080 занят на колонке, панель открыта на порту {port}"));
        }
        let mut mic_online = None;
        let panel_ready = match &client {
            Some(client) => match client.get(format!("{url}healthz")).send().await {
                Ok(response) => response.json::<Value>().await.is_ok_and(|status| {
                    mic_online = status["mic"].as_bool();
                    status["version"] == image.version && status["ready"] == true
                }),
                Err(_) => false,
            },
            None => true,
        };
        if panel_ready
            && app
                .shell(
                    target,
                    concat!(
                        "test ! -e /data/opt/jarvisd/update-rust.json",
                        " && test ! -e /data/opt/jarvisd/update-rust.error",
                    ),
                )
                .await
                .is_ok()
        {
            if let Some(client) = &client
                && mic_online == Some(false)
            {
                for _ in 0..4 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let Ok(resp) = client.get(format!("{url}healthz")).send().await
                        && let Ok(status) = resp.json::<Value>().await
                    {
                        mic_online = status["mic"].as_bool();
                        if mic_online == Some(true) {
                            break;
                        }
                    }
                }
            }
            let vqe_state = platformd_vqe_state(app, target).await;
            if let Some(reason) = reboot_reason_for_mic(tap_changed, &vqe_state, mic_online) {
                app.log(&format!("Внимание: {reason}"));
                let mut job = app.job.lock().unwrap();
                job.reboot_needed = true;
                job.reboot_reason = reason.into();
                job.adb_address = target.to_string();
            }
            if manual_autostart {
                app.job.lock().unwrap().warning = concat!(
                    "Автозапуск не удалось записать в прошивку. ",
                    "Настройте запуск /data/opt/jarvisd/S98jarvisd start до S99quasar ",
                    "и перезагрузите колонку.",
                )
                .into();
            }
            if app.skip_panel_check {
                app.job.lock().unwrap().panel_check_skipped = true;
                app.log("Готово. Проверка панели с компьютера пропущена");
                return Ok(String::new());
            }
            app.log("Готово. Панель Джарвиса отвечает");
            return Ok(url);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if app.skip_panel_check {
        return Err("Колонка не подтвердила завершение установки по ADB".into());
    }
    Err("Панель новой сборки не ответила. Служба вернёт предыдущую версию, если она была установлена".into())
}

async fn read_panel_port(app: &Setup, target: SocketAddrV4) -> u16 {
    app.shell(target, "cat /tmp/jarvisd/port 2>/dev/null || true")
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(8080)
}

async fn platformd_vqe_state(app: &Setup, target: SocketAddrV4) -> String {
    let script = r#"for p in /proc/[0-9]*; do
    [ -r "$p/cmdline" ] || continue
    if tr '\0' ' ' <"$p/cmdline" 2>/dev/null | grep -q -- '--platformd'; then
        if grep -q 'vqe_tap\.so (deleted)' "$p/maps" 2>/dev/null; then echo deleted; exit 0; fi
        if grep -q 'vqe_tap\.so' "$p/maps" 2>/dev/null; then echo loaded; exit 0; fi
        echo missing; exit 0
    fi
done
echo none"#;
    app.shell(target, script).await.unwrap_or_else(|_| "unknown".into())
}

fn reboot_reason_for_mic(
    tap_changed: bool,
    vqe_state: &str,
    mic_online: Option<bool>,
) -> Option<&'static str> {
    if tap_changed || vqe_state == "deleted" {
        return Some("Библиотека микрофона обновилась, но Алиса (maind) держит старую версию до перезагрузки колонки.");
    }
    if matches!(vqe_state, "missing" | "none") {
        return Some("Перехват микрофона установлен, но Алиса работает без него до перезагрузки колонки.");
    }
    if mic_online == Some(false) {
        return Some("Звук с микрофона пока не поступает — перезагрузите колонку.");
    }
    None
}

fn install_script(
    stage: &str,
    userland: &str,
    digest: &str,
    version: &str,
    has_backup: bool,
) -> Result<String, String> {
    if !matches!(userland, "armhf" | "arm64")
        || digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        || version.is_empty()
    {
        return Err("Повреждён пакет установки".into());
    }
    let pending = json!({"version":version,"sha256":digest});
    let restore_block = if has_backup {
        r#"if [ -d "$s/backup" ]; then
    cp -rf "$s/backup/." "$d/"
    chmod 600 "$d/settings.json" "$d/plugins.json" "$d/restore-plugins.json" 2>/dev/null || true
    chmod 600 "$d"/plugins-data/*.json "$d"/plugins/*/data.json 2>/dev/null || true
fi"#
    } else {
        ""
    };
    Ok(format!(
        r#"set -eu
d=/data/opt/jarvisd; s={stage}
mkdir -p "$d"
test ! -e "$d/update-rust.json"
rm -f "$d/jarvisd.prev-rust"
cp "$s/jarvisd" "$d/jarvisd.next-rust"
chmod 755 "$d/jarvisd.next-rust"
cp "$s/jarvis-supervisor" "$d/jarvis-supervisor.new"
chmod 755 "$d/jarvis-supervisor.new"
for name in wake.bin oww.bin wake_oww.bin; do test -e "$d/$name" || cp "$s/$name" "$d/$name"; done
tap_changed=0
old_tap=""
new_tap="$(sha256sum "$s/vqe_tap-{userland}.so" | awk '{{print $1}}')"
if [ -f "$d/vqe_tap.so" ]; then
    old_tap="$(sha256sum "$d/vqe_tap.so" | awk '{{print $1}}')"
fi
if [ "$new_tap" != "$old_tap" ]; then
    cp "$s/vqe_tap-{userland}.so" "$d/vqe_tap.so.new"
fi
if [ -f "$s/spotcmd" ]; then cp "$s/spotcmd" "$d/spotcmd"; chmod 755 "$d/spotcmd"; fi
if [ -f "$s/mini1-pcm" ]; then
    cp "$s/mini1-pcm" "$d/mini1-pcm.new"
    chmod 755 "$d/mini1-pcm.new"
    mv -f "$d/mini1-pcm.new" "$d/mini1-pcm"
fi
init=/etc/init.d/S98jarvisd
manual=0
if [ ! -f "$init" ]; then
    if mount -o remount,rw /; then
        tmp=/etc/init.d/.S98jarvisd.$$
        if [ -d "$init" ] || ! (cp "$s/S98jarvisd" "$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$init"); then
            rm -f "$tmp" || true
            manual=1
        fi
        mount -o remount,ro /
    else
        manual=1
    fi
fi
if [ "$manual" = 1 ]; then
    cp "$s/S98jarvisd" "$d/S98jarvisd"
    chmod 755 "$d/S98jarvisd"
fi
if [ -x "$d/jarvisd.sh" ]; then "$d/jarvisd.sh" stop; sleep 2; fi
for manifest in "$s"/module-*.json; do
    if [ -f "$manifest" ]; then
        id=${{manifest##*/module-}}
        id=${{id%.json}}
        [ ! -e "$d/plugins/$id/plugin.json" ] || continue
        mkdir -p "$d/plugins/$id"
        cp "$manifest" "$d/plugins/$id/plugin.json"
    fi
done
cp "$s/jarvisd.sh" "$d/jarvisd.sh"
chmod 755 "$d/jarvisd.sh"
mv "$d/jarvis-supervisor.new" "$d/jarvis-supervisor"
if [ -f "$d/vqe_tap.so.new" ]; then
    mv "$d/vqe_tap.so.new" "$d/vqe_tap.so"
    tap_changed=1
fi
{restore_block}
cp "$s/seed.json" "$d/seed.json"
chmod 600 "$d/seed.json"
rm -f "$s/seed.json" "$d/disabled" "$d/update-rust.error"
printf '%s' '{pending}' > "$d/update-rust.tmp"
mv "$d/update-rust.tmp" "$d/update-rust.json"
sync
"$d/jarvisd.sh" boot
if [ "$tap_changed" = 1 ]; then
    printf '%s\n' '{TAP_CHANGED}'
fi
if [ "$manual" = 1 ]; then
    printf '%s\n' '{AUTOSTART_MANUAL}'
fi"#
    ))
}

fn parse_elf_userland(raw: &str) -> &'static str {
    let trimmed = raw.trim();
    if trimmed == "elf_armhf" {
        return "armhf";
    }
    if trimmed == "elf_arm64" {
        return "arm64";
    }
    // POSIX `od -t u1` prints octal offsets like `0000000` before decimal bytes; skip tokens > 3 chars.
    let bytes: Vec<u8> = trimmed
        .split_whitespace()
        .filter(|t| t.len() <= 3 && t.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|n| n.parse::<u8>().ok())
        .collect();
    if let Some(pos) = bytes.windows(4).position(|w| w == b"\x7fELF")
        && let Some(hdr) = bytes.get(pos..pos + 20)
        && hdr[5] == 1
    {
        match (hdr[4], u16::from_le_bytes([hdr[18], hdr[19]])) {
            (1, 40) => "armhf",
            (2, 183) => "arm64",
            _ => "",
        }
    } else {
        ""
    }
}

fn detect_compat_problems(
    arch_raw: &str,
    arch: &str,
    userland: &str,
    sys_check: &str,
    profile: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    if arch.is_empty() {
        if arch_raw.is_empty() {
            problems.push("Не удалось определить архитектуру процессора колонки".into());
        } else {
            problems.push(format!(
                "Неподдерживаемая архитектура процессора ({arch_raw}) — нужны armv7 или arm64"
            ));
        }
    }
    if userland.is_empty() {
        problems.push(
            "Не удалось определить ELF-архитектуру /system/vendor/quasar/maind (нужен оригинальный бинарник Алисы и утилита od/hexdump)".into(),
        );
    }
    if sys_check != "ready" {
        match sys_check.strip_prefix("missing:").filter(|s| !s.is_empty()) {
            Some(missing) => problems.push(format!(
                "На колонке не найдены системные компоненты Linux: {missing} (нужны /etc/init.d и start-stop-daemon)"
            )),
            None => problems.push(
                "На колонке не подтверждены системные компоненты Linux (/etc/init.d, start-stop-daemon)".into(),
            ),
        }
    }
    if !profile.contains("--platformd") {
        problems.push("В /system/vendor/quasar/launch.json не найден профиль maind --platformd".into());
    }
    problems
}

#[cfg(test)]
mod mic_and_script_tests {
    use super::*;

    #[test]
    fn install_script_tracks_tap_changes_and_optional_backup_restore() {
        let hex = "a".repeat(64);
        let plain = install_script("/tmp/s", "arm64", &hex, "0.10.16", false).unwrap();
        assert!(plain.contains("tap_changed=0"));
        assert!(plain.contains("jarvis-tap-changed"));
        assert!(!plain.contains("$s/backup"));

        let with_backup = install_script("/tmp/s", "arm64", &hex, "0.10.16", true).unwrap();
        assert!(with_backup.contains("cp -rf \"$s/backup/.\" \"$d/\""));
        assert!(with_backup.find("$s/backup").unwrap() < with_backup.find("$s/seed.json").unwrap());
    }

    #[test]
    fn reboot_reason_flags_updated_or_missing_vqe_tap() {
        assert!(reboot_reason_for_mic(true, "loaded", Some(true)).is_some());
        assert!(reboot_reason_for_mic(false, "deleted", Some(true)).is_some());
        assert!(reboot_reason_for_mic(false, "missing", Some(true)).is_some());
        assert!(reboot_reason_for_mic(false, "loaded", Some(false)).is_some());
        assert!(reboot_reason_for_mic(false, "loaded", Some(true)).is_none());
        assert!(reboot_reason_for_mic(false, "loaded", None).is_none());
    }

    #[test]
    fn parse_elf_userland_handles_valid_and_invalid_headers() {
        let armhf = "127 69 76 70 1 1 1 0 0 0 0 0 0 0 0 0 3 0 40 0";
        let arm64 = "127 69 76 70 2 1 1 0 0 0 0 0 0 0 0 0 3 0 183 0";
        let od_armhf = "0000000 127 69 76 70 1 1 1 0 0 0 0 0 0 0 0 0 0000020 3 0 40 0";
        let od_arm64 = "0000000 127 69 76 70 2 1 1 0 0 0 0 0 0 0 0 0 0000020 3 0 183 0";
        let od_script = "0000000 35 33 47 98 105 110 47 115 104 10";
        let script = "35 33 47 98 105 110 47 115 104 10";
        assert_eq!(parse_elf_userland(armhf), "armhf");
        assert_eq!(parse_elf_userland(arm64), "arm64");
        assert_eq!(parse_elf_userland(od_armhf), "armhf");
        assert_eq!(parse_elf_userland(od_arm64), "arm64");
        assert_eq!(parse_elf_userland("elf_armhf"), "armhf");
        assert_eq!(parse_elf_userland("elf_arm64"), "arm64");
        assert_eq!(parse_elf_userland(od_script), "");
        assert_eq!(parse_elf_userland(script), "");
        assert_eq!(parse_elf_userland(""), "");
    }

    #[test]
    fn detect_compat_problems_separates_causes() {
        assert!(detect_compat_problems("armv7l", "armv7", "armhf", "ready", "--platformd").is_empty());

        let only_maind = detect_compat_problems("aarch64", "arm64", "", "ready", "--platformd");
        assert_eq!(only_maind.len(), 1);
        assert!(only_maind[0].contains("maind"));

        let missing_tools = detect_compat_problems("armv7l", "armv7", "armhf", "missing:init.d,aplay", "--platformd");
        assert_eq!(missing_tools.len(), 1);
        assert!(missing_tools[0].contains("init.d,aplay"));

        let no_profile = detect_compat_problems("aarch64", "arm64", "arm64", "ready", "--mediad");
        assert_eq!(no_profile.len(), 1);
        assert!(no_profile[0].contains("launch.json"));

        let bad_arch = detect_compat_problems("mips", "", "armhf", "ready", "--platformd");
        assert_eq!(bad_arch.len(), 1);
        assert!(bad_arch[0].contains("mips"));
    }
}
