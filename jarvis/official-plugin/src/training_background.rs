//! One audio buffer and one upload to Yandex Disk, with no background audio written to flash.
use crate::{
    storage::rand_hex,
    training_records::pcm_wav,
    training_yadisk::YandexDisk,
};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const RATE: usize = 16_000;
const MINUTE: usize = 60 * RATE;

#[derive(Default)]
struct State {
    until: Option<Instant>,
    samples: Vec<i16>,
    uploading: bool,
    sent: u32,
    discarded: u32,
    error: String,
    generation: u64,
    closed: bool,
}

impl State {
    fn stop(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.until = None;
        self.samples = Vec::new();
        self.uploading = false;
    }
}

pub struct Recorder {
    yadisk: Arc<YandexDisk>,
    state: Mutex<State>,
    cancel: tokio::sync::watch::Sender<u64>,
}

impl Recorder {
    pub fn new(yadisk: Arc<YandexDisk>) -> Self {
        Self {
            yadisk,
            state: Mutex::new(State::default()),
            cancel: tokio::sync::watch::channel(0).0,
        }
    }

    pub async fn connect(&self) -> Result<(), String> {
        {
            let s = self.state.lock().unwrap();
            if s.closed || s.until.is_some() {
                return Err("Сначала остановите запись фона".into());
            }
        }
        self.yadisk.begin_login().await?;
        self.state.lock().unwrap().error.clear();
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        {
            let mut s = self.state.lock().unwrap();
            s.stop();
            s.error.clear();
        }
        self.cancel.send_modify(|n| *n = n.wrapping_add(1));
        self.yadisk.disconnect().await
    }

    pub fn start(self: &Arc<Self>, minutes: u32) -> Result<(), String> {
        if !(1..=720).contains(&minutes) {
            return Err("Укажите от 1 до 720 минут".into());
        }
        let duration = Duration::from_secs(u64::from(minutes) * 60);
        let generation = {
            let mut s = self.state.lock().unwrap();
            if s.closed || s.until.is_some() {
                return Err("Фон уже записывается или плагин выключен".into());
            }
            if !self.yadisk.is_connected() {
                return Err("Сначала подключите Яндекс Диск".into());
            }
            s.generation = s.generation.wrapping_add(1);
            s.until = Some(Instant::now() + duration);
            s.sent = 0;
            s.discarded = 0;
            s.error.clear();
            s.generation
        };
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(duration + Duration::from_secs(30)).await;
            if let Some(recorder) = weak.upgrade() {
                let mut state = recorder.state.lock().unwrap();
                if state.generation == generation {
                    state.stop();
                    recorder.cancel.send_modify(|n| *n = n.wrapping_add(1));
                }
            }
        });
        Ok(())
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap();
        s.stop();
        self.cancel.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        s.stop();
        self.cancel.send_modify(|n| *n = n.wrapping_add(1));
        self.yadisk.close();
    }

    pub fn status(&self) -> Value {
        let mut out = self.yadisk.status();
        let s = self.state.lock().unwrap();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("running".into(), json!(s.until.is_some()));
            obj.insert("uploading".into(), json!(s.uploading));
            obj.insert("sent".into(), json!(s.sent));
            obj.insert("discarded".into(), json!(s.discarded));
            obj.insert(
                "left_sec".into(),
                json!(s.until.map_or(0, |until| {
                    until.saturating_duration_since(Instant::now()).as_secs()
                })),
            );
            obj.insert("error".into(), json!(s.error));
        }
        out
    }

    pub fn frame(self: &Arc<Self>, rt: &tokio::runtime::Handle, pcm: &[i16], discard: bool) {
        let mut s = self.state.lock().unwrap();
        let Some(until) = s.until else { return };
        let expired = Instant::now() >= until;
        if s.uploading {
            return;
        }
        if discard {
            if !s.samples.is_empty() {
                s.samples.clear();
                s.discarded += 1;
            }
            if expired {
                s.stop();
            }
            return;
        }
        if !expired {
            if s.samples.capacity() == 0 {
                s.samples.reserve_exact(MINUTE);
            }
            let n = pcm.len().min(MINUTE - s.samples.len());
            s.samples.extend_from_slice(&pcm[..n]);
        }
        if s.samples.len() < MINUTE && (!expired || s.samples.len() < RATE) {
            if expired {
                s.stop();
            }
            return;
        }
        let mut cancel = self.cancel.subscribe();
        let samples = std::mem::take(&mut s.samples);
        let yadisk = self.yadisk.clone();
        let generation = s.generation;
        s.uploading = true;
        drop(s);
        let recorder = self.clone();
        rt.spawn(async move {
            let name = format!(
                "jarvis-bg-{}-{}",
                jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S"),
                rand_hex(4)
            );
            let archive = format!("{name}.zip");
            let packed = tokio::task::spawn_blocking(move || pack(&name, &samples)).await;
            let result = match packed {
                Ok(Ok(zip)) => tokio::select! {
                    biased;
                    _ = cancel.changed() => return,
                    result = yadisk.upload_bg(&archive, zip) => result,
                },
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let mut s = recorder.state.lock().unwrap();
            if s.generation != generation {
                return;
            }
            s.uploading = false;
            match result {
                Ok(()) => {
                    s.sent += 1;
                    if expired || Instant::now() >= until {
                        s.stop();
                    }
                }
                Err(error) => {
                    s.error = error;
                    s.discarded += 1;
                    s.stop();
                }
            }
        });
    }
}

