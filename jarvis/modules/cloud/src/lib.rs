//! Optional cloud providers. Each provider is independently selected at compile time.
use serde_json::{Value,json};
use std::{time::Duration,net::SocketAddr,sync::Mutex};
use base64::{Engine,engine::general_purpose::STANDARD};
pub type Socket=tokio_tungstenite::WebSocketStream<reqwest::Upgraded>;
pub use tokio_tungstenite::tungstenite::Message;

pub struct Provider {
    id:&'static str,
    base:reqwest::Url,
    client:Mutex<Option<(Value,reqwest::Client)>>,
    websocket:Mutex<Option<(Value,reqwest::Client)>>,
}
impl Provider {
    pub fn new(id:&str)->Result<Self,String> {
        let (id,url):(&'static str,&str)=match id {
            #[cfg(feature="openai")] "openai"=>("openai","https://api.openai.com/"),
            #[cfg(feature="gemini")] "gemini"=>("gemini","https://generativelanguage.googleapis.com/"),
            #[cfg(feature="grok")] "grok"=>("grok","https://api.x.ai/"),
            #[cfg(feature="elevenlabs")] "elevenlabs"=>("elevenlabs","https://api.elevenlabs.io/"),
            _=>return Err("Провайдер не включён в сборку".into()),
        };
        Ok(Self{id,base:reqwest::Url::parse(url).map_err(|e|e.to_string())?,client:Mutex::default(),websocket:Mutex::default()})
    }
    pub fn id(&self)->&'static str{self.id}
    pub async fn socket(&self,key:&str,network:&Value)->Result<Socket,String>{
        self.socket_for(key,network,None).await
    }
    pub async fn socket_for(&self,key:&str,network:&Value,session:Option<&Value>)->Result<Socket,String>{
        use tokio_tungstenite::tungstenite::{handshake::{client::generate_key,derive_accept_key},protocol::{Role,WebSocketConfig}};
        if key.is_empty() || key.len()>4096 || key.bytes().any(|b|b<33 || b>126){return Err("Неверный API-ключ".into());}
        let secret=if self.id=="elevenlabs"{
            let session=session.filter(|s|s.is_object()).ok_or("Нужны настройки ElevenLabs Realtime")?;
            let value=self.request(json!({"key":key,"op":"realtime.session","body":{"session":session,"expires_after":{"seconds":600}}}),network.clone()).await?;
            Some(value["value"].as_str().filter(|s|!s.is_empty()&&s.len()<=4096&&s.bytes().all(|b|(33..=126).contains(&b))).ok_or("ElevenLabs не выдал вход для сессии")?.to_string())
        }else{None};
        let path=match self.id{
            #[cfg(feature="openai")] "openai"=>"v1/live/sessions",
            #[cfg(feature="grok")] "grok"=>"v1/realtime",
            #[cfg(feature="gemini")] "gemini"=>"ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent",
            #[cfg(feature="elevenlabs")] "elevenlabs"=>"v1/convai/realtime",
            _=>return Err("Нет голосового WebSocket у этого провайдера".into()),
        };
        let mut url=self.base.join(path).map_err(|_|"Неверный адрес провайдера")?;
        if self.id=="gemini"{url.query_pairs_mut().append_pair("key",key);}
        let nonce=generate_key();let expected=derive_accept_key(nonce.as_bytes());
        let request=self.client_for(network, true)?.get(url).header("Connection","Upgrade").header("Upgrade","websocket").header("Sec-WebSocket-Version","13").header("Sec-WebSocket-Key",nonce);
        let request=if self.id=="gemini"{request}else if let Some(secret)=secret{request.bearer_auth(secret)}else{self.authenticate(request,key)};
        let response = request
            .send()
            .await
            .map_err(|e| classify_net_error("Не подключиться к голосовому провайдеру", &e))?;
        if response.status()!=101 || response.headers().get("Sec-WebSocket-Accept").and_then(|h|h.to_str().ok())!=Some(expected.as_str()) || response.headers().contains_key("Sec-WebSocket-Extensions") || response.headers().contains_key("Sec-WebSocket-Protocol"){
            let status = response.status();
            let retry_after = header_retry_after(response.headers());
            let mut response = response;
            let bytes = read_body(&mut response, 16 * 1024).await.unwrap_or_default();
            let detail = format_http_error_detail(key, retry_after, &bytes);
            if self.id == "gemini" && status.as_u16() == 404 {
                return Err(if detail.is_empty() {
                    "Gemini Live вернул HTTP 404 на адресе голосового WebSocket".into()
                } else {
                    format!("Gemini Live вернул HTTP 404 на адресе голосового WebSocket: {detail}")
                });
            }
            return Err(if detail.is_empty() {
                format!("{}: голосовой WebSocket отклонён (HTTP {})", self.id, status.as_u16())
            } else {
                format!("{}: голосовой WebSocket отклонён (HTTP {}) {detail}", self.id, status.as_u16())
            });
        }
        let upgraded=response.upgrade().await.map_err(|_|"Обрыв голосового соединения")?;
        let config=WebSocketConfig::default().max_message_size(Some(4*1024*1024)).max_frame_size(Some(4*1024*1024)).write_buffer_size(16*1024).max_write_buffer_size(256*1024);
        Ok(tokio_tungstenite::WebSocketStream::from_raw_socket(upgraded,Role::Client,Some(config)).await)
    }
    fn client_for(&self,network:&Value,websocket:bool)->Result<reqwest::Client,String> {
        if let Some(error)=network["error"].as_str(){return Err(error.into());}
        if !network.is_object(){return Err("Хост не предоставил сетевой маршрут".into());}
        let mut saved=if websocket { &self.websocket } else { &self.client }.lock().unwrap();
        if let Some((route,client))=saved.as_ref(){if route==network{return Ok(client.clone());}}
        let mut builder=reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(60)).connect_timeout(Duration::from_secs(10));
        // Forcing the request version does not disable HTTP/2 negotiation over TLS.
        if websocket { builder = builder.http1_only().pool_max_idle_per_host(0); }
        if let Some(socks)=network["socks"].as_str(){
            let addr:SocketAddr=socks.parse().map_err(|_|"Неверный SOCKS VPN")?;
            let user=network["user"].as_str().unwrap_or("");let pass=network["pass"].as_str().unwrap_or("");
            if !addr.ip().is_loopback() || addr.port()==0 || user.is_empty() || pass.is_empty(){return Err("Неверный SOCKS VPN".into());}
            builder=builder.proxy(reqwest::Proxy::all(format!("socks5h://{socks}")).map_err(|_|"Неверный SOCKS VPN")?.basic_auth(user,pass));
        }
        let client=builder.build().map_err(|_|"Не создать HTTP-клиент")?;
        *saved=Some((network.clone(),client.clone()));Ok(client)
    }
    pub fn close(&self){self.client.lock().unwrap().take();self.websocket.lock().unwrap().take();}
    fn authenticate(&self,request:reqwest::RequestBuilder,key:&str)->reqwest::RequestBuilder {
        match self.id {"gemini"=>request.header("x-goog-api-key",key),"elevenlabs"=>request.header("xi-api-key",key),_=>request.bearer_auth(key)}
    }
    pub async fn request(&self,params:Value,network:Value)->Result<Value,String> {
        let key=params["key"].as_str().unwrap_or("").trim();
        if key.is_empty() || key.len()>4096 || key.bytes().any(|b|b<33 || b>126){return Err("Неверный API-ключ".into());}
        let client=self.client_for(&network, false)?;
        let op=params["op"].as_str().unwrap_or("");
        if matches!(op,"text"|"tts"|"realtime.session") && !params["body"].is_object(){return Err("Нужен объект body".into());}
        if op=="realtime.session" && params["body"].to_string().len()>128*1024{return Err("Слишком большие настройки сессии".into());}
        let model=params["model"].as_str().unwrap_or("");
        let segment=|value:&str| !value.is_empty() && value.len()<=180 && value.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.".contains(&b)) && !matches!(value,"."|"..");
        let mut binary=false;
        let request=match (self.id,op) {
            #[cfg(feature="elevenlabs")]
            ("elevenlabs","realtime.session")=>client.post(self.base.join("v1/convai/realtime/client_secrets").unwrap()).json(&params["body"]),
            (_,"settings.validate"|"models")=>{
                let path=match self.id {"gemini"=>"v1beta/models","elevenlabs"=>"v1/user/subscription",_=>"v1/models"};
                client.get(self.base.join(path).map_err(|e|e.to_string())?)
            },
            #[cfg(feature="openai")]
            ("openai","text")=>client.post(self.base.join("v1/responses").unwrap()).json(&params["body"]),
            #[cfg(feature="grok")]
            ("grok","text")=>client.post(self.base.join("v1/chat/completions").unwrap()).json(&params["body"]),
            #[cfg(feature="gemini")]
            ("gemini","text")=>{
                if !segment(model){return Err("Неверная модель Gemini".into());}
                client.post(self.base.join(&format!("v1beta/models/{model}:generateContent")).unwrap()).json(&params["body"])
            },
            #[cfg(feature="openai")]
            ("openai","tts")=>{
                binary=true;let mut body=params["body"].clone();body["response_format"]=json!("pcm");
                client.post(self.base.join("v1/audio/speech").unwrap()).json(&body)
            },
            #[cfg(feature="elevenlabs")]
            ("elevenlabs","tts")=>{
                let voice=params["voice"].as_str().unwrap_or("");if !segment(voice){return Err("Неверный ID голоса".into());}
                binary=true;client.post(self.base.join(&format!("v1/text-to-speech/{voice}")).unwrap()).query(&[("output_format","pcm_24000")]).json(&params["body"])
            },
            #[cfg(feature="elevenlabs")]
            ("elevenlabs","voices")=>client.get(self.base.join("v2/voices").unwrap()).query(&[("page_size","100")]),
            #[cfg(any(feature="openai",feature="elevenlabs"))]
            ("openai"|"elevenlabs","stt")=>{
                let data=STANDARD.decode(params["audio"].as_str().unwrap_or("")).map_err(|_|"Неверный звук STT")?;
                if data.len()>8*1024*1024 || data.is_empty(){return Err("Звук STT: от 1 байта до 8 МБ".into());}
                let path=if self.id=="openai" {"v1/audio/transcriptions"}else{"v1/speech-to-text"};
                let field=if self.id=="openai" {"model"}else{"model_id"};
                if !segment(model){return Err("Неверная модель STT".into());}
                let mut form=reqwest::multipart::Form::new().text(field,model.to_string()).part("file",reqwest::multipart::Part::bytes(data).file_name("speech.wav").mime_str("audio/wav").unwrap());
                if let Some(language)=params["language"].as_str().filter(|s|!s.is_empty()) {form=form.text(if self.id=="openai"{"language"}else{"language_code"},language.to_string());}
                client.post(self.base.join(path).unwrap()).multipart(form)
            },
            _=>return Err(format!("{} не поддерживает операцию {op}",self.id)),
        };
        let send_error = |error: reqwest::Error| classify_net_error("Нет соединения с провайдером", &error);
        let mut response = self.authenticate(request, key).send().await.map_err(send_error)?;
        let mut status = response.status();
        let mut retry_after = header_retry_after(response.headers());
        let limit=if binary {16*1024*1024}else{4*1024*1024};
        let mut bytes = read_body(&mut response, limit).await?;
        if self.id == "gemini" && op == "text" && gemini_rejected_key(status, &bytes) {
            // Match the working Live API credential transport and keep the same network route.
            let url = self.base.join(&format!("v1beta/models/{model}:generateContent")).unwrap();
            let mut response = client.post(url)
                .query(&[("key", key)])
                .json(&params["body"])
                .send().await.map_err(send_error)?;
            status = response.status();
            retry_after = header_retry_after(response.headers());
            bytes = read_body(&mut response, limit).await?;
        }
        if !status.is_success(){
            let detail = format_http_error_detail(key, retry_after, &bytes);
            return Err(format!("{} ответил {}: {detail}",self.id,status.as_u16()));
        }
        if binary {if bytes.len()%2!=0{return Err("Неверный PCM от провайдера".into());}return Ok(json!({"audio":STANDARD.encode(bytes),"rate":24000}));}
        let value:Value=serde_json::from_slice(&bytes).map_err(|_|"Провайдер вернул неверный JSON")?;
        if op=="settings.validate" {
            let valid=match self.id {"gemini"=>value["models"].is_array(),"elevenlabs"=>value["character_limit"].is_number(),_=>value["data"].is_array()};
            if !valid{return Err("Провайдер не подтвердил API-ключ".into());}
        }
        Ok(value)
    }
}

