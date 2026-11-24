//! Credentials are validated by the selected provider before committing settings.
use super::{App,bad,ok};
use crate::settings::{Settings,ElevenKey};
use axum::{Json,extract::State,response::Response};
use serde_json::{Value,json};
use std::time::Duration;

fn snapshot(s:&Settings,service:&str)->Value {
    match service {
        "openai"=>json!(s.openai_key),"grok"=>json!(s.grok_key),"yandex"=>json!(s.yandex_token),
        "gemini"=>json!([s.gemini_key,s.gemini_keys]),"elevenlabs"=>json!([s.elevenlabs_key,s.eleven_keys]),_=>Value::Null,
    }
}
fn assign(s:&mut Settings,service:&str,key:String){
    let auto_select = !key.is_empty();
    match service {
        "openai"=>s.openai_key=key,"grok"=>s.grok_key=key,
        "yandex"=>{let was=s.home_connected("yandex");s.yandex_token=key;if !was && s.home_connected("yandex"){s.home_use.remove("yandex");}},
        "gemini"=>{
            if key.is_empty(){s.gemini_key.clear();s.gemini_keys.clear();}
            else {if !s.gemini_keys.contains(&key){s.gemini_keys.push(key.clone());}if s.gemini_key.is_empty(){s.gemini_key=key;}}
        },
        "elevenlabs"=>{
            if key.is_empty(){s.elevenlabs_key.clear();s.eleven_keys.clear();}
            else {if !s.eleven_keys.iter().any(|k|k.key==key){s.eleven_keys.push(ElevenKey{key:key.clone(),..Default::default()});}if s.elevenlabs_key.is_empty(){s.elevenlabs_key=key;}}
        },_=>{},
    }
    if auto_select {
        s.auto_select_engine(service);
    }
}
pub async fn put(State(app):State<App>,Json(v):Json<Value>)->Response {
    let Some(values)=v.as_object().filter(|v|v.len()==1)else{return bad("Нужно одно подключение");};
    let (service,value)=values.iter().next().unwrap();
    if !["openai","gemini","grok","elevenlabs","yandex"].contains(&service.as_str()){return bad("Неизвестное подключение");}
    let Some(key)=value.as_str()else{return bad("Ключ должен быть строкой");};let key=key.trim().to_string();
    let before=app.store.with(|s|snapshot(s,service));
    if !key.is_empty(){
        let capability=if service=="yandex"{"yandexhome"}else{service.as_str()};
        let provider=match app.plugins.provider(capability){Ok(p)=>p,Err(e)=>return bad(e)};
        let params=if service=="yandex"{json!({"op":"settings.validate","token":key})}else{json!({"op":"settings.validate","key":key})};
        if let Err(e)=provider.request(capability,params,Duration::from_secs(20)).await{return bad(e);}
    }
    let gemini_key_for_pool = (service == "gemini").then(|| key.clone());
    match app.store.update(|s|->Result<(),std::io::Error>{
        if snapshot(s,service)!=before{return Err(std::io::Error::other("Подключение изменилось; повторите сохранение"));}
        assign(s,service,key);Ok(())
    }){
        Ok(())=>{
            if let Some(k) = gemini_key_for_pool {
                if k.is_empty() {
                    app.engine.gemini_pool.clear_all();
                } else {
                    app.engine.gemini_pool.reset_key(&k);
                }
            }
            ok()
        }
        Err(e)=>bad(e.to_string())
    }
}
pub async fn gemini_pool(State(app):State<App>,Json(v):Json<Value>)->Response{pool(&app,"gemini",v)}
pub async fn eleven_pool(State(app):State<App>,Json(v):Json<Value>)->Response{pool(&app,"elevenlabs",v)}
pub async fn eleven_voices(State(app):State<App>)->Response{
    let key=app.store.with(|s|s.elevenlabs_key.clone());
    let provider=match app.plugins.provider("elevenlabs"){Ok(p)=>p,Err(e)=>return bad(e)};
    match provider.request("elevenlabs",json!({"op":"voices","key":key}),Duration::from_secs(25)).await{
        Ok(value)=>super::reply(axum::http::StatusCode::OK,json!({"voices":value["voices"],"models":[{"id":crate::settings::ELEVEN_DEFAULT_MODEL,"name":"Eleven Flash v2.5"},{"id":"eleven_multilingual_v2","name":"Eleven Multilingual v2"},{"id":"eleven_v3","name":"Eleven v3"}]})),
        Err(e)=>bad(e),
    }
}
pub async fn tts_voices(
    State(app): State<App>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let provider = query.get("provider").map(String::as_str).unwrap_or("elevenlabs").trim();
    if provider == "elevenlabs" {
        return eleven_voices(State(app)).await;
    }
    if !crate::plugins::valid_id(provider) {
        return bad("Неверный идентификатор плагина озвучки");
    }
    match app.plugins.plugin_tts_voices(provider).await {
        Ok(voices) => super::reply(axum::http::StatusCode::OK, json!({"voices": voices})),
        Err(e) => bad(e),
    }
}
fn validate_voice_input<'a>(name: &'a str, prompt: &'a str) -> Result<(&'a str, &'a str), String> {
    let name = name.trim();
    let prompt = prompt.trim();
    if name.is_empty()
        || name.chars().count() > 80
        || prompt.chars().count() < 10
        || prompt.chars().count() > 2000
    {
        return Err("Нужны название и описание голоса (хотя бы пара фраз)".into());
    }
    Ok((name, prompt))
}

