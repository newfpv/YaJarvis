use jarvis_plugin_sdk::process::rpc::{PROTOCOL, Peer, Result};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::OnceCell;

#[cfg(feature = "cloud")]
mod cloud;
#[cfg(feature = "training")]
mod storage;
#[cfg(feature = "training")]
mod training;
#[cfg(feature = "training")]
mod training_background;
#[cfg(feature = "training")]
mod training_records;
#[cfg(feature = "training")]
mod training_yadisk;

type Callback = Arc<
    dyn Fn(String, Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result> + Send>>
        + Send
        + Sync,
>;

enum Provider {
    #[cfg(feature = "chatgpt")]
    ChatGpt(jarvis_chatgpt::ChatGpt),
    #[cfg(feature = "cloud")]
    Cloud(cloud::Cloud),
    #[cfg(feature = "vpn")]
    Vpn(jarvis_vpn::Vpn),
    #[cfg(feature = "youtube")]
    Youtube(Arc<jarvis_youtube::Youtube>),
    #[cfg(feature = "homeassistant")]
    HomeAssistant(jarvis_homeassistant::HomeAssistant),
    #[cfg(feature = "yandexhome")]
    YandexHome(jarvis_yandexhome::YandexHome),
    #[cfg(feature = "smartlife")]
    SmartLife(Arc<jarvis_smartlife::SmartLife>),
    #[cfg(feature = "xiaomi")]
    Xiaomi(Arc<jarvis_xiaomi::Xiaomi>),
    #[cfg(feature = "training")]
    Training(training::Training),
}

impl Provider {
    fn start(id: &str, peer: Peer, params: &Value) -> std::result::Result<Self, String> {
        let services: Callback = Arc::new(move |method, params| {
            let peer = peer.clone();
            Box::pin(async move {
                let timeout = if method == "voice.brain" { 45 } else { 20 };
                peer.call(&method, params, Duration::from_secs(timeout)).await
            })
        });
        #[cfg(any(feature = "youtube", feature = "smartlife", feature = "xiaomi"))]
        let home = || -> std::result::Result<jarvis_home_common::Host, String> {
            let data = std::env::current_dir().map_err(|error| error.to_string())?;
            Ok(jarvis_home_common::Host::new(services.clone(), data))
        };
        let _ = (&services, params);
        match id {
            #[cfg(feature = "chatgpt")]
            "chatgpt" => Ok(Self::ChatGpt(jarvis_chatgpt::ChatGpt::new(services))),
            #[cfg(feature = "cloud")]
            "openai" | "gemini" | "grok" | "elevenlabs" => {
                Ok(Self::Cloud(cloud::Cloud::new(id, services)?))
            }
            #[cfg(feature = "vpn")]
            "vpn" => Ok(Self::Vpn(jarvis_vpn::Vpn::new())),
            #[cfg(feature = "youtube")]
            "youtube" => {
                let entry = Arc::new(jarvis_youtube::Youtube::new(home()?));
                entry.watch();
                Ok(Self::Youtube(entry))
            }
            #[cfg(feature = "homeassistant")]
            "homeassistant" => Ok(Self::HomeAssistant(
                jarvis_homeassistant::HomeAssistant::new()?,
            )),
            #[cfg(feature = "yandexhome")]
            "yandexhome" => Ok(Self::YandexHome(jarvis_yandexhome::YandexHome::new())),
            #[cfg(feature = "smartlife")]
            "smartlife" => {
                let entry = Arc::new(jarvis_smartlife::SmartLife::new(home()?));
                entry.start_lan();
                Ok(Self::SmartLife(entry))
            }
            #[cfg(feature = "xiaomi")]
            "xiaomi" => Ok(Self::Xiaomi(Arc::new(jarvis_xiaomi::Xiaomi::new(home()?)?))),
            #[cfg(feature = "training")]
            "training" => Ok(Self::Training(training::Training::new(params)?)),
            _ => Err(format!("Этот исполняемый файл не содержит плагин {id}")),
        }
    }

    async fn request(&self, peer: Peer, method: &str, params: Value) -> Result {
        let _ = (&peer, &params);
        match (self, method) {
            #[cfg(feature = "chatgpt")]
            (Self::ChatGpt(plugin), method) => plugin.request(method, params).await,
            #[cfg(feature = "cloud")]
            (Self::Cloud(plugin), method) => plugin.request(method, params).await,
            #[cfg(feature = "vpn")]
            (Self::Vpn(plugin), "vpn") => plugin.request(params).await,
            #[cfg(feature = "vpn")]
            (Self::Vpn(_), "vpn.validate") => jarvis_vpn::Vpn::validate(params),
            #[cfg(feature = "youtube")]
            (Self::Youtube(plugin), "music") => plugin.music(params).await,
            #[cfg(feature = "youtube")]
            (Self::Youtube(plugin), "volume") => Ok(plugin.volume(&params)),
            #[cfg(feature = "homeassistant")]
            (Self::HomeAssistant(plugin), "homeassistant") => plugin.request(params).await,
            #[cfg(feature = "yandexhome")]
            (Self::YandexHome(plugin), "yandexhome") => {
                let network = if matches!(params["op"].as_str(), Some("has-catalog" | "invalidate"))
                {
                    Value::Null
                } else {
                    peer.call("network", Value::Null, Duration::from_secs(5))
                        .await?
                };
                plugin.request(params, network).await
            }
            #[cfg(feature = "smartlife")]
            (Self::SmartLife(plugin), "smartlife") => plugin.request(params).await,
            #[cfg(feature = "xiaomi")]
            (Self::Xiaomi(plugin), "xiaomi") => plugin.request(params).await,
            #[cfg(feature = "training")]
            (Self::Training(plugin), method) => plugin.request(method, params).await,
            #[allow(unreachable_patterns)]
            _ => Err(format!("Плагин не поддерживает метод {method}")),
        }
    }

    async fn stop(&self) {
        match self {
            #[cfg(feature = "chatgpt")]
            Self::ChatGpt(plugin) => plugin.shutdown().await,
            #[cfg(feature = "cloud")]
            Self::Cloud(plugin) => plugin.stop(),
            #[cfg(feature = "vpn")]
            Self::Vpn(plugin) => plugin.stop().await,
            #[cfg(feature = "youtube")]
            Self::Youtube(plugin) => plugin.stop().await,
            #[cfg(feature = "smartlife")]
            Self::SmartLife(plugin) => plugin.stop(),
            #[cfg(feature = "xiaomi")]
            Self::Xiaomi(plugin) => plugin.stop(),
            #[cfg(feature = "training")]
            Self::Training(plugin) => plugin.stop(),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_logger();
    let plugin = Arc::new(OnceCell::<Provider>::new());
    let endpoint = Peer::open(
        tokio::io::stdin(),
        tokio::io::stdout(),
        Box::new(move |peer, method, params| {
            let plugin = plugin.clone();
            Box::pin(async move {
                if method == "init" {
                    if params["api"] != PROTOCOL {
                        return Err("Несовместимый API ядра".into());
                    }
                    let id = params["id"].as_str().ok_or("Нет идентификатора плагина")?;
                    plugin
                        .get_or_try_init(|| async { Provider::start(id, peer, &params) })
                        .await?;
                    return Ok(json!({"api":PROTOCOL}));
                }
                let plugin = plugin.get().ok_or("Плагин ещё не запущен")?;
                if method == "shutdown" {
                    plugin.stop().await;
                    return Ok(Value::Null);
                }
                plugin.request(peer, &method, params).await
            })
        }),
    );
    endpoint.closed().await;
}

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
            || (metadata.level() == log::Level::Info
                && metadata.target().starts_with("jarvis_youtube"))
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{}", record.args());
        }
    }
    fn flush(&self) {}
}

fn init_logger() {
    static LOGGER: StderrLogger = StderrLogger;
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
}