fn classify_net_error(prefix: &str, error: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut detail = error.to_string().to_ascii_lowercase();
    let mut src = error.source();
    while let Some(s) = src {
        detail.push(' ');
        detail.push_str(&s.to_string().to_ascii_lowercase());
        src = s.source();
    }
    if error.is_timeout() {
        format!("{prefix}: провайдер не ответил вовремя")
    } else if detail.contains("socks") || detail.contains("proxy") {
        format!("{prefix}: ошибка VPN-прокси")
    } else if detail.contains("dns") || detail.contains("lookup") || detail.contains("resolve") {
        format!("{prefix}: ошибка DNS")
    } else if detail.contains("tls")
        || detail.contains("handshake")
        || detail.contains("certificate")
        || detail.contains("ssl")
    {
        format!("{prefix}: ошибка TLS-соединения")
    } else {
        prefix.to_string()
    }
}

fn header_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    raw.parse::<u64>().ok().filter(|&s| s > 0)
}

fn format_http_error_detail(key: &str, retry_after: Option<u64>, bytes: &[u8]) -> String {
    let data: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let mut tags = Vec::new();
    if let Some(secs) = retry_after {
        tags.push(format!("retry={secs}s"));
    }
    if let Some(details) = data.pointer("/error/details").and_then(Value::as_array) {
        for detail in details {
            if let Some(delay) = detail.get("retryDelay").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                if !tags.iter().any(|t| t.starts_with("retry=")) {
                    tags.push(format!("retry={delay}"));
                }
            }
            if let Some(reason) = detail.get("reason").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                tags.push(format!("reason={reason}"));
            }
            if let Some(violations) = detail.get("violations").and_then(Value::as_array) {
                for v in violations.iter().take(2) {
                    if let Some(qid) = v.get("quotaId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        tags.push(format!("quota={qid}"));
                    }
                    if let Some(limit) = v.get("quotaValue").and_then(Value::as_str).filter(|s| *s == "0") {
                        tags.push(format!("limit: {limit}"));
                    }
                }
            }
        }
    }
    if let Some(status) = data.pointer("/error/status").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        if !tags.iter().any(|t| t.contains(status)) {
            tags.push(status.to_string());
        }
    }
    let message = data
        .pointer("/error/message")
        .or_else(|| data.pointer("/detail/message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace(key, "***");
    let message: String = message.chars().take(220).collect();
    if tags.is_empty() {
        message
    } else if message.is_empty() {
        format!("[{}]", tags.join(", "))
    } else {
        format!("[{}] {message}", tags.join(", "))
    }
}

async fn read_body(response: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "Оборван ответ провайдера")? {
        if bytes.len() + chunk.len() > limit {
            return Err("Слишком большой ответ провайдера".into());
        }
        bytes.extend(chunk);
    }
    Ok(bytes)
}

