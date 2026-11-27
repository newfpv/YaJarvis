use super::{App,bad,ok};
use axum::{extract::State,response::Response,Json};
use serde_json::{Value,json};
use std::time::Duration;
async fn stop_chat(app:&App){let e=app.engine.clone();let _=tokio::task::spawn_blocking(move||e.end_chat("смена аккаунта ChatGPT")).await;}
pub async fn login(State(app):State<App>)->Response{
    let provider=match app.plugins.provider("chatgpt"){Ok(p)=>p,Err(e)=>return bad(e)};
    match provider.request("login.start",json!({"add":true}),Duration::from_secs(25)).await{
        Ok(state)=>super::reply(axum::http::StatusCode::OK,json!({"ok":true,"login":state})),Err(e)=>bad(e),
    }
}
pub async fn accounts(State(app):State<App>,Json(p):Json<Value>)->Response{
    let action=p["action"].as_str().unwrap_or("");
    if action=="auto"{
        let Some(enabled)=p["enabled"].as_bool()else{return bad("Нужно логическое значение enabled")};
        return match app.store.update(|s|->Result<(),std::io::Error>{s.chatgpt_auto_switch=enabled;Ok(())}){Ok(_)=>ok(),Err(e)=>bad(e.to_string())};
    }
    if !matches!(action,"select"|"remove"){return bad("Неизвестное действие")}
    let id=p["id"].as_str().unwrap_or("").to_string();
    if !app.store.with(|s|s.chatgpt_accounts.iter().any(|a|a.id==id)){return bad("Аккаунт не найден")}
    stop_chat(&app).await;
    if let Ok(provider)=app.plugins.provider("chatgpt"){
        return match provider.request(&format!("accounts.{action}"),json!({"id":id}),Duration::from_secs(15)).await{Ok(_)=>ok(),Err(e)=>bad(e)};
    }
    let res=app.store.update(|s|->Result<(),std::io::Error>{
        match action{"select"=>crate::chatgpt_accounts::select(s,&id),_=>crate::chatgpt_accounts::remove(s,&id)}.map_err(std::io::Error::other)
    });
    match res{Ok(_)=>ok(),Err(e)=>bad(e.to_string())}
}
pub async fn logout(State(app):State<App>)->Response{
    let id=app.store.with(|s|s.chatgpt_active.clone());
    accounts(State(app),Json(json!({"action":"remove","id":id}))).await
}

pub async fn usage(State(app):State<App>,Json(p):Json<Value>)->Response {
    let id=p["id"].as_str().unwrap_or("");
    let force=p["force"].as_bool().unwrap_or(false);
    if !app.store.with(|s|s.chatgpt_accounts.iter().any(|a|a.id==id)){return bad("Аккаунт не найден")}
    let provider=match app.plugins.provider("chatgpt"){Ok(p)=>p,Err(e)=>return bad(e)};
    match provider.request("accounts.usage",json!({"id":id,"force":force}),Duration::from_secs(40)).await {
        Ok(usage)=>super::reply(axum::http::StatusCode::OK,json!({"ok":true,"usage":usage})),Err(e)=>bad(e),
    }
}

pub async fn live_stats(State(app):State<App>,axum::extract::Query(p):axum::extract::Query<std::collections::HashMap<String,String>>)->Response {
    let result=app.store.with(|s|s.chatgpt_accounts.iter().find(|a|Some(&a.id)==p.get("id")).map(|a|a.live.summary()));
    match result{Some(stats)=>super::reply(axum::http::StatusCode::OK,json!({"stats":stats})),None=>bad("Аккаунт не найден")}
}