fn pack(name: &str, samples: &[i16]) -> zip::result::ZipResult<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(3));
    zip.start_file(format!("bg/{name}.wav"), options)?;
    zip.write_all(&pcm_wav(samples))?;
    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        extract::Path as AxumPath,
        http::{StatusCode, Uri, header},
        routing::{get, put},
    };
    use std::io::Read;

    #[tokio::test]
    async fn minute_upload_sends_zip_to_yadisk_without_writing_audio_to_disk() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let base_copy = base.clone();

        let app = Router::new()
            .route(
                "/v1/disk/resources/upload",
                get(move |uri: Uri| {
                    let base = base_copy.clone();
                    async move {
                        let url = reqwest::Url::parse(&format!("http://x{uri}")).unwrap();
                        let path = url
                            .query_pairs()
                            .find(|(k, _)| k == "path")
                            .map(|(_, v)| v.into_owned())
                            .unwrap_or_default();
                        let name = path.rsplit('/').next().unwrap_or("bg.zip");
                        (
                            [(header::CONTENT_TYPE, "application/json")],
                            json!({"href": format!("{base}/put/{name}")}).to_string(),
                        )
                    }
                }),
            )
            .route(
                "/put/{name}",
                put(move |_name: AxumPath<String>, data: Bytes| {
                    let tx = tx.clone();
                    async move {
                        tx.send(data).await.unwrap();
                        StatusCode::CREATED
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!("jarvis-bg-yd-{}", rand_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("yandex-disk.json"),
            json!({"access_token": "test-tok"}).to_string(),
        )
        .unwrap();
        let yadisk = YandexDisk::with_endpoints(
            dir.join("yandex-disk.json"),
            dir.join("background-cloud.json"),
            &base,
            &format!("{base}/v1/disk"),
            true,
        );
        let recorder = Arc::new(Recorder::new(yadisk));
        recorder.start(2).unwrap();
        let runtime = tokio::runtime::Handle::current();
        recorder.frame(&runtime, &vec![5; RATE], false);
        recorder.frame(&runtime, &[0], true);
        assert_eq!(recorder.status()["discarded"], 1);
        recorder.frame(&runtime, &vec![7; MINUTE], false);
        let data = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(data)).unwrap();
        let mut file = zip.by_index(0).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[44..46], &7i16.to_le_bytes());
        assert_eq!(bytes.len(), 44 + 2 * MINUTE);
        recorder.close();
        assert_eq!(recorder.status()["running"], false);
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
}