fn gemini_rejected_key(status: reqwest::StatusCode, bytes: &[u8]) -> bool {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return true;
    }
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let Ok(body) = serde_json::from_slice::<Value>(bytes) else { return false };
    body["error"]["details"].as_array().is_some_and(|details| {
        details.iter().any(|detail| detail["reason"] == "API_KEY_INVALID")
    }) || body["error"]["message"].as_str().is_some_and(|message| {
        message.to_ascii_lowercase().contains("api key not valid")
    })
}

#[cfg(all(test, feature = "gemini"))]
mod gemini_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn gemini_text_retries_rejected_header_using_live_key_transport() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider = Provider::new("gemini").unwrap();
        provider.base = format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap();
        let server = tokio::spawn(async move {
            for header_auth in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                while !raw.ends_with(b"\r\n\r\n") {
                    raw.push(socket.read_u8().await.unwrap());
                    assert!(raw.len() < 8192);
                }
                let header = String::from_utf8(raw).unwrap().to_ascii_lowercase();
                assert_eq!(header.contains("x-goog-api-key: test-key"), header_auth);
                assert_eq!(header.contains("?key=test-key"), !header_auth);
                assert!(!header.contains("authorization:"));
                let length = header.lines().find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap().trim().parse().unwrap();
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                assert_eq!(body, br#"{"contents":[]}"#);
                let (status, body) = if header_auth {
                    ("400 Bad Request", r#"{"error":{"details":[{"reason":"API_KEY_INVALID"}]}}"#)
                } else {
                    ("200 OK", r#"{"candidates":[]}"#)
                };
                let http = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(http.as_bytes()).await.unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let result = provider.request(json!({"op":"text","key":"test-key",
                "model":"gemini-test","body":{"contents":[]}}), json!({})).await.unwrap();
            assert_eq!(result, json!({"candidates":[]}));
            server.await.unwrap();
        }).await.unwrap();
        assert!(!gemini_rejected_key(reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"Unknown name parameters"}}"#));
    }

    #[test]
    fn extracts_retry_after_and_quota_ids_without_leaking_key() {
        let body = br#"{
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "message": "Quota exceeded for key AIzaSecretKey999",
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{"quotaId": "GenerateRequestsPerMinutePerProjectPerModel"}]},
                    {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "42s"}
                ]
            }
        }"#;
        let detail = format_http_error_detail("AIzaSecretKey999", None, body);
        assert!(detail.contains("retry=42s"));
        assert!(detail.contains("quota=GenerateRequestsPerMinutePerProjectPerModel"));
        assert!(!detail.contains("AIzaSecretKey999"));
        assert!(detail.contains("***"));
    }
}

