//! Measurement only: no PCM, transcript, token or provider error text is retained.
use super::{Callback, Value, json, now};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Default)]
struct Counters {
    start: Option<Instant>,
    elapsed_ms: u64,
    closed: bool,
    sent_ms: u64,
    received_samples: u64,
    limit: bool,
    limit_at: i64,
}
pub(super) struct Meter {
    services: Callback,
    account: String,
    session: String,
    at: i64,
    state: Mutex<Counters>,
}
impl Meter {
    pub fn new(services: Callback, auth: &Value, session: &str) -> Arc<Self> {
        Arc::new(Self {
            services,
            account: auth["_local_id"].as_str().unwrap_or("").into(),
            session: session.into(),
            at: now(),
            state: Mutex::default(),
        })
    }
    pub fn start(&self) {
        let mut s = self.state.lock().unwrap();
        if !s.closed && s.start.is_none() {
            s.start = Some(Instant::now());
        }
    }
    pub fn sent(&self, ms: u64) {
        let mut s = self.state.lock().unwrap();
        if !s.closed && s.start.is_some() {
            s.sent_ms += ms;
        }
    }
    pub fn received(&self, samples: usize) {
        let mut s = self.state.lock().unwrap();
        if !s.closed && s.start.is_some() {
            s.received_samples += samples as u64;
        }
    }
    pub fn limit(&self) {
        let mut s = self.state.lock().unwrap();
        s.limit = true;
        if s.limit_at == 0 {
            s.limit_at = now();
        }
    }
    pub fn finish(&self) {
        let mut s = self.state.lock().unwrap();
        if !s.closed {
            s.elapsed_ms = s.start.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
            s.closed = true;
        }
    }
    fn snapshot(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"id":self.account,"report":{"session":self.session,"started_at":self.at,"opened":s.start.is_some(),"closed":s.closed,"connected_ms":if s.closed{s.elapsed_ms}else{s.start.map(|t|t.elapsed().as_millis() as u64).unwrap_or(0)},"sent_ms":s.sent_ms,"received_ms":s.received_samples/48,"limit":s.limit,"limit_at":s.limit_at}})
    }
    pub async fn flush(&self) {
        if self.account.is_empty() {
            return;
        }
        let _ = (self.services)("voice.stats".into(), self.snapshot()).await;
    }
    pub fn background(self: &Arc<Self>) {
        let meter = self.clone();
        tokio::spawn(async move {
            loop {
                meter.flush().await;
                if meter.state.lock().unwrap().closed {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
    pub fn finish_background(self: &Arc<Self>) {
        self.finish();
        let meter = self.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                meter.flush().await;
            });
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn freeze_on_close_and_never_store_audio_or_credentials() {
        let reports = Arc::new(Mutex::new(vec![]));
        let out = reports.clone();
        let cb: Callback = Arc::new(move |method, p| {
            let out = out.clone();
            Box::pin(async move {
                assert_eq!(method, "voice.stats");
                out.lock().unwrap().push(p);
                Ok(Value::Null)
            })
        });
        let m = Meter::new(cb, &json!({"_local_id":"a","access":"secret"}), "s");
        m.start();
        m.sent(20);
        m.received(960);
        m.limit();
        m.finish();
        let first = m.snapshot();
        m.sent(20);
        m.received(960);
        m.finish();
        assert_eq!(first, m.snapshot());
        m.flush().await;
        let v = &reports.lock().unwrap()[0];
        assert_eq!(v["report"]["sent_ms"], 20);
        assert_eq!(v["report"]["received_ms"], 20);
        assert!(!v.to_string().contains("secret"));
    }
}
