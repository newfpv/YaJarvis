use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const UUID: &str = "00112233-4455-6677-8899-aabbccddeeff";
mod peer;
mod vision;
mod udp;

#[test]
fn parses_vless_and_rejects_unsupported_or_ambiguous_settings() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let valid_pbk = URL_SAFE_NO_PAD.encode([1u8; 32]);
    let c = Config::parse(&format!("vless://{UUID}@[::1]:8443?type=ws&security=tls&sni=example.org&host=front.example.org&path=%2Fproxy%3Fx%3D1#My%20VPN")).unwrap();
    assert_eq!(c.host, "::1");
    assert_eq!(c.port, 8443);
    assert_eq!(c.ws_path, "/proxy?x=1");
    assert_eq!(c.info()["name"], "My VPN");
    assert!(c.tls && c.websocket);
    assert!(!c.info().to_string().contains(UUID));
    let bogus_fp = format!("?security=reality&pbk={valid_pbk}&sni=example.org&fp=bogus");
    for suffix in [
        "?security=reality",
        "?flow=xtls-rprx-vision",
        "?type=grpc",
        "?type=ws&type=tcp",
        "?encryption=secret",
        "?type=ws&security=tls&alpn=h2",
        "?security=tls&alpn=h3",
        bogus_fp.as_str(),
        "?type=ws&host=evil%0D%0Ax:y",
        "?type=ws&path=http://elsewhere",
        "?insecure=yes",
    ] {
        assert!(
            Config::parse(&format!("vless://{UUID}@example.org:443{suffix}")).is_err(),
            "{suffix}"
        );
    }
    for raw in [
        "https://example.org:443",
        "vless://bad@example.org:443",
        "vless://00112233445566778899aabbccddeeff@example.org:0",
        "vless://00112233445566778899aabbccddeeff@example.org",
    ] {
        assert!(Config::parse(raw).is_err());
    }
    let tls_edge = Config::parse(&format!(
        "vless://{UUID}@example.org:443?security=tls&flow=xtls-rprx-vision&fp=edge&alpn=h2"
    ))
    .unwrap();
    assert_eq!(tls_edge.info()["fingerprint"], "rustls");
    let tls_chrome = Config::parse(&format!(
        "vless://{UUID}@example.org:443?security=tls&fp=chrome&alpn=h2%2Chttp%2F1.1"
    ))
    .unwrap();
    assert_eq!(tls_chrome.info()["fingerprint"], "rustls");
    for fp in ["edge", "randomized", "safari"] {
        let reality = Config::parse(&format!(
            "vless://{UUID}@example.org:443?security=reality&pbk={valid_pbk}&sni=example.org&fp={fp}&alpn=h2"
        ))
        .unwrap();
        assert_eq!(reality.info()["fingerprint"], "chrome133");
    }
}

async fn socks(state: &Value, host: &str, port: u16) -> TcpStream {
    socks_expect(state, host, port, 0).await
}
async fn socks_expect(state: &Value, host: &str, port: u16, expected: u8) -> TcpStream {
    let mut socket = TcpStream::connect(state["socks"].as_str().unwrap())
        .await
        .unwrap();
    socket.write_all(&[5, 2, 0, 2]).await.unwrap();
    let mut response = [0; 2];
    socket.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [5, 2]);
    let user = state["user"].as_str().unwrap().as_bytes();
    let pass = state["pass"].as_str().unwrap().as_bytes();
    let mut auth = vec![1, user.len() as u8];
    auth.extend(user);
    auth.push(pass.len() as u8);
    auth.extend(pass);
    socket.write_all(&auth).await.unwrap();
    socket.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [1, 0]);
    let mut connect = vec![5, 1, 0, 3, host.len() as u8];
    connect.extend(host.as_bytes());
    connect.extend(port.to_be_bytes());
    socket.write_all(&connect).await.unwrap();
    let mut response = [0; 10];
    socket.read_exact(&mut response).await.unwrap();
    assert_eq!(response[1], expected);
    socket
}

async fn mock_vless(mut io: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin) {
    let mut prefix = [0; 23];
    io.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix[0], 0);
    assert_eq!(&prefix[1..17], &hex::decode(UUID.replace('-', "")).unwrap());
    assert_eq!(&prefix[17..], &[0, 1, 1, 187, 2, 23]);
    let mut domain = [0; 23];
    io.read_exact(&mut domain).await.unwrap();
    assert_eq!(&domain, b"no-local-dns.invalid.xx");
    // Wait for application data before replying, as a real server can do.
    let mut payload = [0; 4];
    io.read_exact(&mut payload).await.unwrap();
    assert_eq!(&payload, b"ping");
    // Fragment the response header and addons to exercise streaming decoding.
    for byte in [0, 3, 9, 8, 7] {
        io.write_all(&[byte]).await.unwrap();
        tokio::task::yield_now().await;
    }
    io.write_all(b"pong").await.unwrap();
    io.flush().await.unwrap();
    let mut rest = Vec::new();
    let _ = io.read_to_end(&mut rest).await;
}