#[cfg(all(test,feature="openai"))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt,AsyncWriteExt};
    #[cfg(feature="elevenlabs")]
    #[tokio::test]
    async fn eleven_mints_session_then_uses_only_ephemeral_bearer_for_websocket(){
        use futures_util::{SinkExt,StreamExt};
        tokio::time::timeout(Duration::from_secs(5),async{
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let mut provider=Provider::new("elevenlabs").unwrap();provider.base=format!("http://{}/",listener.local_addr().unwrap()).parse().unwrap();
            let config=json!({"type":"realtime","instructions":"test","tools":[],"audio":{"input":{"format":{"rate":24000}}}});
            assert!(provider.socket_for("private-api-key",&json!({"error":"VPN blocked"}),Some(&config)).await.unwrap_err().contains("VPN blocked"));
            let server=tokio::spawn(async move{
                let (mut socket,_)=listener.accept().await.unwrap();let mut bytes=vec![];while !bytes.ends_with(b"\r\n\r\n"){bytes.push(socket.read_u8().await.unwrap());assert!(bytes.len()<8192);}
                let header=String::from_utf8_lossy(&bytes).to_lowercase();assert!(header.starts_with("post /v1/convai/realtime/client_secrets "));assert!(header.contains("xi-api-key: private-api-key"));
                let length:usize=header.lines().find_map(|l|l.strip_prefix("content-length:").map(str::trim)).unwrap().parse().unwrap();let mut body=vec![0;length];socket.read_exact(&mut body).await.unwrap();let body:Value=serde_json::from_slice(&body).unwrap();assert_eq!(body["session"]["instructions"],"test");assert_eq!(body["expires_after"]["seconds"],600);
                let body=json!({"value":"one-use-secret"}).to_string();socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();drop(socket);
                let (socket,_)=listener.accept().await.unwrap();let mut socket=tokio_tungstenite::accept_hdr_async(socket,|request:&tokio_tungstenite::tungstenite::handshake::server::Request,response|{assert_eq!(request.uri().path(),"/v1/convai/realtime");assert_eq!(request.headers()["authorization"],"Bearer one-use-secret");assert!(!request.headers().contains_key("xi-api-key"));Ok(response)}).await.unwrap();
                socket.send(json!({"type":"session.created"}).to_string().into()).await.unwrap();let packet=socket.next().await.unwrap().unwrap();let packet:Value=serde_json::from_slice(&packet.into_data()).unwrap();assert_eq!(packet["type"],"input_audio_buffer.append");socket.send(json!({"type":"response.audio.delta","delta":"AAA="}).to_string().into()).await.unwrap();
            });
            let mut socket=provider.socket_for("private-api-key",&json!({}),Some(&config)).await.unwrap();assert_eq!(serde_json::from_slice::<Value>(&socket.next().await.unwrap().unwrap().into_data()).unwrap()["type"],"session.created");
            socket.send(json!({"type":"input_audio_buffer.append","audio":"AAA="}).to_string().into()).await.unwrap();let reply:Value=serde_json::from_slice(&socket.next().await.unwrap().unwrap().into_data()).unwrap();assert_eq!(reply["delta"],"AAA=");server.await.unwrap();
        }).await.unwrap();
    }
    #[tokio::test]
    async fn reuses_http_connection_without_reusing_request_credentials(){
        tokio::time::timeout(Duration::from_secs(5),async{
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let mut provider=Provider::new("openai").unwrap();provider.base=format!("http://{}/",listener.local_addr().unwrap()).parse().unwrap();
            let server=tokio::spawn(async move{let (mut socket,_)=listener.accept().await.unwrap();for key in ["first-key","second-key"]{let mut header=Vec::new();while !header.ends_with(b"\r\n\r\n"){let b=socket.read_u8().await.unwrap();header.push(b);assert!(header.len()<8192);}assert!(String::from_utf8_lossy(&header).to_lowercase().contains(&format!("authorization: bearer {key}")));socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"data\":[]}").await.unwrap();}});
            for key in ["first-key","second-key"]{provider.request(json!({"key":key,"op":"models"}),json!({})).await.unwrap();}
            server.await.unwrap();provider.close();assert!(provider.client.lock().unwrap().is_none());
        }).await.unwrap();
    }
    #[tokio::test]
    async fn authenticates_and_preserves_response_and_never_bypasses_blocked_vpn(){
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let mut provider=Provider::new("openai").unwrap();provider.base=format!("http://{addr}/").parse().unwrap();
        assert!(provider.request(json!({"key":"test-key","op":"models"}),json!({"error":"VPN blocked"})).await.unwrap_err().contains("VPN blocked"));
        let server=tokio::spawn(async move {let (mut socket,_)=listener.accept().await.unwrap();let mut request=[0;8192];let n=socket.read(&mut request).await.unwrap();let text=String::from_utf8_lossy(&request[..n]).to_lowercase();assert!(text.contains("authorization: bearer test-key"));assert!(text.starts_with("get /v1/models "));socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}").await.unwrap();});
        assert_eq!(provider.request(json!({"key":"test-key","op":"settings.validate"}),json!({})).await.unwrap(),json!({"data":[]}));server.await.unwrap();
    }
    #[tokio::test]
    async fn websocket_upgrade_uses_provider_route_and_validates_handshake(){
        use futures_util::{SinkExt,StreamExt};
        use tokio_tungstenite::tungstenite::{Message,handshake::server::{Request,Response}};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let mut provider=Provider::new("openai").unwrap();provider.base=format!("http://{addr}/").parse().unwrap();
        let server=tokio::spawn(async move {
            let (socket,_)=listener.accept().await.unwrap();
            let mut ws=tokio_tungstenite::accept_hdr_async(socket,|request:&Request,response:Response|{assert_eq!(request.uri().path(),"/v1/live/sessions");assert_eq!(request.headers()["authorization"],"Bearer test-key");Ok(response)}).await.unwrap();
            assert_eq!(ws.next().await.unwrap().unwrap().into_text().unwrap(),"hello");ws.send(Message::Text("ok".into())).await.unwrap();
        });
        let mut ws=provider.socket("test-key",&json!({})).await.unwrap();ws.send(Message::Text("hello".into())).await.unwrap();assert_eq!(ws.next().await.unwrap().unwrap().into_text().unwrap(),"ok");server.await.unwrap();
    }
}
