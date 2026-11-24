//! Штатная Алиса: команда ей через локальный канал Quasar (127.0.0.1:9875) — как будто сказали голосом;
//! журнал maind — «Алиса» позвали, кнопка пауза; перезапуск Алисы, если она поднялась без нашего vqe_tap.so.
//!
//! Кадр: длина u32 LE + protobuf; Remoting{7:"AliceCapability", 8:AliceCapabilityMethod}, 253=210.
//! AliceCapabilityMethod{1: START_REQUEST(0), 2: VinsRequest{1:id, 2:event_json, 4:is_silent, 18:TEXT_INPUT(1)}}.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::SocketAddr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn varint(mut v: u64, b: &mut Vec<u8>) {
    while v >= 0x80 {
        b.push(v as u8 | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn pb_int(field: u64, v: u64, b: &mut Vec<u8>) {
    varint(field << 3, b);
    varint(v, b);
}

fn pb_bytes(field: u64, v: &[u8], b: &mut Vec<u8>) {
    varint(field << 3 | 2, b);
    varint(v.len() as u64, b);
    b.extend_from_slice(v);
}

fn rpc_frame(field: u64, payload: &[u8]) -> Vec<u8> {
    let mut remote = vec![];
    pb_bytes(7, b"AliceCapability", &mut remote);
    pb_bytes(field, payload, &mut remote);
    let mut env = vec![];
    pb_bytes(252, &remote, &mut env);
    pb_int(253, 210, &mut env);
    let mut out = (env.len() as u32).to_le_bytes().to_vec();
    out.extend(env);
    out
}

fn frame(text: &str, silent: bool) -> Vec<u8> {
    let ev = serde_json::json!({"type": "text_input", "text": text}).to_string();
    let ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut vins = vec![];
    pb_bytes(1, format!("jarvis-{ns}").as_bytes(), &mut vins);
    pb_bytes(2, ev.as_bytes(), &mut vins);
    pb_int(4, silent as u64, &mut vins);
    pb_int(18, 1, &mut vins); // TEXT_INPUT
    let mut method = vec![];
    pb_int(1, 0, &mut method); // START_REQUEST
    pb_bytes(2, &vins, &mut method);
    rpc_frame(8, &method)
}

fn subscribe_frame() -> Vec<u8> {
    let mut sub = vec![];
    pb_bytes(1, b"AliceCapability", &mut sub);
    rpc_frame(20, &sub)
}

fn vocalize_frame(text: &str) -> Vec<u8> {
    // START_VOCALIZE=18, events=true, TVocalizeDirective(text, Append=1).
    let mut directive = vec![];
    pb_bytes(1, text.as_bytes(), &mut directive);
    pb_int(2, 1, &mut directive);
    let mut method = vec![];
    pb_int(1, 18, &mut method);
    pb_int(3, 1, &mut method);
    pb_bytes(13, &directive, &mut method);
    rpc_frame(8, &method)
}

fn cancel_frame() -> Vec<u8> {
    // CANCEL_DIALOG_AND_CLEAR_QUEUE=6
    let mut method = vec![];
    pb_int(1, 6, &mut method);
    rpc_frame(8, &method)
}

#[derive(Debug)]
enum PbVal<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn decode_fields(data: &[u8]) -> Option<BTreeMap<u64, PbVal<'_>>> {
    let mut offset = 0usize;
    let read_varint = |off: &mut usize| -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = *data.get(*off)?;
            *off += 1;
            if shift == 63 && byte > 1 {
                return None;
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(value);
            }
        }
        None
    };
    let mut out = BTreeMap::new();
    while offset < data.len() {
        let key = read_varint(&mut offset)?;
        let field = key >> 3;
        if field == 0 {
            return None;
        }
        match key & 7 {
            0 => {
                let v = read_varint(&mut offset)?;
                out.insert(field, PbVal::Varint(v));
            }
            2 => {
                let len = usize::try_from(read_varint(&mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let slice = data.get(offset..end)?;
                offset = end;
                out.insert(field, PbVal::Bytes(slice));
            }
            1 => {
                offset = offset.checked_add(8)?;
                if offset > data.len() {
                    return None;
                }
            }
            5 => {
                offset = offset.checked_add(4)?;
                if offset > data.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn extract_alice_event(envelope: &[u8]) -> Option<u64> {
    let env = decode_fields(envelope)?;
    let tag = match env.get(&253) {
        Some(PbVal::Varint(v)) => *v,
        _ => return None,
    };
    let remote_bytes = match env.get(&252) {
        Some(PbVal::Bytes(b)) => *b,
        _ => return None,
    };
    let remote = decode_fields(remote_bytes)?;
    let is_alice = tag == 3
        || (tag == 210
            && matches!(remote.get(&7), Some(PbVal::Bytes(b)) if *b == b"AliceCapability"));
    if !is_alice {
        return None;
    }
    let event_bytes = match remote.get(&18) {
        Some(PbVal::Bytes(b)) => *b,
        _ => return None,
    };
    match decode_fields(event_bytes)?.get(&1) {
        Some(PbVal::Varint(code)) => Some(*code),
        _ => None,
    }
}

/// Отправить Алисе текстовый запрос. silent=false — она ответит голосом, как обычно. Блокирует ~0,2 с.
pub fn text(text: &str, silent: bool) -> Result<(), String> {
    if crate::fastcmd::alice_schedule(text) == crate::fastcmd::AliceSchedule::Pending {
        return Err(crate::fastcmd::ALICE_TIME_QUESTION.into());
    }
    let addr = "127.0.0.1:9875".parse().unwrap();
    let mut c = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).map_err(|e| format!("Алиса недоступна: {e}"))?;
    let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
    let r = c.write_all(&frame(text, silent)).map_err(|e| e.to_string());
    std::thread::sleep(Duration::from_millis(150)); // дать aliced прочитать кадр до закрытия
    r
}

/// Дословно озвучить фразу штатным голосом Алисы через `AliceCapability.START_VOCALIZE`.
/// Не трактует текст как команду и не зависит от ключей AI / VPN.
pub fn vocalize(text: &str, timeout: Duration, cancelled: impl Fn() -> bool) -> Result<(), String> {
    let addr: SocketAddr = "127.0.0.1:9875".parse().unwrap();
    vocalize_on(addr, text, timeout, cancelled)
}

/// Отменить текущую озвучку Алисы (например, по «Стоп» или новой активации).
pub fn cancel_vocalize() {
    let addr: SocketAddr = "127.0.0.1:9875".parse().unwrap();
    if let Ok(mut c) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
        let _ = c.set_write_timeout(Some(Duration::from_millis(500)));
        let _ = c.write_all(&cancel_frame());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn vocalize_on(
    addr: SocketAddr,
    text: &str,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    if cancelled() {
        return Ok(());
    }
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .map_err(|e| format!("Алиса недоступна: {e}"))?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let mut payload = subscribe_frame();
    payload.extend_from_slice(&vocalize_frame(text));
    stream
        .write_all(&payload)
        .map_err(|e| format!("не удалось отправить фразу Алисе: {e}"))?;

    let started_at = Instant::now();
    // Если прошивка не шлёт события поля 18, не блокируем поток на весь таймаут.
    let no_event_grace = Duration::from_secs(3).min(timeout);
    let mut tts_started = false;
    let mut len_buf = [0u8; 4];
    let mut len_got = 0usize;

    while started_at.elapsed() < timeout {
        if cancelled() {
            let _ = stream.write_all(&cancel_frame());
            std::thread::sleep(Duration::from_millis(50));
            return Ok(());
        }
        match stream.read(&mut len_buf[len_got..]) {
            Ok(0) => {
                return if tts_started {
                    Ok(())
                } else {
                    Err("Алиса закрыла соединение до начала озвучки".into())
                };
            }
            Ok(n) => {
                len_got += n;
                if len_got < 4 {
                    continue;
                }
                len_got = 0;
                let size = u32::from_le_bytes(len_buf) as usize;
                if size == 0 || size > 512 * 1024 {
                    return Err("некорректный ответ Quasar IPC".into());
                }
                let mut body = vec![0u8; size];
                let mut got = 0usize;
                while got < size {
                    if cancelled() {
                        let _ = stream.write_all(&cancel_frame());
                        std::thread::sleep(Duration::from_millis(50));
                        return Ok(());
                    }
                    if started_at.elapsed() >= timeout {
                        break;
                    }
                    match stream.read(&mut body[got..]) {
                        Ok(0) => return Err("Quasar IPC оборвал кадр".into()),
                        Ok(k) => got += k,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        Err(e) => return Err(format!("ошибка чтения Quasar IPC: {e}")),
                    }
                }
                if got < size {
                    break;
                }
                if let Some(code) = extract_alice_event(&body) {
                    match code {
                        6 => tts_started = true,            // TTS_STARTED
                        1 if tts_started => return Ok(()),  // TTS_COMPLETED
                        4 => return Err("штатный TTS Алисы сообщил об ошибке".into()), // ON_ALICE_ERROR
                        _ => {}
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !tts_started && started_at.elapsed() >= no_event_grace {
                    return Ok(());
                }
            }
            Err(e) => return Err(format!("ошибка канала Алисы: {e}")),
        }
    }
    Ok(())
}

pub const MAIND_LOG: &str = "/data/quasar/daemons_logs/maind.log";

/// Читать новые строки журнала (переживает ротацию): каждая строка — в on_line. Общий поток.
pub fn follow_log(path: &'static str, on_line: impl Fn(&str) + Send + 'static) {
    follow_logs(vec![(path, Box::new(on_line))]);
}

pub fn follow_logs(watches: Vec<(&'static str, Box<dyn Fn(&str) + Send + 'static>)>) {
    if watches.is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("maind-log".into())
        .stack_size(128 * 1024)
        .spawn(move || follow_logs_loop(watches))
        .expect("поток журнала maind");
}

#[cfg(target_os = "linux")]
fn follow_logs_loop(watches: Vec<(&'static str, Box<dyn Fn(&str) + Send + 'static>)>) {
    use std::ffi::CString;
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    struct Item {
        path: &'static str,
        dir: Option<CString>,
        file_cstr: Option<CString>,
        on_line: Box<dyn Fn(&str) + Send + 'static>,
        cur: Option<(u64, BufReader<std::fs::File>)>,
        first: bool,
        part: String,
        dir_wd: i32,
        file_wd: i32,
    }

    let ifd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    let mut items: Vec<Item> = watches
        .into_iter()
        .map(|(path, on_line)| {
            let p = std::path::Path::new(path);
            let dir = p
                .parent()
                .and_then(|d| d.to_str())
                .and_then(|s| CString::new(s).ok());
            let file_cstr = CString::new(path).ok();
            Item {
                path,
                dir,
                file_cstr,
                on_line,
                cur: None,
                first: true,
                part: String::new(),
                dir_wd: -1,
                file_wd: -1,
            }
        })
        .collect();
    let mut ev_buf = [0u8; 2048];
    loop {
        let mut need_poll_fallback = ifd < 0;
        for it in &mut items {
            if ifd >= 0 && it.dir_wd < 0
                && let Some(dir) = &it.dir
            {
                let wd = unsafe {
                    libc::inotify_add_watch(
                        ifd,
                        dir.as_ptr(),
                        libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM | libc::IN_DELETE,
                    )
                };
                if wd >= 0 {
                    it.dir_wd = wd;
                } else {
                    need_poll_fallback = true;
                }
            }
            let Ok(st) = std::fs::metadata(it.path) else {
                if it.file_wd >= 0 && ifd >= 0 {
                    unsafe { libc::inotify_rm_watch(ifd, it.file_wd) };
                    it.file_wd = -1;
                }
                it.cur = None;
                continue;
            };
            let ino = inode(&st);
            if it.cur.as_ref().is_none_or(|(i, _)| *i != ino) {
                if let Ok(mut f) = std::fs::File::open(it.path) {
                    if it.first {
                        let _ = f.seek(SeekFrom::End(0)); // старые активации не переигрываем
                        it.first = false;
                    }
                    it.part.clear();
                    it.cur = Some((ino, BufReader::with_capacity(8192, f)));
                    if ifd >= 0 {
                        if it.file_wd >= 0 {
                            unsafe { libc::inotify_rm_watch(ifd, it.file_wd) };
                            it.file_wd = -1;
                        }
                        if let Some(fc) = &it.file_cstr {
                            let wd = unsafe {
                                libc::inotify_add_watch(
                                    ifd,
                                    fc.as_ptr(),
                                    libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_MOVE_SELF | libc::IN_DELETE_SELF,
                                )
                            };
                            if wd >= 0 {
                                it.file_wd = wd;
                            }
                        }
                    }
                }
            }
            if it.cur.is_some() && it.file_wd < 0 {
                need_poll_fallback = true;
            }
            if let Some((_, rd)) = it.cur.as_mut() {
                loop {
                    match rd.read_line(&mut it.part) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if it.part.ends_with('\n') => {
                            (it.on_line)(&it.part);
                            it.part.clear();
                        }
                        Ok(_) => {} // строка ещё дописывается — ждём продолжения
                    }
                }
            }
        }
        if ifd < 0 {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let timeout_ms = if need_poll_fallback { 1000 } else { -1 };
        let mut pfd = libc::pollfd {
            fd: ifd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 {
            while unsafe { libc::read(ifd, ev_buf.as_mut_ptr().cast(), ev_buf.len()) } > 0 {}
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn follow_logs_loop(watches: Vec<(&'static str, Box<dyn Fn(&str) + Send + 'static>)>) {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    struct Item {
        path: &'static str,
        on_line: Box<dyn Fn(&str) + Send + 'static>,
        cur: Option<(u64, BufReader<std::fs::File>)>,
        first: bool,
        part: String,
    }
    let mut items: Vec<Item> = watches
        .into_iter()
        .map(|(path, on_line)| Item {
            path,
            on_line,
            cur: None,
            first: true,
            part: String::new(),
        })
        .collect();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        for it in &mut items {
            let Ok(st) = std::fs::metadata(it.path) else { continue };
            let ino = inode(&st);
            if it.cur.as_ref().is_none_or(|(i, _)| *i != ino) {
                let Ok(mut f) = std::fs::File::open(it.path) else { continue };
                if it.first {
                    let _ = f.seek(SeekFrom::End(0));
                    it.first = false;
                }
                it.part.clear();
                it.cur = Some((ino, BufReader::with_capacity(8192, f)));
            }
            let (_, rd) = it.cur.as_mut().unwrap();
            loop {
                match rd.read_line(&mut it.part) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if it.part.ends_with('\n') => {
                        (it.on_line)(&it.part);
                        it.part.clear();
                    }
                    Ok(_) => {}
                }
            }
        }
    }
}

#[cfg(unix)]
fn inode(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}

#[cfg(not(unix))]
fn inode(_: &std::fs::Metadata) -> u64 {
    1
}

const LAUNCH: &str = "/system/vendor/quasar/launch.json";
const RESTART_MARK: &str = "/tmp/jarvisd/alice-mic-restarted"; // /tmp в памяти: не больше раза за загрузку
const RUN_SCRIPT: &str = "/system/vendor/quasar/run.sh";
const RESTART_SHELL: &str = "killall quasar_launcher2; i=0; while pidof maind >/dev/null && [ $i -lt 20 ]; do sleep 0.5; i=$((i+1)); done; \
killall -9 maind 2>/dev/null; sleep 1; start-stop-daemon -S -b -x /system/vendor/quasar/run.sh";

/// Профиль с vqe_tap.so подложен, Алиса работает, но библиотеки в ней нет — она запустилась раньше нас.
fn misses_tap(mounts: &str, maps: &[String]) -> bool {
    mounts.contains(&format!(" {LAUNCH} ")) && !maps.is_empty() && !maps.iter().any(|m| m.contains("vqe_tap"))
}

pub(crate) fn platformd_maps() -> Vec<String> {
    let mut out = vec![];
    for d in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let p = d.path();
        if !p.file_name().is_some_and(|n| n.to_string_lossy().bytes().all(|c| c.is_ascii_digit())) {
            continue;
        }
        let Ok(cmd) = std::fs::read(p.join("cmdline")) else { continue };
        let cmd = String::from_utf8_lossy(&cmd);
        if cmd.contains("maind") && cmd.contains("--platformd")
            && let Ok(m) = std::fs::read_to_string(p.join("maps"))
        {
            out.push(m);
        }
    }
    out
}

/// На Midi/Mini Алиса стартует раньше нас со штатным профилем — микрофона нет, пока её не перезапустить.
pub fn watch_mic(mic: std::sync::Arc<crate::audio::Mic>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(25));
        if mic.online() {
            return;
        }
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        if !misses_tap(&mounts, &platformd_maps()) {
            return;
        }
        if std::path::Path::new(RESTART_MARK).exists() {
            log::info!("микрофон: Алиса снова без vqe_tap.so — второй раз не перезапускаю, нужна перезагрузка колонки");
            return;
        }
        if !std::path::Path::new(RUN_SCRIPT).exists() {
            log::info!("микрофон: Алиса запустилась раньше Джарвиса, а {RUN_SCRIPT} нет — перезапускать не рискую, перезагрузите колонку");
            return;
        }
        let _ = std::fs::write(RESTART_MARK, "1");
        log::info!("микрофон: Алиса запустилась раньше Джарвиса (без vqe_tap.so) — перезапускаю Алису");
        match std::process::Command::new("/bin/sh").args(["-c", RESTART_SHELL]).output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => return log::info!("микрофон: перезапуск Алисы не удался: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => return log::info!("микрофон: перезапуск Алисы не удался: {e}"),
        }
        for i in 1..=60 {
            std::thread::sleep(Duration::from_secs(1));
            if mic.online() {
                return log::info!("микрофон: после перезапуска Алисы на связи ({i} с)");
            }
        }
        log::info!("микрофон: после перезапуска Алисы звука всё ещё нет");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn make_event_frame(tag: u64, event_code: u64) -> Vec<u8> {
        let mut ev = vec![];
        pb_int(1, event_code, &mut ev);
        let mut remote = vec![];
        pb_bytes(7, b"AliceCapability", &mut remote);
        pb_bytes(18, &ev, &mut remote);
        let mut env = vec![];
        pb_bytes(252, &remote, &mut env);
        pb_int(253, tag, &mut env);
        let mut out = (env.len() as u32).to_le_bytes().to_vec();
        out.extend(env);
        out
    }

    #[test]
    fn frame_layout() {
        let f = super::frame("пауза", true);
        let n = u32::from_le_bytes(f[..4].try_into().unwrap()) as usize;
        assert_eq!(n, f.len() - 4);
        assert_eq!(&f[4..6], &[0xe2, 0x0f]); // поле 252, длина следом
        assert!(f.ends_with(&[0xe8, 0x0f, 0xd2, 0x01])); // 253=210
        assert!(super::misses_tap(" /system/vendor/quasar/launch.json ", &["libc.so".into()]));

        // Проверка START_VOCALIZE (метод 18, events=1, TVocalizeDirective) и CANCEL (6).
        let vf = super::vocalize_frame("Проверка связи");
        let vn = u32::from_le_bytes(vf[..4].try_into().unwrap()) as usize;
        assert_eq!(vn, vf.len() - 4);
        let env = decode_fields(&vf[4..]).unwrap();
        let PbVal::Bytes(remote_bytes) = env.get(&252).unwrap() else { panic!() };
        let remote = decode_fields(remote_bytes).unwrap();
        let PbVal::Bytes(method_bytes) = remote.get(&8).unwrap() else { panic!() };
        let method = decode_fields(method_bytes).unwrap();
        assert!(matches!(method.get(&1), Some(PbVal::Varint(18))));
        assert!(matches!(method.get(&3), Some(PbVal::Varint(1))));
        let PbVal::Bytes(dir_bytes) = method.get(&13).unwrap() else { panic!() };
        let dir = decode_fields(dir_bytes).unwrap();
        assert!(matches!(dir.get(&1), Some(PbVal::Bytes(b)) if *b == "Проверка связи".as_bytes()));
        assert!(matches!(dir.get(&2), Some(PbVal::Varint(1))));

        let cf = super::cancel_frame();
        let env = decode_fields(&cf[4..]).unwrap();
        let PbVal::Bytes(remote_bytes) = env.get(&252).unwrap() else { panic!() };
        let remote = decode_fields(remote_bytes).unwrap();
        let PbVal::Bytes(method_bytes) = remote.get(&8).unwrap() else { panic!() };
        assert!(matches!(decode_fields(method_bytes).unwrap().get(&1), Some(PbVal::Varint(6))));
    }

    #[test]
    fn vocalize_handles_completion_error_and_cancel() {
        // 1. Успешный цикл TTS_STARTED(6) -> TTS_COMPLETED(1).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf);
            sock.write_all(&make_event_frame(3, 6)).unwrap();
            sock.write_all(&make_event_frame(210, 1)).unwrap();
        });
        vocalize_on(addr, "Привет", Duration::from_secs(3), || false).unwrap();
        server.join().unwrap();

        // 2. ON_ALICE_ERROR(4) возвращает Err для локального earcon-фоллбека.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf);
            sock.write_all(&make_event_frame(210, 4)).unwrap();
        });
        assert!(vocalize_on(addr, "Ошибка", Duration::from_secs(3), || false).is_err());
        server.join().unwrap();

        // 3. Отмена во время озвучки отправляет CANCEL_DIALOG_AND_CLEAR_QUEUE(6).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let c_srv = cancelled.clone();
        let got_cancel = Arc::new(AtomicBool::new(false));
        let gc_srv = got_cancel.clone();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf);
            sock.write_all(&make_event_frame(3, 6)).unwrap();
            std::thread::sleep(Duration::from_millis(80));
            c_srv.store(true, Ordering::SeqCst);
            let mut cancel_buf = [0u8; 256];
            if let Ok(n) = sock.read(&mut cancel_buf)
                && n > 4
                && cancel_buf[..n].windows(cancel_frame().len()).any(|w| w == cancel_frame().as_slice())
            {
                gc_srv.store(true, Ordering::SeqCst);
            }
        });
        vocalize_on(addr, "Длинная фраза", Duration::from_secs(3), || cancelled.load(Ordering::SeqCst)).unwrap();
        server.join().unwrap();
        assert!(got_cancel.load(Ordering::SeqCst));
    }
}
