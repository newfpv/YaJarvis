use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn saved(id: &str) -> Value {
    json!({"account":id,"_local_id":id,"_revision":"revision", "access":format!("token-{id}"),"refresh":format!("refresh-{id}"),"expires":now()+3600})
}
fn host(auto: bool) -> (Callback, Arc<Mutex<(Value, usize)>>) {
    let state = Arc::new(Mutex::new((saved("a"), 0)));
    let shared = state.clone();
    let cb: Callback = Arc::new(move |method, p| {
        let state = shared.clone();
        Box::pin(async move {
            let mut s = state.lock().unwrap();
            match method.as_str() {
                "network" => Ok(json!({})),
                "auth.get" | "accounts.prepare" => Ok(s.0.clone()),
                "auth.set" => {
                    assert_eq!(s.0, p["before"]);
                    s.0 = p["after"].clone();
                    Ok(json!(true))
                }
                "accounts.failover" => {
                    s.1 += 1;
                    assert_eq!(s.0["_local_id"], p["before"]["_local_id"]);
                    if auto && s.0["account"] == "a" {
                        s.0 = saved("b");
                        Ok(json!({"switched":true}))
                    } else {
                        Ok(json!({"switched":false}))
                    }
                }
                _ => Err(format!("unexpected callback {method}")),
            }
        })
    });
    (cb, state)
}
async fn reply(listener: &tokio::net::TcpListener, status: u16, body: &str) -> String {
    let (mut io, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = io.read(&mut buf).await.unwrap();
        assert!(n > 0);
        bytes.extend(&buf[..n]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
        .unwrap_or("0")
        .parse()
        .unwrap();
    while bytes.len() < header_end + length {
        let n = io.read(&mut buf).await.unwrap();
        assert!(n > 0);
        bytes.extend(&buf[..n]);
    }
    io.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nRetry-After: 120\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    String::from_utf8(bytes).unwrap()
}
const SUCCESS: &str = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n";

#[tokio::test]
async fn http_quota_switches_only_when_enabled_and_scopes_sessions() {
    for auto in [false, true] {
        let (cb, state) = host(auto);
        let mut p = ChatGpt::new(cb);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.codex = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let first = reply(
                &listener,
                429,
                r#"{"error":{"code":"usage_limit_reached"}}"#,
            )
            .await;
            assert!(first.contains("Bearer token-a"));
            assert!(first.contains("session_id: a-dialog"));
            if auto {
                let second = reply(&listener, 200, SUCCESS).await;
                assert!(second.contains("Bearer token-b"));
                assert!(second.contains("session_id: b-dialog"));
            }
        });
        let result = p
            .request("codex.request", json!({"session":"dialog","body":{}}))
            .await;
        if auto {
            assert_eq!(result.unwrap()["answer"], "ok");
            assert_eq!(state.lock().unwrap().0["account"], "b");
        } else {
            assert!(result.unwrap_err().contains("Ограничение"));
            assert_eq!(state.lock().unwrap().0["account"], "a");
        }
        assert_eq!(state.lock().unwrap().1, 1);
        server.await.unwrap();
    }
}
#[tokio::test]
async fn html_403_and_server_errors_never_rotate() {
    for (status, body) in [
        (403, "<html>Blocked</html>"),
        (503, r#"{"error":"upstream unavailable"}"#),
    ] {
        let (cb, state) = host(true);
        let mut p = ChatGpt::new(cb);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.codex = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            reply(&listener, status, body).await;
        });
        assert!(
            p.request("codex.request", json!({"body":{}}))
                .await
                .is_err()
        );
        assert_eq!(state.lock().unwrap().1, 0);
        server.await.unwrap();
    }
}
#[tokio::test]
async fn revoked_refresh_token_uses_next_account_without_retrying_forever() {
    let (cb, state) = host(true);
    state.lock().unwrap().0["expires"] = 0.into();
    let mut p = ChatGpt::new(cb);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    p.issuer = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let req = reply(&listener, 400, r#"{"error":"invalid_grant"}"#).await;
        assert!(req.starts_with("POST /oauth/token"));
    });
    assert_eq!(
        p.request("auth.token", Value::Null).await.unwrap()["account"],
        "b"
    );
    assert_eq!(state.lock().unwrap().1, 1);
    server.await.unwrap();
}

#[tokio::test]
async fn voice_limits_try_each_account_once_without_emitting_early_close() {
    let (cb, state) = host(true);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let stats = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_stats = stats.clone();
    let wrapped: Callback = Arc::new(move |method, p| {
        let cb = cb.clone();
        let events = captured.clone();
        let stats = captured_stats.clone();
        Box::pin(async move {
            if method == "voice.stats" {
                stats.lock().unwrap().push(p);
                Ok(Value::Null)
            } else if method == "voice.event" {
                events.lock().unwrap().push(p);
                Ok(Value::Null)
            } else {
                cb(method, p).await
            }
        })
    });
    let mut provider = ChatGpt::new(wrapped);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    provider.call_url = format!("http://{}/call", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for account in ["a", "b"] {
            let req = reply(
                &listener,
                429,
                r#"{"error":{"code":"voice_usage_limit_reached"}}"#,
            )
            .await;
            assert!(req.contains(&format!("Bearer token-{account}")));
        }
    });
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        provider.request(
            "voice.open",
            json!({"id":"test","profile":{"voice":"aube","thread":"dialog"}}),
        ),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("Ограничение"));
    assert_eq!(state.lock().unwrap().1, 2);
    assert!(events.lock().unwrap().iter().all(|e| e["kind"] != "closed"));
    server.await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while stats.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let stats = stats.lock().unwrap();
    for id in ["a", "b"] {
        let report = &stats.iter().find(|s| s["id"] == id).unwrap()["report"];
        assert_eq!(report["limit"], true);
        assert_eq!(report["opened"], false);
        assert_eq!(report["sent_ms"], 0);
    }
}

#[tokio::test]
async fn usage_reads_inactive_account_and_coalesces_requests_without_switching() {
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let seen = callbacks.clone();
    let cb: Callback = Arc::new(move |method, p| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(method.clone());
            match method.as_str() {
                "network" => Ok(json!({})),
                "auth.get" => {
                    assert_eq!(p["id"], "b");
                    Ok(saved("b"))
                }
                _ => panic!("Usage fetch must not mutate account or voice state: {method}"),
            }
        })
    });
    let mut provider = ChatGpt::new(cb);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    provider.usage_url = format!("http://{}/usage", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let req=reply(&listener,200,r#"{"rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_after_seconds":1800},"secondary_window":{"used_percent":40,"limit_window_seconds":604800,"reset_after_seconds":86400}}}"#).await;
        assert!(req.starts_with("GET /usage"));
        assert!(req.contains("Bearer token-b"));
        assert!(req.to_lowercase().contains("chatgpt-account-id: b"));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
    });
    let (a, b) = tokio::join!(
        provider.request("accounts.usage", json!({"id":"b"})),
        provider.request("accounts.usage", json!({"id":"b"}))
    );
    let a = a.unwrap();
    assert_eq!(a, b.unwrap());
    assert_eq!(a["windows"]["five_hour"]["remaining_percent"], 75.0);
    assert_eq!(a["windows"]["weekly"]["remaining_percent"], 60.0);
    assert!(!a.to_string().contains("token-b"));
    server.await.unwrap();
}