pub async fn create_voice(State(app): State<App>, Json(v): Json<Value>) -> Response {
    let (name, prompt) = match validate_voice_input(
        v["name"].as_str().unwrap_or(""),
        v["prompt"].as_str().unwrap_or(""),
    ) {
        Ok(pair) => pair,
        Err(e) => return bad(e),
    };
    let openai_key = app.store.with(|s| s.openai_key.clone());
    if openai_key.is_empty() {
        return bad("Добавьте ключ OpenAI в «Подключениях»");
    }
    let client = match super::vpn::route_client(&app.proxy.route("openai"), Duration::from_secs(60)) {
        Ok(c) => c,
        Err(e) => return bad(e),
    };
    let mut resp = match client
        .post("https://api.openai.com/v1/audio/voices")
        .bearer_auth(&openai_key)
        .json(&json!({"type": "prompt", "name": name, "prompt": prompt, "model": "auto"}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return bad(crate::net::short(&e)),
    };
    let status = resp.status();
    let mut body = Vec::new();
    while body.len() < 256 * 1024 {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = (256 * 1024) - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    let data: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let id = data["id"].as_str().map(str::trim).unwrap_or("");
    if !status.is_success() || id.is_empty() {
        let mut msg = format!("OpenAI ответил {}", status.as_u16());
        if let Some(err_msg) = data
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let safe: String = err_msg.replace(&openai_key, "***").chars().take(240).collect();
            msg.push_str(": ");
            msg.push_str(&safe);
        }
        return bad(msg);
    }
    if let Err(e) = app.store.update(|s| -> Result<(), std::io::Error> {
        s.live_voices.push(crate::settings::CustomVoice {
            id: id.to_string(),
            name: name.to_string(),
            prompt: prompt.to_string(),
        });
        Ok(())
    }) {
        return bad(e.to_string());
    }
    super::reply(axum::http::StatusCode::OK, json!({"ok": true, "id": id}))
}

pub async fn delete_voice(
    State(app): State<App>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let id = query.get("id").map(|s| s.trim()).unwrap_or("");
    match app.store.update(|s| -> Result<(), std::io::Error> {
        s.live_voices.retain(|v| v.id != id);
        Ok(())
    }) {
        Ok(()) => ok(),
        Err(e) => bad(e.to_string()),
    }
}
fn pool(app:&App,service:&str,v:Value)->Response{
    let Some(object)=v.as_object().filter(|o|o.len()==1)else{return bad("Нужна одна операция с ключом");};
    let (op,value)=object.iter().next().unwrap();
    if service == "gemini" && op == "reset_all" {
        app.engine.gemini_pool.clear_all();
        return ok();
    }
    let Some(index)=value.as_u64().and_then(|i|usize::try_from(i).ok())else{return bad("Неверный номер ключа");};
    let allowed = op == "remove"
        || (service == "elevenlabs" && op == "reset")
        || (service == "gemini" && matches!(op.as_str(), "reset" | "activate"));
    if !allowed {return bad("Неизвестная операция с ключом");}
    let mut reset_gemini_key: Option<String> = None;
    match app.store.update(|s|->Result<(),std::io::Error>{
        let invalid=||std::io::Error::other("Ключ с таким номером отсутствует");
        if service=="gemini"{
            if index>=s.gemini_keys.len(){return Err(invalid());}
            match op.as_str() {
                "remove" => {
                    let removed = s.gemini_keys.remove(index);
                    reset_gemini_key = Some(removed);
                    if !s.gemini_keys.contains(&s.gemini_key){
                        s.gemini_key=s.gemini_keys.first().cloned().unwrap_or_default();
                    }
                }
                "reset" => {
                    reset_gemini_key = Some(s.gemini_keys[index].clone());
                }
                "activate" => {
                    s.gemini_key = s.gemini_keys[index].clone();
                }
                _ => {}
            }
        }else{
            if index>=s.eleven_keys.len(){return Err(invalid());}
            if op=="remove"{s.eleven_keys.remove(index);}else{s.eleven_keys[index].until=0;s.eleven_keys[index].why.clear();}
            let now=jiff::Timestamp::now().as_second();
            let active=s.eleven_keys.iter().find(|k|k.key==s.elevenlabs_key && k.until<=now).or_else(||s.eleven_keys.iter().find(|k|k.until<=now)).or_else(||s.eleven_keys.iter().min_by_key(|k|k.until));
            s.elevenlabs_key=active.map(|k|k.key.clone()).unwrap_or_default();
        }
        Ok(())
    }){
        Ok(())=>{
            if let Some(key) = reset_gemini_key {
                app.engine.gemini_pool.reset_key(&key);
            }
            ok()
        }
        Err(e)=>bad(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_pool_add_clear_and_duplicates_preserve_selected_key(){
        let mut s=Settings::default();assign(&mut s,"gemini","first".into());assign(&mut s,"gemini","second".into());assign(&mut s,"gemini","second".into());
        assert_eq!(s.gemini_key,"first");assert_eq!(s.gemini_keys,vec!["first","second"]);
        assert_eq!(s.active_profile_ref().unwrap().engine, "gemini");
        assign(&mut s,"openai","sk-test".into());
        assert_eq!(s.active_profile_ref().unwrap().engine, "gemini");
        assign(&mut s,"elevenlabs","a".into());assign(&mut s,"elevenlabs","b".into());assert_eq!(s.elevenlabs_key,"a");assert_eq!(s.eleven_keys.len(),2);
        assign(&mut s,"gemini","".into());assign(&mut s,"elevenlabs","".into());assert!(s.gemini_keys.is_empty() && s.eleven_keys.is_empty());
    }

    #[test]
    fn auto_select_engine_sets_chatgpt_and_custom_voices_validate_and_mutate() {
        let mut s = Settings::default();
        assert_eq!(s.active_profile_ref().unwrap().engine, "");
        s.auto_select_engine("chatgpt");
        assert_eq!(s.active_profile_ref().unwrap().engine, "chatgpt");

        assert!(validate_voice_input("", "Достаточно длинное описание").is_err());
        assert!(validate_voice_input("Бархат", "коротко").is_err());
        let (name, prompt) = validate_voice_input(" Бархат ", "  Спокойный низкий мужской голос  ").unwrap();
        assert_eq!(name, "Бархат");
        assert_eq!(prompt, "Спокойный низкий мужской голос");

        s.live_voices.push(crate::settings::CustomVoice {
            id: "v1".into(),
            name: name.into(),
            prompt: prompt.into(),
        });
        assert_eq!(s.live_voices.len(), 1);
        s.live_voices.retain(|v| v.id != "v1");
        assert!(s.live_voices.is_empty());
    }
}
