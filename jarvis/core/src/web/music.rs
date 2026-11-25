use super::{App, bad, reply};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Response,
};
use serde_json::{Value, json};
use std::collections::HashMap;

pub async fn get(State(app): State<App>) -> Json<Value> {
    Json(app.engine.music.snapshot(&app.plugins).await)
}

pub async fn post(State(app): State<App>, Json(v): Json<Value>) -> Response {
    let action = v["action"].as_str().unwrap_or("");
    let query = v["query"].as_str().unwrap_or("").trim();
    let (op, args) = match action {
        "play" | "queue" if query.is_empty() => return bad("Что включить?"),
        "play" => ("play", json!({"query":query})),
        "id" if !query.is_empty() => (
            "play_id",
            json!({"id":query,"title":v["title"],"author":v["author"]}),
        ),
        "queue" => (
            "enqueue",
            json!({"query":query,"title":v["title"],"author":v["author"]}),
        ),
        "like" | "unlike" => (
            "like",
            json!({
                "id": v["id"],
                "liked": action == "like",
                "title": v["title"],
                "author": v["author"]
            }),
        ),
        "liked" => (
            "play_liked",
            json!({
                "index": v.get("index").and_then(Value::as_u64),
                "shuffle": v["shuffle"] == true,
                "queue": v["queue"] == true
            }),
        ),
        "playlist" => {
            let id = v["id"].as_str().unwrap_or("").trim();
            if id.is_empty() && query.is_empty() {
                return bad("Укажите плейлист");
            }
            (
                "play_playlist",
                json!({
                    "id": id,
                    "query": query,
                    "index": v.get("index").and_then(Value::as_u64),
                    "shuffle": v["shuffle"] == true,
                    "queue": v["queue"] == true
                }),
            )
        }
        "pause" | "resume" | "next" | "prev" | "stop" => (action, Value::Null),
        "seek" => match v["sec"].as_f64().filter(|s| s.is_finite() && *s >= 0.0) {
            Some(sec) => ("seek", json!({"delta":sec,"abs":true})),
            None => return bad("Неверная позиция"),
        },
        "unqueue" => match v["index"].as_u64() {
            Some(i) => ("unqueue", json!({"index":i})),
            None => return bad("Неверный номер в очереди"),
        },
        "repeat" => match v["on"].as_bool() {
            Some(on) => ("repeat", json!({"on": on})),
            None => return bad("Нужно указать on"),
        },
        "radio" => {
            let Some(on) = v["on"].as_bool() else {
                return bad("Нужно указать on");
            };
            if let Err(e) = app.store.update(|s| -> Result<(), std::io::Error> {
                s.radio_off = !on;
                Ok(())
            }) {
                return bad(e.to_string());
            }
            return reply(
                StatusCode::OK,
                app.engine.music.snapshot(&app.plugins).await,
            );
        }
        _ => return bad("Неизвестная команда"),
    };
    match app.engine.music.transport(&app.plugins, op, args).await {
        Ok(_) => reply(
            StatusCode::OK,
            app.engine.music.snapshot(&app.plugins).await,
        ),
        Err(e) => bad(e),
    }
}

pub async fn youtube_state(app: &App) -> Value {
    let plugin = app.plugins.provider("youtube").is_ok();
    if !plugin {
        return json!({"plugin":false});
    }
    let mut state = app.engine.music.call(&app.plugins, "account", Value::Null).await
        .unwrap_or_else(|_| json!({}));
    if !state.is_object() {
        state = json!({});
    }
    state["plugin"] = true.into();
    state
}

async fn youtube_op(app: App, op: &str, args: Value) -> Response {
    match app.engine.music.call(&app.plugins, op, args).await {
        Ok(_) => reply(StatusCode::OK, youtube_state(&app).await),
        Err(error) => bad(error),
    }
}

pub async fn set_cookies(State(app): State<App>, Json(value): Json<Value>) -> Response {
    let Some(cookies) = value["cookies"].as_str().filter(|v| !v.is_empty() && v.len() <= 200 << 10) else {
        return bad("Нужен файл cookies.txt (до 200 КБ)");
    };
    youtube_op(app, "cookies_set", json!({"cookies":cookies})).await
}

pub async fn clear_cookies(State(app): State<App>) -> Response {
    youtube_op(app, "cookies_clear", Value::Null).await
}

pub async fn youtube_login(State(app): State<App>) -> Response {
    youtube_op(app, "account_begin", Value::Null).await
}

pub async fn youtube_logout(State(app): State<App>) -> Response {
    youtube_op(app, "account_logout", Value::Null).await
}

pub async fn youtube_library(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let refresh = q.get("refresh").is_some_and(|v| v == "1" || v == "true");
    match app
        .engine
        .music
        .call(&app.plugins, "library", json!({"refresh": refresh}))
        .await
    {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => bad(error),
    }
}

pub async fn youtube_playlist(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let refresh = q.get("refresh").is_some_and(|v| v == "1" || v == "true");
    match app
        .engine
        .music
        .call(
            &app.plugins,
            "playlist_items",
            json!({"id": id, "refresh": refresh}),
        )
        .await
    {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => bad(error),
    }
}

pub async fn youtube_like(State(app): State<App>, Json(v): Json<Value>) -> Response {
    match app.engine.music.call(&app.plugins, "like", v).await {
        Ok(value) => reply(
            StatusCode::OK,
            json!({
                "ok": true,
                "id": value["id"],
                "liked": value["liked"],
                "track": value["track"],
                "music": app.engine.music.snapshot(&app.plugins).await,
            }),
        ),
        Err(error) => bad(error),
    }
}

pub async fn youtube_search(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let args = json!({
        "query": q.get("q").map(String::as_str).unwrap_or(""),
        "type": q.get("type").map(String::as_str).unwrap_or("video"),
        "sort": q.get("sort").map(String::as_str).unwrap_or("relevance"),
        "date": q.get("date").map(String::as_str).unwrap_or(""),
        "duration": q.get("duration").map(String::as_str).unwrap_or(""),
        "live": q.get("live").is_some_and(|v| v == "1" || v == "true"),
    });
    match app.engine.music.call(&app.plugins, "search", args).await {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => bad(error),
    }
}