#[tokio::test]
async fn tcp_remote_dns_reuse_replacement_and_shutdown() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            mock_vless(socket).await;
        });
        let vpn = Vpn::new();
        let link = format!("vless://{UUID}@127.0.0.1:{port}");
        let state = vpn.request(json!({"link":link})).await.unwrap();
        assert_eq!(state, vpn.request(json!({"link":link})).await.unwrap());
        let mut socket = socks(&state, "no-local-dns.invalid.xx", 443).await;
        socket.write_all(b"ping").await.unwrap();
        let mut bytes = [0; 4];
        socket.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"pong");
        assert!(
            vpn.request(json!({"link":"vless://invalid"}))
                .await
                .is_err()
        );
        let mut byte = [0];
        assert!(matches!(socket.read(&mut byte).await, Ok(0) | Err(_)));
        server.await.unwrap();
        let state2 = vpn.request(json!({"link":link})).await.unwrap();
        assert_ne!(state["pass"], state2["pass"]);
        vpn.stop().await;
        assert!(
            TcpStream::connect(state2["socks"].as_str().unwrap())
                .await
                .is_err()
        );
        assert!(
            vpn.request(json!({"link":link}))
                .await
                .unwrap_err()
                .contains("остановлен")
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn socks_authentication_is_required_and_wrong_password_never_dials() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let vpn = Vpn::new();
        let state = vpn
            .request(json!({"link":format!("vless://{UUID}@127.0.0.1:{port}")}))
            .await
            .unwrap();
        let mut socket = TcpStream::connect(state["socks"].as_str().unwrap())
            .await
            .unwrap();
        socket.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0; 2];
        socket.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 255]);
        let mut socket = TcpStream::connect(state["socks"].as_str().unwrap())
            .await
            .unwrap();
        socket.write_all(&[5, 1, 2]).await.unwrap();
        socket.read_exact(&mut reply).await.unwrap();
        socket.write_all(&[1, 1, b'x', 1, b'y']).await.unwrap();
        socket.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [1, 1]);
        assert!(
            tokio::time::timeout(Duration::from_millis(75), listener.accept())
                .await
                .is_err()
        );
        vpn.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn vless_response_rejects_truncation_and_wrong_version() {
    use protocol::VlessIo;
    for bytes in [vec![], vec![0], vec![1, 0], vec![0, 2, 1]] {
        let (client, mut server) = tokio::io::duplex(16);
        server.write_all(&bytes).await.unwrap();
        drop(server);
        let mut io = VlessIo::new(Box::new(client));
        assert!(io.read_u8().await.is_err());
    }
}

fn test_tls() -> tokio_rustls::TlsAcceptor {
    let certificate = rcgen::generate_simple_self_signed(vec!["vpn.example.org".into()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der());
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate.cert.der().clone()], key.into())
    .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(server))
}

