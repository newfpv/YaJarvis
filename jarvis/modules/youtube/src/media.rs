//! Bounded byte-range input and Opus decoding for YouTube's WebM audio.
use crate::remote::Stream;
use jarvis_home_common::bytes;
use libopus_sys as opus;
use reqwest::{Client, StatusCode};
use std::{
    io,
    process::Stdio,
    ptr::NonNull,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

const FIRST_CHUNK: u64 = 64 << 10;
const CHUNK: u64 = 256 << 10;
const PREFETCH_CHUNKS: usize = 4;
const CLUSTER: u32 = 0x1f43b675;
const SAMPLE_RATE: usize = 48_000;
const CHANNELS: usize = 2;
const MAX_FRAME_SAMPLES: usize = 5760;
const BYTES_PER_SEC: f64 = (SAMPLE_RATE * CHANNELS * 2) as f64;

struct Input {
    rx: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
    data: Vec<u8>,
    index: usize,
    eof: bool,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn send_err(tx: &tokio::sync::mpsc::Sender<io::Result<Vec<u8>>>, err: io::Error) {
    let _ = tx.send(Err(err)).await;
}

async fn prefetch(
    client: Client,
    mut stream: Stream,
    mut pos: u64,
    tx: tokio::sync::mpsc::Sender<io::Result<Vec<u8>>>,
) {
    let mut total = stream.length;
    let mut chunk = CHUNK;
    let mut first = true;
    while (total == 0 || pos < total) && !tx.is_closed() {
        let mut want = if first { FIRST_CHUNK.min(chunk) } else { chunk };
        let mut last = io::Error::other("YouTube не отдал звук");
        let mut fetched = false;
        for retry in 0..4 {
            let mut redirects = 0u8;
            loop {
                let end = if total == 0 {
                    pos + want - 1
                } else {
                    (pos + want - 1).min(total - 1)
                };
                let response = client
                    .get(stream.url.clone())
                    .header("User-Agent", stream.agent)
                    .header("Range", format!("bytes={pos}-{end}"))
                    .send()
                    .await;
                match response {
                    Ok(response) if response.status().is_redirection() => {
                        let Some(location) = response
                            .headers()
                            .get(reqwest::header::LOCATION)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| stream.url.join(s).ok())
                            .filter(crate::remote::valid_media_url)
                        else {
                            return send_err(
                                &tx,
                                io::Error::other("Неверное перенаправление аудио YouTube"),
                            )
                            .await;
                        };
                        redirects += 1;
                        if redirects > 5 {
                            return send_err(
                                &tx,
                                io::Error::other("Слишком много перенаправлений аудио YouTube"),
                            )
                            .await;
                        }
                        stream.url = location;
                        continue;
                    }
                    Ok(response)
                        if response.status() == StatusCode::FORBIDDEN && want > 64 << 10 =>
                    {
                        chunk = want / 2;
                        want = chunk;
                        first = false;
                        continue;
                    }
                    Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => {
                        let Some((start, last_byte, content_total)) = content_range(&response)
                            .filter(|&(start, last, total)| {
                                start == pos && (start..=end).contains(&last) && total > last
                            })
                        else {
                            return send_err(&tx, invalid("Неверный диапазон аудио YouTube"))
                                .await;
                        };
                        if total != 0 && total != content_total {
                            return send_err(&tx, invalid("Размер аудио YouTube изменился"))
                                .await;
                        }
                        match bytes(response, want as usize).await {
                            Ok(data) if data.len() as u64 == last_byte - start + 1 => {
                                total = content_total;
                                pos += data.len() as u64;
                                first = false;
                                if tx.send(Ok(data)).await.is_err() {
                                    return;
                                }
                                fetched = true;
                                break;
                            }
                            _ => {
                                last = io::Error::other("Соединение с аудио YouTube оборвалось");
                                break;
                            }
                        }
                    }
                    Ok(response) if response.status() == StatusCode::OK && pos == 0 => {
                        let data = match bytes(response, want.max(CHUNK) as usize).await {
                            Ok(data) => data,
                            Err(e) => return send_err(&tx, io::Error::other(e)).await,
                        };
                        if total != 0 && total != data.len() as u64 {
                            return send_err(&tx, invalid("Неполный ответ YouTube")).await;
                        }
                        if !data.is_empty() {
                            let _ = tx.send(Ok(data)).await;
                        }
                        return;
                    }
                    Ok(response) => {
                        let status = response.status();
                        last = io::Error::other(format!("YouTube не отдал звук ({status})"));
                        if !status.is_server_error() {
                            return send_err(&tx, last).await;
                        }
                        break;
                    }
                    Err(_) => {
                        last = io::Error::other("Соединение с аудио YouTube оборвалось");
                        break;
                    }
                }
            }
            if fetched {
                break;
            }
            if retry < 3 {
                tokio::time::sleep(Duration::from_millis(250 * (retry + 1))).await;
            }
        }
        if !fetched {
            return send_err(&tx, last).await;
        }
    }
}

impl Input {
    fn new(client: Client, stream: Stream, pos: u64) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(PREFETCH_CHUNKS);
        let task = tokio::spawn(prefetch(client, stream, pos, tx));
        Self {
            rx,
            task,
            data: Vec::new(),
            index: 0,
            eof: false,
        }
    }

    async fn refill(&mut self) -> io::Result<bool> {
        if self.index < self.data.len() {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        match self.rx.recv().await {
            Some(Ok(data)) => {
                self.data = data;
                self.index = 0;
                Ok(!self.data.is_empty())
            }
            Some(Err(err)) => {
                self.eof = true;
                Err(err)
            }
            None => {
                self.eof = true;
                Ok(false)
            }
        }
    }

    async fn byte(&mut self) -> io::Result<Option<u8>> {
        if !self.refill().await? {
            return Ok(None);
        }
        let byte = self.data[self.index];
        self.index += 1;
        Ok(Some(byte))
    }

    async fn take(&mut self, mut len: u64, mut out: Option<&mut [u8]>) -> io::Result<()> {
        while len > 0 {
            if !self.refill().await? {
                return Err(truncated());
            }
            let n = len.min((self.data.len() - self.index) as u64) as usize;
            let end = self.index + n;
            if let Some(buf) = out.take() {
                let (head, tail) = buf.split_at_mut(n);
                head.copy_from_slice(&self.data[self.index..end]);
                out = Some(tail);
            }
            self.index = end;
            len -= n as u64;
        }
        Ok(())
    }

    async fn vint(&mut self, first: u8, id: bool) -> io::Result<(u64, bool)> {
        let n = first.leading_zeros() + 1;
        if n > 8 || (id && n > 4) {
            return Err(invalid("Неверный элемент WebM"));
        }
        let mut value = u64::from(if id {
            first
        } else {
            first & (0xffu16 >> n) as u8
        });
        for _ in 1..n {
            value = (value << 8) | u64::from(self.byte().await?.ok_or_else(truncated)?);
        }
        Ok((value, !id && value == (1 << (7 * n)) - 1))
    }
}

fn content_range(response: &reqwest::Response) -> Option<(u64, u64, u64)> {
    let value = response.headers().get("Content-Range")?.to_str().ok()?;
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "Обрыв дорожки WebM")
}

