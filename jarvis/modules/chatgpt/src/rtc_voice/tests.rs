use super::*;
use tokio::io::{AsyncReadExt,AsyncWriteExt};

#[test]
fn uses_the_high_quality_bitrate_but_respects_the_remote_limit() {
    assert_eq!(audio_bitrate("m=audio 9 UDP/TLS/RTP/SAVPF 111\na=fmtp:111 useinbandfec=1"), 64_000);
    assert_eq!(audio_bitrate("m=audio 9 UDP/TLS/RTP/SAVPF 111\na=fmtp:111 minptime=10; maxaveragebitrate = 32000\r\n"), 32_000);
    assert_eq!(audio_bitrate("m=video 9 UDP/TLS/RTP/SAVPF 111\na=fmtp:111 maxaveragebitrate=8000"), 64_000);
}
struct PeerHandler{gathered:Arc<tokio::sync::Notify>,channel:tokio::sync::mpsc::Sender<Arc<dyn DataChannel>>,audio:tokio::sync::mpsc::Sender<Vec<u8>>}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerHandler{
    async fn on_ice_gathering_state_change(&self,state:RTCIceGatheringState){if state==RTCIceGatheringState::Complete{self.gathered.notify_one();}}
    async fn on_data_channel(&self,channel:Arc<dyn DataChannel>){let _=self.channel.send(channel).await;}
    async fn on_track(&self,track:Arc<dyn TrackRemote>){let tx=self.audio.clone();tokio::spawn(async move{while let Some(event)=track.poll().await{if let TrackRemoteEvent::OnRtpPacket(packet)=event{if tx.send(packet.payload.to_vec()).await.is_err(){break;}}}});}
}
#[tokio::test]
async fn loopback_call_keeps_thread_and_carries_opus_and_delegations(){
    call(json!({}), false).await;
}
#[tokio::test]
async fn rejected_live_11_retries_live_1_with_the_same_voice() {
    call(json!({}), true).await;
}
#[tokio::test]
async fn vpn_call_carries_signaling_ice_dtls_opus_and_delegations(){
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vpn=jarvis_vpn::Vpn::new();
    let network=vpn.request(json!({"link":format!("vless://00112233-4455-6677-8899-aabbccddeeff@{}",listener.local_addr().unwrap())})).await.unwrap();
    let packets=Arc::new(std::sync::atomic::AtomicUsize::new(0));let count=packets.clone();
    let proxy=tokio::spawn(async move{
        let mut tasks=tokio::task::JoinSet::new();
        loop{tokio::select!{
            accepted=listener.accept()=>{let (mut io,_)=accepted.unwrap();let count=count.clone();tasks.spawn(async move{
                let mut prefix=[0;22];io.read_exact(&mut prefix).await.unwrap();assert_eq!(prefix[0],0);assert_eq!(prefix[17],0);
                let port=u16::from_be_bytes([prefix[19],prefix[20]]);
                let ip:std::net::IpAddr=match prefix[21]{1=>{let mut ip=[0;4];io.read_exact(&mut ip).await.unwrap();std::net::Ipv4Addr::from(ip).into()},3=>{let mut ip=[0;16];io.read_exact(&mut ip).await.unwrap();std::net::Ipv6Addr::from(ip).into()},2=>{let len=io.read_u8().await.unwrap() as usize;let mut host=vec![0;len];io.read_exact(&mut host).await.unwrap();String::from_utf8(host).unwrap().parse().unwrap()},_=>panic!("address type")};
                io.write_all(&[0,0]).await.unwrap();
                if prefix[18]==1 {let mut target=tokio::net::TcpStream::connect((ip,port)).await.unwrap();let _=tokio::io::copy_bidirectional(&mut io,&mut target).await;}
                else {assert_eq!(prefix[18],2);let socket=tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();socket.connect((ip,port)).await.unwrap();let (mut read,mut write)=tokio::io::split(io);
                    let send=async{loop{let size=read.read_u16().await? as usize;let mut payload=vec![0;size];read.read_exact(&mut payload).await?;socket.send(&payload).await?;count.fetch_add(1,Ordering::SeqCst);}};
                    let receive=async{let mut payload=[0;65535];loop{let size=socket.recv(&mut payload).await?;write.write_u16(size as u16).await?;write.write_all(&payload[..size]).await?;}};
                    let _:std::io::Result<()>=tokio::select!{result=send=>result,result=receive=>result};
                }
            });},
            _=tasks.join_next(),if !tasks.is_empty()=>{},
        }}
    });
    call(network, false).await;
    assert!(packets.load(Ordering::SeqCst)>5,"ICE, DTLS and RTP must traverse VLESS UDP");
    vpn.stop().await;proxy.abort();let _=proxy.await;
}
async fn call(network:Value, fallback: bool){
    tokio::time::timeout(Duration::from_secs(25),async{
        let (events,event_rx)=tokio::sync::mpsc::channel(128);let mut event_rx=event_rx;
        let reports=Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));let stats=reports.clone();
        let services:Callback=Arc::new(move|method,params|{let stats=stats.clone();let events=events.clone();let network=network.clone();Box::pin(async move{match method.as_str(){
            "voice.stats"=>{stats.lock().unwrap().push(params);Ok(Value::Null)},
            "network"=>Ok(network),"auth.get"|"accounts.prepare"=>Ok(json!({"_local_id":"test-account-id","access":"test-access","account":"test-account","refresh":"test-refresh","expires":super::super::now()+3600})),
            "accounts.ok"=>Ok(Value::Null),
            "voice.event"=>{events.send(params).await.map_err(|e|e.to_string())?;Ok(json!(true))},"voice.brain"=>{assert_eq!(params["task"],"test task");Ok(json!("done"))},_=>Err("unexpected service".into()),
        }})});
        let mut provider=ChatGpt::new(services);let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();provider.call_url=format!("http://{}/call",listener.local_addr().unwrap());
        let (channels,mut channel_rx)=tokio::sync::mpsc::channel(1);let (audio,mut audio_rx)=tokio::sync::mpsc::channel(32);
        let gathered=Arc::new(tokio::sync::Notify::new());let handler=Arc::new(PeerHandler{gathered:gathered.clone(),channel:channels,audio});
        let mut media=MediaEngine::default();media.register_codec(RTCRtpCodecParameters{rtp_codec:RTCRtpCodec{mime_type:"audio/opus".into(),clock_rate:48000,channels:2,sdp_fmtp_line:"minptime=10;useinbandfec=1".into(),rtcp_feedback:vec![]},payload_type:111,..Default::default()},RtpCodecKind::Audio).unwrap();
        let remote:Arc<dyn PeerConnection>=Arc::new(PeerConnectionBuilder::new().with_media_engine(media).with_handler(handler).with_runtime(Arc::new(webrtc::runtime::TokioRuntime)).with_udp_addrs(vec!["0.0.0.0:0".into()]).with_tcp_addrs(Vec::<String>::new()).build().await.unwrap());
        let remote_ssrc=0x54455354;
        let remote_codec=RTCRtpCodec{mime_type:"audio/opus".into(),clock_rate:48000,channels:2,sdp_fmtp_line:"minptime=10;useinbandfec=1".into(),rtcp_feedback:vec![]};
        let remote_track=Arc::new(TrackLocalStaticSample::new(Instant::now(),MediaStreamTrack::new("peer".into(),"audio".into(),"peer".into(),RtpCodecKind::Audio,vec![RTCRtpEncodingParameters{rtp_coding_parameters:RTCRtpCodingParameters{ssrc:Some(remote_ssrc),..Default::default()},codec:remote_codec,..Default::default()}])).unwrap());
        remote.add_track(remote_track.clone() as Arc<dyn TrackLocal>).await.unwrap();
        let _guard=Guard(Some(remote.clone()));let pc=remote.clone();
        let server=tokio::spawn(async move{
            let models: &[_] = if fallback { &[LIVE_11, LIVE_1] } else { &[LIVE_11] };
            for &model in models {
            let (mut socket,_)=listener.accept().await.unwrap();let mut request=Vec::new();let mut chunk=[0;4096];
            let header_end=loop{let n=socket.read(&mut chunk).await.unwrap();assert!(n>0);request.extend(&chunk[..n]);if let Some(at)=request.windows(4).position(|v|v==b"\r\n\r\n"){break at+4;}assert!(request.len()<65536);};
            let headers=String::from_utf8_lossy(&request[..header_end]).to_lowercase();assert!(headers.contains("thread-id: test-account-id-thread-stable"));assert!(headers.contains("session-id: test-account-id-thread-stable"));
            let length:usize=headers.lines().find_map(|line|line.strip_prefix("content-length:").map(str::trim)).unwrap().parse().unwrap();while request.len()<header_end+length{let n=socket.read(&mut chunk).await.unwrap();assert!(n>0);request.extend(&chunk[..n]);}
            let body:Value=serde_json::from_slice(&request[header_end..header_end+length]).unwrap();assert_eq!(body["session"]["initial_items"][0]["role"],"developer");
            assert_eq!(body["session"]["model"], model);
            assert_eq!(body["session"]["audio"]["output"]["voice"], "aube");
            if fallback && model == LIVE_11 {
                let rejection = r#"{"error":{"code":"model_not_found","param":"model"}}"#;
                socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{rejection}", rejection.len()).as_bytes()).await.unwrap();
                continue;
            }
            pc.set_remote_description(RTCSessionDescription::offer(body["sdp"].as_str().unwrap().to_string()).unwrap()).await.unwrap();let answer=pc.create_answer(None).await.unwrap();pc.set_local_description(answer).await.unwrap();let _=tokio::time::timeout(Duration::from_secs(3),gathered.notified()).await;let answer=pc.local_description().await.unwrap().sdp;
            socket.write_all(format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",answer.len(),answer).as_bytes()).await.unwrap();
            }
        });
        let opened = provider.request("voice.open",json!({"id":"local-call","profile":{"voice":"aube","thread":"thread-stable"},"instructions":"You are Jarvis"})).await.unwrap();
        assert_eq!(opened["requested_model"], if fallback { LIVE_1 } else { LIVE_11 });
        server.await.unwrap();
        let channel=channel_rx.recv().await.unwrap();
        let actual = "gpt-live-1.1-codex-test";
        channel.send_text(&json!({"type":"session.started","session":{"model":actual}}).to_string()).await.unwrap();
        channel.send_text(&json!({"type":"session.input_transcript.added","item":{"text":"hello"}}).to_string()).await.unwrap();
        channel.send_text(&json!({"type":"delegation.created","item":{"id":"d1","content":[{"text":"test task"}]}}).to_string()).await.unwrap();
        loop{if let Some(DataChannelEvent::OnMessage(message))=channel.poll().await{let result:Value=serde_json::from_slice(&message.data).unwrap();if result["type"]=="delegation.context.append"{assert_eq!(result["content"][0]["text"],"done");break;}}}
        let mut confirmed = false;
        let mut heard=false;while let Some(event)=event_rx.recv().await{
            if event["kind"]=="model" { assert_eq!(event["value"], actual); confirmed = true; }
            if event["kind"]=="user" && event["value"]=="hello"{heard=true;break;}
        }assert!(heard && confirmed);
        provider.request("voice.feed",json!({"id":"local-call","pcm":B64.encode(vec![0;6400])})).await.unwrap();let packet=audio_rx.recv().await.unwrap();assert_eq!(opus::Decoder::new().unwrap().decode(Some(&packet),false,5760).unwrap().len(),960);
        let text="Проверка голоса. ".repeat(40);provider.request("voice.say",json!({"id":"local-call","text":text})).await.unwrap();
        let mut spoken=String::new();while spoken.len()<text.len(){if let Some(DataChannelEvent::OnMessage(message))=channel.poll().await{let event:Value=serde_json::from_slice(&message.data).unwrap();assert_eq!(event["type"],"session.context.append");assert_eq!(event["channel"],"speakable");let part=event["content"][0]["text"].as_str().unwrap();assert!(part.len()<=500);spoken.push_str(part);}}assert_eq!(spoken,text);
        // Incoming RTP must stay close to its source clock, including timer jitter.
        // The old `next = now + duration` accumulated ~1 second of lag in this stream.
        let started=Instant::now();let frames=90;
        let send=async{
            let mut encoder=opus::Encoder::new().unwrap();let mut tick=tokio::time::interval(Duration::from_millis(20));
            for index in 0..frames {
                tick.tick().await;
                if index % 12 == 4 {
                    // Late pairs arrive together on the following ticks.
                    tokio::time::sleep(Duration::from_millis(85)).await;
                }
                let pcm: Vec<i16> = (0..320).map(|sample| {
                    let phase = (index * 320 + sample) as f64 * std::f64::consts::TAU * 220.0 / 16000.0;
                    (phase.sin() * 5000.0) as i16
                }).collect();
                let packet = encoder.encode(&pcm).unwrap();
                remote_track.write_sample(remote_ssrc, 111, &rtc::media::Sample {
                    data: packet.into(), duration: Duration::from_millis(20),
                    ..rtc::media::Sample::new(Instant::now())
                }, &[]).await.unwrap();
            }
        };
        let receive=async {
            let mut received = 0;
            let mut heartbeats = 0;
            while received < frames {
                let event = event_rx.recv().await.unwrap();
                match event["kind"].as_str() {
                    Some("audio") => received += 1,
                    Some("heard") => {
                        if heartbeats == 0 {
                            assert!(received < 5, "The first voiced frame must report activity immediately");
                        }
                        heartbeats += 1;
                    }
                    _ => {}
                }
            }
            assert!((2..=12).contains(&heartbeats), "Unexpected voice activity rate: {heartbeats}");
        };
        tokio::join!(send,receive);
        assert!(started.elapsed()<Duration::from_millis(frames*20+400),"RTP playout drifted: {:?}",started.elapsed());
        provider.request("voice.close",json!({"id":"local-call"})).await.unwrap();
        {let reports=reports.lock().unwrap();let report=reports.iter().rev().find(|r|r["report"]["closed"]==true).expect("final stats");
        assert_eq!(report["id"],"test-account-id");assert_eq!(report["report"]["opened"],true);
        assert!(report["report"]["sent_ms"].as_u64().unwrap()>=20);
        assert_eq!(report["report"]["received_ms"],frames*20);
        assert!(report["report"]["connected_ms"].as_u64().unwrap()>=1500);assert!(!report.to_string().contains("test-access"));}
        assert!(provider.request("voice.feed",json!({"id":"local-call","pcm":""})).await.is_err());remote.close().await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn voice_warm_builds_standby_without_socks_traffic_and_open_consumes_it() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let socks_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_addr = socks_listener.local_addr().unwrap();
        let dummy_services: Callback = Arc::new(|_, _| Box::pin(async { Ok(Value::Null) }));
        let standby = StandbyPeer::build(
            &dummy_services,
            json!({"socks": socks_addr.to_string(), "user": "user", "pass": "pass", "udp": true}),
        ).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(60), socks_listener.accept()).await.is_err());
        standby.close().await;

        let (events, mut event_rx) = tokio::sync::mpsc::channel(32);
        let services: Callback = Arc::new(move |method, params| {
            let events = events.clone();
            Box::pin(async move {
                match method.as_str() {
                    "network" => Ok(json!({})),
                    "auth.get" | "accounts.prepare" => Ok(json!({"access":"test-access","account":"test-account","refresh":"test-refresh","expires":super::super::now()+3600})),
                    "accounts.ok" => Ok(Value::Null),
                    "voice.event" => { events.send(params).await.map_err(|e| e.to_string())?; Ok(json!(true)) }
                    _ => Err("unexpected service".into()),
                }
            })
        });
        let mut provider = ChatGpt::new(services);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        provider.call_url = format!("http://{}/call", listener.local_addr().unwrap());

        let (channels, mut channel_rx) = tokio::sync::mpsc::channel(1);
        let (audio, _audio_rx) = tokio::sync::mpsc::channel(8);
        let gathered = Arc::new(tokio::sync::Notify::new());
        let handler = Arc::new(PeerHandler { gathered: gathered.clone(), channel: channels, audio });
        let mut media = MediaEngine::default();
        media.register_codec(RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec { mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), rtcp_feedback: vec![] },
            payload_type: 111,
            ..Default::default()
        }, RtpCodecKind::Audio).unwrap();
        let remote: Arc<dyn PeerConnection> = Arc::new(PeerConnectionBuilder::new().with_media_engine(media).with_handler(handler).with_runtime(Arc::new(webrtc::runtime::TokioRuntime)).with_udp_addrs(vec!["0.0.0.0:0".into()]).with_tcp_addrs(Vec::<String>::new()).build().await.unwrap());
        let _guard = Guard(Some(remote.clone()));
        let pc = remote.clone();
        let server = tokio::spawn(async move {
            for expect_head in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                let header_end = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend(&chunk[..n]);
                    if let Some(at) = request.windows(4).position(|v| v == b"\r\n\r\n") { break at + 4; }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                if expect_head {
                    assert!(headers.starts_with("head /call"));
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    continue;
                }
                let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:").map(str::trim)).unwrap().parse().unwrap();
                while request.len() < header_end + length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend(&chunk[..n]);
                }
                let body: Value = serde_json::from_slice(&request[header_end..header_end + length]).unwrap();
                pc.set_remote_description(RTCSessionDescription::offer(body["sdp"].as_str().unwrap().to_string()).unwrap()).await.unwrap();
                let answer = pc.create_answer(None).await.unwrap();
                pc.set_local_description(answer).await.unwrap();
                let _ = tokio::time::timeout(Duration::from_secs(3), gathered.notified()).await;
                let answer = pc.local_description().await.unwrap().sdp;
                socket.write_all(format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", answer.len(), answer).as_bytes()).await.unwrap();
            }
        });

        provider.request("voice.warm", Value::Null).await.unwrap();
        assert!(provider.has_standby(), "voice.warm must pre-build a standby peer");
        let opened = provider.request("voice.open", json!({"id":"warm-call","profile":{"voice":"cove"},"instructions":"hi"})).await.unwrap();
        assert_eq!(opened["requested_model"], LIVE_11);
        assert!(!provider.has_standby(), "voice.open must consume the standby peer");
        server.await.unwrap();
        let channel = channel_rx.recv().await.unwrap();
        channel.send_text(&json!({"type":"session.started","session":{"model":"gpt-live-1.1-codex"}}).to_string()).await.unwrap();
        while let Some(ev) = event_rx.recv().await {
            if ev["kind"] == "model" { break; }
        }
        provider.shutdown().await;
        remote.close().await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn voice_say_waits_for_data_channel_open_after_dtls_connect() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let services: Callback = Arc::new(|method, _params| {
            Box::pin(async move {
                match method.as_str() {
                    "network" => Ok(json!({})),
                    "auth.get" | "accounts.prepare" => Ok(json!({"access":"test-access","account":"test-account","refresh":"test-refresh","expires":super::super::now()+3600})),
                    "accounts.ok" => Ok(Value::Null),
                    "voice.event" => Ok(json!(true)),
                    _ => Err("unexpected service".into()),
                }
            })
        });
        let mut provider = ChatGpt::new(services);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        provider.call_url = format!("http://{}/call", listener.local_addr().unwrap());

        let (channels, mut channel_rx) = tokio::sync::mpsc::channel(1);
        let (audio, _audio_rx) = tokio::sync::mpsc::channel(8);
        let gathered = Arc::new(tokio::sync::Notify::new());
        let handler = Arc::new(PeerHandler { gathered: gathered.clone(), channel: channels, audio });
        let mut media = MediaEngine::default();
        media.register_codec(RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec { mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), rtcp_feedback: vec![] },
            payload_type: 111,
            ..Default::default()
        }, RtpCodecKind::Audio).unwrap();
        let remote: Arc<dyn PeerConnection> = Arc::new(PeerConnectionBuilder::new().with_media_engine(media).with_handler(handler).with_runtime(Arc::new(webrtc::runtime::TokioRuntime)).with_udp_addrs(vec!["0.0.0.0:0".into()]).with_tcp_addrs(Vec::<String>::new()).build().await.unwrap());
        let _guard = Guard(Some(remote.clone()));
        let pc = remote.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend(&chunk[..n]);
                if let Some(at) = request.windows(4).position(|v| v == b"\r\n\r\n") { break at + 4; }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
            let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:").map(str::trim)).unwrap().parse().unwrap();
            while request.len() < header_end + length {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend(&chunk[..n]);
            }
            let body: Value = serde_json::from_slice(&request[header_end..header_end + length]).unwrap();
            pc.set_remote_description(RTCSessionDescription::offer(body["sdp"].as_str().unwrap().to_string()).unwrap()).await.unwrap();
            let answer = pc.create_answer(None).await.unwrap();
            pc.set_local_description(answer).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), gathered.notified()).await;
            let answer = pc.local_description().await.unwrap().sdp;
            socket.write_all(format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", answer.len(), answer).as_bytes()).await.unwrap();
        });

        provider.request("voice.open", json!({"id":"say-race","profile":{"voice":"cove"},"instructions":"hi"})).await.unwrap();
        server.await.unwrap();
        provider.request("voice.say", json!({"id":"say-race","text":"сразу после dtls"})).await.unwrap();
        let channel = channel_rx.recv().await.unwrap();
        loop {
            if let Some(DataChannelEvent::OnMessage(message)) = channel.poll().await {
                let event: Value = serde_json::from_slice(&message.data).unwrap();
                assert_eq!(event["type"], "session.context.append");
                assert_eq!(event["content"][0]["text"], "сразу после dtls");
                break;
            }
        }
        provider.shutdown().await;
        remote.close().await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn rtp_playout_recovers_from_150ms_stall_without_dropping_packets() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (events, mut event_rx) = tokio::sync::mpsc::channel(256);
        let services: Callback = Arc::new(move |method, params| {
            let events = events.clone();
            Box::pin(async move {
                match method.as_str() {
                    "network" => Ok(json!({})),
                    "auth.get" | "accounts.prepare" => Ok(json!({"access":"test-access","account":"test-account","refresh":"test-refresh","expires":super::super::now()+3600})),
                    "accounts.ok" => Ok(Value::Null),
                    "voice.event" => { events.send(params).await.map_err(|e| e.to_string())?; Ok(json!(true)) }
                    _ => Err("unexpected service".into()),
                }
            })
        });
        let mut provider = ChatGpt::new(services);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        provider.call_url = format!("http://{}/call", listener.local_addr().unwrap());

        let (channels, mut channel_rx) = tokio::sync::mpsc::channel(1);
        let (audio, _audio_rx) = tokio::sync::mpsc::channel(8);
        let gathered = Arc::new(tokio::sync::Notify::new());
        let handler = Arc::new(PeerHandler { gathered: gathered.clone(), channel: channels, audio });
        let mut media = MediaEngine::default();
        media.register_codec(RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec { mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), rtcp_feedback: vec![] },
            payload_type: 111,
            ..Default::default()
        }, RtpCodecKind::Audio).unwrap();
        let remote: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_media_engine(media)
                .with_handler(handler)
                .with_runtime(Arc::new(webrtc::runtime::TokioRuntime))
                .with_udp_addrs(vec!["0.0.0.0:0".into()])
                .with_tcp_addrs(Vec::<String>::new())
                .build()
                .await
                .unwrap(),
        );
        let remote_ssrc = 0x54455355;
        let remote_codec = RTCRtpCodec { mime_type: "audio/opus".into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), rtcp_feedback: vec![] };
        let remote_track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new("peer".into(), "audio".into(), "peer".into(), RtpCodecKind::Audio, vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(remote_ssrc), ..Default::default() },
                codec: remote_codec,
                ..Default::default()
            }]),
        ).unwrap());
        remote.add_track(remote_track.clone() as Arc<dyn TrackLocal>).await.unwrap();
        let _guard = Guard(Some(remote.clone()));
        let pc = remote.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend(&chunk[..n]);
                if let Some(at) = request.windows(4).position(|v| v == b"\r\n\r\n") { break at + 4; }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
            let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:").map(str::trim)).unwrap().parse().unwrap();
            while request.len() < header_end + length {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend(&chunk[..n]);
            }
            let body: Value = serde_json::from_slice(&request[header_end..header_end + length]).unwrap();
            pc.set_remote_description(RTCSessionDescription::offer(body["sdp"].as_str().unwrap().to_string()).unwrap()).await.unwrap();
            let answer = pc.create_answer(None).await.unwrap();
            pc.set_local_description(answer).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), gathered.notified()).await;
            let answer = pc.local_description().await.unwrap().sdp;
            socket.write_all(format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", answer.len(), answer).as_bytes()).await.unwrap();
        });

        provider.request("voice.open", json!({"id":"stall-call","profile":{"voice":"cove"},"instructions":"hi"})).await.unwrap();
        server.await.unwrap();
        let _channel = channel_rx.recv().await.unwrap();

        let frames = 60usize;
        let started = Instant::now();
        let send = async {
            let mut encoder = opus::Encoder::new().unwrap();
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            for index in 0..frames {
                tick.tick().await;
                if index == 20 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                let pcm: Vec<i16> = (0..320).map(|sample| {
                    let phase = (index * 320 + sample) as f64 * std::f64::consts::TAU * 220.0 / 16000.0;
                    (phase.sin() * 5000.0) as i16
                }).collect();
                let packet = encoder.encode(&pcm).unwrap();
                remote_track.write_sample(remote_ssrc, 111, &rtc::media::Sample {
                    data: packet.into(),
                    duration: Duration::from_millis(20),
                    ..rtc::media::Sample::new(Instant::now())
                }, &[]).await.unwrap();
            }
        };
        let receive = async {
            let mut received = 0usize;
            while received < frames {
                let event = event_rx.recv().await.unwrap();
                if event["kind"].as_str() == Some("audio") {
                    received += 1;
                }
            }
            received
        };
        let (_, received) = tokio::join!(send, receive);
        assert_eq!(received, frames);
        assert!(started.elapsed() < Duration::from_millis(frames as u64 * 20 + 600), "stalled playout drifted: {:?}", started.elapsed());
        provider.shutdown().await;
        remote.close().await.unwrap();
    }).await.unwrap();
}
