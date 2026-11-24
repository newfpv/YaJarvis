use super::{Host, Manifest};
use futures_util::{SinkExt, StreamExt};
use jarvis_chatgpt::{Callback, ChatGpt};
use jarvis_cloud::{Message, Provider};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::AbortHandle};

struct Cloud {
    provider: Provider,
    services: Callback,
    current: Mutex<Option<Arc<Live>>>,
    next: AtomicU64,
}

struct Live {
    id: u64,
    send: mpsc::Sender<Value>,
    received: tokio::sync::Mutex<mpsc::Receiver<Result<Value, String>>>,
    task: AbortHandle,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Cloud {
    fn new(id: &str, services: Callback) -> Result<Self, String> {
        Ok(Self {
            provider: Provider::new(id)?,
            services,
            current: Mutex::new(None),
            next: AtomicU64::new(1),
        })
    }

    async fn open_live(&self, params: Value) -> Result<Value, String> {
        let key = params["key"].as_str().ok_or("Нет API-ключа")?;
        let network = (self.services)("network".into(), Value::Null).await?;
        let session = params.get("session").filter(|value| !value.is_null());
        let mut socket = self.provider.socket_for(key, &network, session).await?;
        let (send, mut outgoing) = mpsc::channel::<Value>(8);
        let (incoming, received) = mpsc::channel(12);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    value = outgoing.recv() => {
                        let Some(value) = value else { break };
                        if socket.send(value.to_string().into()).await.is_err() {
                            let _ = incoming.send(Err("Обрыв голосового соединения".into())).await;
                            break;
                        }
                    }
                    message = socket.next() => match message {
                        Some(Ok(Message::Close(frame))) => {
                            let (code, reason) = frame
                                .map(|f| (u16::from(f.code), f.reason.to_string()))
                                .unwrap_or((1000, String::new()));
                            let _ = incoming.send(Ok(json!({"close":reason,"code":code}))).await;
                            break;
                        }
                        Some(Ok(message)) if message.is_text() || message.is_binary() => {
                            let event = serde_json::from_slice::<Value>(&message.into_data())
                                .map(|event| json!({"event":event}))
                                .map_err(|_| "Неверное голосовое событие".into());
                            if incoming.send(event).await.is_err() { break }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) => {
                            let _ = incoming.send(Err("Обрыв голосового соединения".into())).await;
                            break;
                        }
                        None => break,
                    }
                }
            }
        }).abort_handle();
        let live = Arc::new(Live {
            id,
            send,
            received: tokio::sync::Mutex::new(received),
            task,
        });
        if let Some(previous) = self.current.lock().unwrap().replace(live) {
            previous.task.abort();
        }
        Ok(json!({"id":id}))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if method == self.provider.id() {
            let network = (self.services)("network".into(), Value::Null).await?;
            return Box::pin(self.provider.request(params, network)).await;
        }
        if method == "live.open" {
            return Box::pin(self.open_live(params)).await;
        }
        let id = params["id"].as_u64().ok_or("Нет голосовой сессии")?;
        if method == "live.close" {
            let mut current = self.current.lock().unwrap();
            if current.as_ref().is_some_and(|live| live.id == id) {
                if let Some(live) = current.take() {
                    live.task.abort();
                }
            }
            return Ok(Value::Null);
        }
        let live = self
            .current
            .lock()
            .unwrap()
            .clone()
            .filter(|live| live.id == id)
            .ok_or("Голосовая сессия завершилась")?;
        match method {
            "live.send" => {
                live.send
                    .send(params["event"].clone())
                    .await
                    .map_err(|_| "Голосовая сессия завершилась")?;
                Ok(Value::Null)
            }
            "live.recv" => {
                let mut received = live.received.lock().await;
                match tokio::time::timeout(Duration::from_secs(12), received.recv()).await {
                    Ok(value) => value.unwrap_or(Ok(Value::Null)),
                    Err(_) => Ok(json!({"idle":true})),
                }
            }
            _ => Err(format!("Неподдерживаемый метод {method}")),
        }
    }

    fn stop(&self) {
        if let Some(live) = self.current.lock().unwrap().take() {
            live.task.abort();
        }
        self.provider.close();
    }
}

pub enum Builtin {
    ChatGpt(ChatGpt),
    Cloud(Cloud),
}

impl Builtin {
    /// Start an in-process voice or cloud provider bound to the plugin host hooks.
    pub fn start(id: &str, host: Weak<Host>) -> Result<Arc<Self>, String> {
        let provider_id = id.to_owned();
        let manifest = Manifest {
            id: provider_id.clone(),
            ..Default::default()
        };
        let services: Callback = Arc::new(move |method, params| {
            let host = host.clone();
            let id = provider_id.clone();
            let manifest = manifest.clone();
            Box::pin(async move {
                let host = host.upgrade().ok_or("Ядро завершилось")?;
                if id == "chatgpt" && method == "voice.event" {
                    (host.hooks.event)(&manifest, &method, params);
                    return Ok(Value::Null);
                }
                (host.hooks.request)(id, method, params).await
            })
        });
        match id {
            "chatgpt" => Ok(Arc::new(Self::ChatGpt(ChatGpt::new(services)))),
            "openai" | "gemini" | "grok" | "elevenlabs" => {
                Ok(Arc::new(Self::Cloud(Cloud::new(id, services)?)))
            }
            _ => Err(format!("Встроенный провайдер «{id}» не найден")),
        }
    }

    /// Execute an in-process provider request subject to the caller timeout.
    pub fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            let fut: Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> = match self {
                Self::ChatGpt(provider) => Box::pin(provider.request(method, params)),
                Self::Cloud(provider) => Box::pin(provider.request(method, params)),
            };
            tokio::time::timeout(timeout, fut)
                .await
                .map_err(|_| "Провайдер не ответил вовремя".to_string())?
        })
    }

    /// Gracefully shut down active voice calls and drop cached HTTP clients.
    pub fn stop(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            match self {
                Self::ChatGpt(provider) => provider.shutdown().await,
                Self::Cloud(provider) => provider.stop(),
            }
        })
    }
}