struct Webm {
    input: Input,
    cluster_ms: i64,
}

impl Webm {
    async fn sync(&mut self) -> io::Result<()> {
        let mut window = 0u32;
        for _ in 0..8 << 20 {
            window = (window << 8) | u32::from(self.input.byte().await?.ok_or_else(truncated)?);
            if window == CLUSTER {
                let first = self.input.byte().await?.ok_or_else(truncated)?;
                self.input.vint(first, false).await?;
                return Ok(());
            }
        }
        Err(invalid("В WebM нет начала кластера"))
    }

    async fn next(&mut self, packet: &mut [u8]) -> io::Result<Option<(usize, i64)>> {
        while let Some(first) = self.input.byte().await? {
            let (id, _) = self.input.vint(first, true).await?;
            let first = self.input.byte().await?.ok_or_else(truncated)?;
            let (size, unknown) = self.input.vint(first, false).await?;
            match id {
                0x18538067 | 0x1f43b675 | 0xa0 => {}
                0xe7 if !unknown && (1..=8).contains(&size) => {
                    let mut timestamp = [0; 8];
                    self.input
                        .take(size, Some(&mut timestamp[8 - size as usize..]))
                        .await?;
                    self.cluster_ms = i64::try_from(u64::from_be_bytes(timestamp))
                        .map_err(|_| invalid("Неверная метка времени WebM"))?;
                }
                0xa3 | 0xa1 if !unknown && size >= 4 && size <= packet.len() as u64 => {
                    self.input
                        .take(size, Some(&mut packet[..size as usize]))
                        .await?;
                    let offset = packet[0].leading_zeros() as usize + 1;
                    if offset > 8 || offset + 3 >= size as usize || packet[offset + 2] & 6 != 0 {
                        continue;
                    }
                    let relative = i16::from_be_bytes([packet[offset], packet[offset + 1]]);
                    let timestamp = self.cluster_ms.saturating_add(i64::from(relative));
                    packet.copy_within(offset + 3..size as usize, 0);
                    return Ok(Some((size as usize - offset - 3, timestamp)));
                }
                _ if !unknown => self.input.take(size, None).await?,
                _ => return Err(invalid("Элемент WebM неизвестной длины")),
            }
        }
        Ok(None)
    }
}