#[tokio::test]
async fn tls_requires_trusted_certificate_unless_explicitly_insecure() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let acceptor = test_tls();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            assert!(acceptor.accept(socket).await.is_err());
            let (socket, _) = listener.accept().await.unwrap();
            mock_vless(acceptor.accept(socket).await.unwrap()).await;
        });
        let vpn = Vpn::new();
        let link = format!("vless://{UUID}@127.0.0.1:{port}?security=tls&sni=vpn.example.org");
        let state = vpn.request(json!({"link":link})).await.unwrap();
        drop(socks_expect(&state, "no-local-dns.invalid.xx", 443, 5).await);
        let state = vpn
            .request(json!({"link":format!("{link}&allowInsecure=1")}))
            .await
            .unwrap();
        let mut socket = socks(&state, "no-local-dns.invalid.xx", 443).await;
        socket.write_all(b"ping").await.unwrap();
        let mut response = [0; 4];
        socket.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        vpn.stop().await;
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn websocket_and_secure_websocket_keep_target_remote_and_check_host_path() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{
        Message,
        handshake::server::{Request, Response},
    };
    tokio::time::timeout(Duration::from_secs(8), async {
        for secure in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let io: protocol::BoxIo = if secure { Box::new(test_tls().accept(socket).await.unwrap()) } else { Box::new(socket) };
                let mut ws = tokio_tungstenite::accept_hdr_async(io, |req: &Request, res: Response| {
                    assert_eq!(req.uri().path_and_query().unwrap().as_str(), "/vless?token=path");
                    assert_eq!(req.headers()["host"], "front.example.org");
                    Ok(res)
                }).await.unwrap();
                let header = ws.next().await.unwrap().unwrap().into_data();
                assert_eq!(&header[..1], &[0]); assert_eq!(&header[17..23], &[0, 1, 1, 187, 2, 23]);
                assert_eq!(&header[23..], b"no-local-dns.invalid.xx");
                assert_eq!(&ws.next().await.unwrap().unwrap().into_data()[..], b"ping");
                ws.send(Message::Ping(b"control".as_slice().into())).await.unwrap();
                ws.send(Message::binary(vec![0])).await.unwrap();
                ws.send(Message::binary(vec![2, 8])).await.unwrap();
                ws.send(Message::binary(vec![7, b'p', b'o', b'n', b'g'])).await.unwrap();
                // Keep the peer alive until the plugin closes the tunnel.
                while let Some(message) = ws.next().await { if !matches!(message, Ok(Message::Pong(_))) { break; } }
            });
            let vpn = Vpn::new();
            let link = format!("vless://{UUID}@127.0.0.1:{port}?type=ws&path=%2Fvless%3Ftoken%3Dpath&host=front.example.org{}", if secure {"&security=tls&allowInsecure=1&sni=vpn.example.org"} else {""});
            let state = vpn.request(json!({"link":link})).await.unwrap();
            let mut socket = socks(&state, "no-local-dns.invalid.xx", 443).await;
            socket.write_all(b"ping").await.unwrap(); let mut response = [0; 4]; socket.read_exact(&mut response).await.unwrap(); assert_eq!(&response, b"pong");
            vpn.stop().await; server.await.unwrap();
        }
    }).await.unwrap();
}

#[tokio::test]
async fn socks_rejects_bind_and_unknown_address_types() {
    for request in [[5, 2, 0, 1], [5, 1, 0, 9]] {
        let (mut client, mut server) = tokio::io::duplex(128);
        let task =
            tokio::spawn(async move { protocol::socks_handshake(&mut server, b"u", b"p").await });
        client.write_all(&[5, 1, 2]).await.unwrap();
        let mut auth = [0; 2];
        client.read_exact(&mut auth).await.unwrap();
        client.write_all(&[1, 1, b'u', 1, b'p']).await.unwrap();
        client.read_exact(&mut auth).await.unwrap();
        client.write_all(&request).await.unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], if request[1] == 1 { 8 } else { 7 });
        assert!(task.await.unwrap().is_err());
    }
}

#[test]
fn vless_ip_address_families_use_protocol_not_socks_tags() {
    use protocol::{Address, Target};
    let ipv4 = Target {
        address: Address::Ip("127.1.2.3".parse().unwrap()),
        port: 80,
    }
    .vless_header(&[0; 16])
    .unwrap();
    assert_eq!(&ipv4[19..], &[0, 80, 1, 127, 1, 2, 3]);
    let ipv6 = Target {
        address: Address::Ip("::1".parse().unwrap()),
        port: 443,
    }
    .vless_header(&[0; 16])
    .unwrap();
    assert_eq!(&ipv6[19..22], &[1, 187, 3]);
    assert_eq!(ipv6.len(), 38);
}

#[tokio::test]
async fn connection_limit_counts_incomplete_handshakes_and_stop_closes_them() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let vpn = Vpn::new();
        let state = vpn
            .request(json!({"link":format!("vless://{UUID}@127.0.0.1:443")}))
            .await
            .unwrap();
        let mut clients = Vec::new();
        for _ in 0..64 {
            let mut socket = TcpStream::connect(state["socks"].as_str().unwrap())
                .await
                .unwrap();
            socket.write_all(&[5, 1, 2]).await.unwrap();
            let mut response = [0; 2];
            socket.read_exact(&mut response).await.unwrap();
            assert_eq!(response, [5, 2]);
            clients.push(socket);
        }
        let mut extra = TcpStream::connect(state["socks"].as_str().unwrap())
            .await
            .unwrap();
        assert!(extra.read_u8().await.is_err());
        vpn.stop().await;
        for mut socket in clients {
            assert!(socket.read_u8().await.is_err());
        }
    })
    .await
    .unwrap();
}

#[test]
fn stage_errors_include_stage_and_redact_server_host() {
    let raw = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connect to secret-vpn.example.org:8443 failed");
    let err = transport::sanitize_err("reality_tls", &raw, &["secret-vpn.example.org"]);
    let msg = err.to_string();
    assert!(msg.starts_with("[reality_tls] "));
    assert!(!msg.contains("secret-vpn.example.org"));
    assert!(msg.contains("<redacted>"));
}