struct Decoder(NonNull<opus::OpusDecoder>);
// One playback task owns the decoder and uses it sequentially.
unsafe impl Send for Decoder {}
impl Decoder {
    fn new() -> io::Result<Self> {
        let mut error = 0;
        let pointer = unsafe { opus::opus_decoder_create(SAMPLE_RATE as i32, CHANNELS as i32, &mut error) };
        NonNull::new(pointer)
            .map(Self)
            .ok_or_else(|| io::Error::other(format!("Opus: {error}")))
    }

    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> io::Result<usize> {
        let frame_size = (pcm.len() / CHANNELS) as i32;
        let count = unsafe {
            opus::opus_decode(
                self.0.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                frame_size,
                0,
            )
        };
        usize::try_from(count).map_err(|_| invalid("Неверный кадр Opus"))
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus::opus_decoder_destroy(self.0.as_ptr()) };
    }
}

struct Aplay(Option<tokio::process::Child>);

impl Drop for Aplay {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if child.id().is_none() {
            return;
        }
        // aplay closes ALSA's shared mixer on SIGINT.
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if tokio::time::timeout(Duration::from_secs(1), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            });
        }
    }
}

async fn write_audio(stdin: &mut tokio::process::ChildStdin, data: &[u8]) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(3), stdin.write_all(data))
        .await
        .map_err(|_| io::Error::other("ALSA не принимает звук"))?
}

pub async fn play(
    client: Client,
    stream: Stream,
    start: f64,
    mut progress: impl FnMut(f64),
    volume: impl Fn() -> f32,
) -> io::Result<()> {
    let mut audio = Audio::open(client, stream, start, volume()).await?;
    let (_, data) = audio
        .next(volume())
        .await?
        .ok_or_else(|| invalid("YouTube не вернул кадры Opus"))?;
    let child = tokio::process::Command::new("aplay")
        .args([
            "-q", "-D", "default", "-t", "raw", "-f", "S16_LE", "-r", "48000", "-c", "2",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut player = Aplay(Some(child));
    let child = player.0.as_mut().unwrap();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Нет входа aplay"))?;
    let lead = Duration::from_millis(350);
    let mut next_write = Instant::now();
    write_audio(&mut stdin, data).await?;
    next_write += Duration::from_secs_f64(data.len() as f64 / BYTES_PER_SEC);
    progress(start);
    while let Some((timestamp, data)) = audio.next(volume()).await? {
        // ALSA may accept a long buffer immediately. Keep the producer close to real time
        // so pause and saved positions refer to audio the speaker has had time to play.
        if let Some(deadline) = next_write.checked_sub(lead) {
            tokio::time::sleep_until(deadline.into()).await;
        }
        write_audio(&mut stdin, data).await?;
        next_write =
            next_write.max(Instant::now()) + Duration::from_secs_f64(data.len() as f64 / BYTES_PER_SEC);
        progress((timestamp - lead.as_secs_f64()).max(start));
    }
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .map_err(|_| io::Error::other("ALSA не завершила воспроизведение"))??;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("aplay остановился"))
    }
}

pub struct Audio {
    webm: Webm,
    decoder: Decoder,
    first: Option<(usize, i64)>,
    start: f64,
    gain: f32,
    packet: Vec<u8>,
    pcm: Vec<i16>,
    out: Vec<u8>,
}
impl Audio {
    pub async fn open(client: Client, stream: Stream, start: f64, gain: f32) -> io::Result<Self> {
        tokio::time::sleep_until(stream.ready.into()).await;
        let offset = if start > 0.5 && stream.duration > 0.0 {
            ((start / stream.duration * stream.length as f64) as u64)
                .saturating_sub(128 << 10)
                .min(stream.length.saturating_sub(64 << 10))
        } else {
            0
        };
        let mut webm = Webm {
            input: Input::new(client.clone(), stream.clone(), offset),
            cluster_ms: 0,
        };
        let mut packet = vec![0; 8192];
        let mut first = None;
        if offset > 0 {
            if webm.sync().await.is_ok()
                && let Ok(Some(frame)) = webm.next(&mut packet).await
                && frame.1 <= (start * 1000.0) as i64
            {
                first = Some(frame);
            } else {
                webm = Webm {
                    input: Input::new(client, stream, 0),
                    cluster_ms: 0,
                };
            }
        }
        Ok(Self {
            webm,
            decoder: Decoder::new()?,
            first,
            start,
            gain,
            packet,
            pcm: vec![0; MAX_FRAME_SAMPLES * CHANNELS],
            out: vec![0; MAX_FRAME_SAMPLES * CHANNELS * 2],
        })
    }

    pub async fn next(&mut self, target: f32) -> io::Result<Option<(f64, &[u8])>> {
        loop {
            let frame = match self.first.take() {
                Some(frame) => Some(frame),
                None => self.webm.next(&mut self.packet).await?,
            };
            let Some((len, timestamp)) = frame else {
                return Ok(None);
            };
            let sec = (timestamp as f64 / 1000.0).max(0.0);
            if sec + 0.12 < self.start {
                continue;
            }
            let count = self.decoder.decode(&self.packet[..len], &mut self.pcm)?;
            if sec < self.start {
                continue;
            }
            self.gain = if target < self.gain {
                target
            } else {
                self.gain + (target - self.gain) * 0.1
            };
            let samples = count * CHANNELS;
            for (sample, bytes) in self.pcm[..samples].iter().zip(self.out.chunks_exact_mut(2)) {
                let scaled = (*sample as f32 * self.gain).round().clamp(-32767.0, 32767.0) as i16;
                bytes.copy_from_slice(&scaled.to_le_bytes());
            }
            return Ok(Some((sec, &self.out[..samples * 2])));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn input_prefetches_chunks_and_aborts_on_drop() {
        let payload: Vec<u8> = (0..(400 * 1024usize)).map(|i| (i % 251) as u8).collect();
        let total = payload.len();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let req_count = requests.clone();
        let data = Arc::new(payload.clone());

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req_count = req_count.clone();
                let data = data.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let range = req
                            .lines()
                            .find_map(|l| l.strip_prefix("Range: bytes=").or_else(|| l.strip_prefix("range: bytes=")))
                            .unwrap_or("0-0")
                            .trim();
                        let (s, e) = range.split_once('-').unwrap();
                        let start: usize = s.parse().unwrap();
                        let end: usize = e.parse::<usize>().unwrap().min(total - 1);
                        let slice = &data[start..=end];
                        let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                            slice.len()
                        );
                        if sock.write_all(header.as_bytes()).await.is_err()
                            || sock.write_all(slice).await.is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });

        let stream = Stream {
            url: reqwest::Url::parse(&format!("http://{addr}/audio.webm")).unwrap(),
            agent: "test",
            client: "TEST",
            length: total as u64,
            duration: 30.0,
            ready: Instant::now(),
            itag: 251,
            bitrate: 160_000,
        };
        let client = Client::builder().no_proxy().build().unwrap();
        let mut input = Input::new(client.clone(), stream.clone(), 0);
        // Yield briefly so background prefetch downloads chunk 1 + chunk 2 ahead of consumer.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(requests.load(Ordering::SeqCst) >= 2);

        let mut received = vec![0u8; total];
        input.take(total as u64, Some(&mut received)).await.unwrap();
        assert_eq!(received, payload);
        assert!(input.byte().await.unwrap().is_none());
        drop(input);

        // Verify dropping an active Input aborts background prefetch before downloading everything.
        let count_before = requests.load(Ordering::SeqCst);
        let input2 = Input::new(client, stream, 0);
        drop(input2);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(requests.load(Ordering::SeqCst), count_before);

        server.abort();
    }

    #[tokio::test]
    async fn redirect_validation_allows_googlevideo_and_blocks_other_hosts() {
        assert!(crate::remote::valid_media_url(
            &reqwest::Url::parse("https://rr3---sn-test.googlevideo.com/videoplayback?id=1").unwrap()
        ));
        assert!(!crate::remote::valid_media_url(
            &reqwest::Url::parse("http://rr3---sn-test.googlevideo.com/videoplayback").unwrap()
        ));
        assert!(!crate::remote::valid_media_url(
            &reqwest::Url::parse("https://rr3---sn-test.googlevideo.com:8443/videoplayback").unwrap()
        ));
        assert!(!crate::remote::valid_media_url(
            &reqwest::Url::parse("https://user@rr3---sn-test.googlevideo.com/videoplayback").unwrap()
        ));
        assert!(!crate::remote::valid_media_url(
            &reqwest::Url::parse("https://evil.example/videoplayback").unwrap()
        ));

        // Verify 302 redirect to a non-googlevideo host is rejected during prefetch.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: https://evil.example/audio\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let stream = Stream {
            url: reqwest::Url::parse(&format!("http://{addr}/audio.webm")).unwrap(),
            agent: "test",
            client: "TEST",
            length: 1024,
            duration: 10.0,
            ready: Instant::now(),
            itag: 251,
            bitrate: 160_000,
        };
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut input = Input::new(client, stream, 0);
        let err = input.byte().await.unwrap_err();
        assert!(err.to_string().contains("перенаправление"), "{err}");
        server.abort();
    }
}
