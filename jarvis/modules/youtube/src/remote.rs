use crate::player_script::{Prepared, Scripts};
use base64::{Engine, engine::general_purpose::STANDARD};
use jarvis_home_common::{Host, json as response_json};
use reqwest::{Client, Url, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const WEB_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0 Safari/537.36";
pub const WEB_VERSION: &str = "2.20261112.01.00";
pub const TV_AGENT: &str = "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";
const TV_VERSION: &str = "4.20240101";
const TV_BROWSE_VERSION: &str = "7.20260114.12.00";
const VR_VERSION: &str = "1.65.10";
const VR_AGENT: &str = "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    #[serde(rename = "ID", alias = "id", default)]
    pub id: String,
    #[serde(rename = "Title", alias = "title", default)]
    pub title: String,
    #[serde(rename = "Author", alias = "author", default)]
    pub author: String,
}
impl Track {
    pub fn from_id(id: String) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    #[serde(rename = "ID", alias = "id", default)]
    pub id: String,
    #[serde(rename = "Title", alias = "title", default)]
    pub title: String,
    #[serde(
        rename = "Count",
        alias = "count",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub count: String,
    #[serde(
        rename = "Thumb",
        alias = "thumb",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub thumb: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchItem {
    pub kind: String,
    #[serde(rename = "ID", alias = "id", default)]
    pub id: String,
    #[serde(rename = "Title", alias = "title", default)]
    pub title: String,
    #[serde(rename = "Author", alias = "author", default)]
    pub author: String,
    #[serde(
        rename = "Duration",
        alias = "duration",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub duration: String,
    #[serde(
        rename = "Views",
        alias = "views",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub views: String,
    #[serde(
        rename = "Published",
        alias = "published",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub published: String,
    #[serde(
        rename = "Count",
        alias = "count",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub count: String,
    #[serde(
        rename = "Thumb",
        alias = "thumb",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub thumb: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFilter {
    pub kind: String,
    pub sort: String,
    pub date: String,
    pub duration: String,
    pub live: bool,
}

pub(crate) fn encode_search_params(f: &SearchFilter) -> Option<String> {
    let mut buf = Vec::new();
    let sort_code = match f.sort.as_str() {
        "rating" => Some(1u8),
        "date" => Some(2u8),
        "views" => Some(3u8),
        _ => None,
    };
    if let Some(code) = sort_code {
        buf.push(0x08);
        buf.push(code);
    }

    let mut sub = Vec::new();
    if let Some(date_code) = match f.date.as_str() {
        "hour" => Some(1u8),
        "today" => Some(2u8),
        "week" => Some(3u8),
        "month" => Some(4u8),
        "year" => Some(5u8),
        _ => None,
    } {
        sub.push(0x08);
        sub.push(date_code);
    }
    if let Some(type_code) = match f.kind.as_str() {
        "video" => Some(1u8),
        "playlist" => Some(3u8),
        _ => None,
    } {
        sub.push(0x10);
        sub.push(type_code);
    }
    if f.kind != "playlist" {
        if let Some(dur_code) = match f.duration.as_str() {
            "short" => Some(1u8),
            "long" => Some(2u8),
            "medium" => Some(3u8),
            _ => None,
        } {
            sub.push(0x18);
            sub.push(dur_code);
        }
        if f.live {
            sub.push(0x40);
            sub.push(1u8);
        }
    }

    if !sub.is_empty() {
        buf.push(0x12);
        buf.push(sub.len() as u8);
        buf.extend_from_slice(&sub);
    }
    if buf.is_empty() {
        None
    } else {
        Some(STANDARD.encode(&buf))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountProfile {
    pub name: String,
    pub handle: String,
    pub email: String,
}

impl AccountProfile {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.handle.is_empty() && self.email.is_empty()
    }

    pub fn merge_missing(&mut self, other: AccountProfile) {
        if self.name.is_empty() {
            self.name = other.name;
        }
        if self.handle.is_empty() {
            self.handle = other.handle;
        }
        if self.email.is_empty() {
            self.email = other.email;
        }
        self.normalize();
    }

    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        self.handle = self.handle.trim().to_string();
        self.email = self.email.trim().to_string();
        if self.handle.contains('@') && !self.handle.starts_with('@') {
            if self.email.is_empty() {
                self.email = std::mem::take(&mut self.handle);
            } else {
                self.handle.clear();
            }
        }
        if self.email.starts_with('@') {
            if self.handle.is_empty() {
                self.handle = std::mem::take(&mut self.email);
            } else {
                self.email.clear();
            }
        }
    }
}

#[derive(Clone)]
pub struct Stream {
    pub url: Url,
    pub agent: &'static str,
    pub client: &'static str,
    pub length: u64,
    pub duration: f64,
    pub ready: Instant,
    pub itag: u64,
    pub bitrate: u64,
}
pub struct Remote {
    host: Arc<Host>,
    scripts: Scripts,
    visitor: Mutex<Option<(Instant, String)>>,
}
impl Remote {
    pub fn new(host: Arc<Host>) -> Self {
        let scripts = Scripts::with_cache(host.data.join("player-cache.json.gz"));
        Self {
            host,
            scripts,
            visitor: Default::default(),
        }
    }
    pub fn invalidate_player_cache(&self) {
        self.scripts.invalidate();
    }
    pub async fn warm(&self) -> Result<(), String> {
        self.scripts
            .get(&self.host.client().await?)
            .await
            .map(|_| ())
    }
    pub async fn warm_background(&self) {
        if let Ok(client) = self.host.client().await {
            self.scripts.warm_background(client);
        }
    }
    pub async fn evict_idle(&self, idle: Duration) {
        self.scripts.evict_if_idle(idle).await;
    }
    pub async fn search(&self, query: &str, mut headers: HeaderMap) -> Result<Vec<Track>, String> {
        strip_bearer(&mut headers);
        let mut body = json!({"context":{"client":web()},"query":query});
        let client = self.host.client().await?;
        for filter in [false, true] {
            if filter {
                body["params"] = "EgIQAQ==".into();
            }
            let data = post(&client, "search", WEB_AGENT, &body, headers.clone()).await?;
            self.note_visitor(&data);
            let section = &data["contents"]["twoColumnSearchResultsRenderer"]["primaryContents"]["sectionListRenderer"]
                ["contents"];
            let mut result = vec![];
            organic(section, &mut result, &mut HashSet::new(), 0);
            if !result.is_empty() {
                return Ok(result);
            }
        }
        Ok(vec![])
    }

    pub async fn search_filtered(
        &self,
        query: &str,
        filter: &SearchFilter,
        mut headers: HeaderMap,
    ) -> Result<Vec<SearchItem>, String> {
        strip_bearer(&mut headers);
        let mut body = json!({"context":{"client":web()},"query":query});
        if let Some(params) = encode_search_params(filter) {
            body["params"] = params.into();
        }
        let client = self.host.client().await?;
        let data = post(&client, "search", WEB_AGENT, &body, headers).await?;
        self.note_visitor(&data);
        let section = &data["contents"]["twoColumnSearchResultsRenderer"]["primaryContents"]
            ["sectionListRenderer"]["contents"];
        let mut items = Vec::new();
        parse_search_items(section, &filter.kind, &mut items, &mut HashSet::new(), 0);
        Ok(items)
    }

    pub async fn oembed(&self, id: &str) -> Result<Track, String> {
        if !valid_id(id) {
            return Err("Неверный ID видео YouTube".into());
        }
        let client = self.host.client().await?;
        let url = format!(
            "https://www.youtube.com/oembed?url=https://www.youtube.com/watch?v={id}&format=json"
        );
        let response = client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| "Нет соединения с YouTube")?;
        if !response.status().is_success() {
            return Err(format!("YouTube oEmbed ответил {}", response.status().as_u16()));
        }
        let data = response_json(response).await?;
        Ok(Track {
            id: id.into(),
            title: text_label(&data["title"]).trim().to_string(),
            author: text_label(&data["author_name"]).trim().to_string(),
        })
    }

    pub async fn set_like(&self, id: &str, liked: bool, headers: HeaderMap) -> Result<(), String> {
        if !valid_id(id) {
            return Err("Неверный ID видео YouTube".into());
        }
        if headers.is_empty() {
            return Err("Войдите в аккаунт YouTube в настройках".into());
        }
        let endpoint = if liked { "like/like" } else { "like/removelike" };
        let innertube_err = match self
            .post_client(endpoint, json!({"target": {"videoId": id}}), headers.clone())
            .await
        {
            Ok(data) => {
                if data["responseContext"]["mainAppWebResponseContext"]["loggedOut"] == true {
                    return Err("YouTube просит войти заново".into());
                }
                return Ok(());
            }
            Err(e) if e.contains("401") || e.contains("403") => {
                return Err("YouTube просит войти заново".into());
            }
            Err(e) => e,
        };
        if has_bearer(&headers) {
            let Some(auth) = bearer_headers(&headers) else {
                return Err(innertube_err);
            };
            let rating = if liked { "like" } else { "none" };
            let response = self
                .host
                .client()
                .await?
                .post("https://www.googleapis.com/youtube/v3/videos/rate")
                .query(&[("id", id), ("rating", rating)])
                .headers(auth)
                .header(reqwest::header::CONTENT_LENGTH, 0)
                .send()
                .await
                .map_err(|_| "Нет соединения с Google")?;
            if response.status().as_u16() == 401 {
                return Err("YouTube просит войти заново".into());
            }
            if !response.status().is_success() {
                return Err(format!(
                    "YouTube не смог обновить лайк ({})",
                    response.status().as_u16()
                ));
            }
            return Ok(());
        }
        Err(innertube_err)
    }

    pub async fn recommendations(&self, headers: HeaderMap) -> Result<Vec<Track>, String> {
        if headers.is_empty() {
            return Ok(Vec::new());
        }
        let response = self
            .browse_request(json!({"browseId": "FEwhat_to_watch"}), headers)
            .await?;
        let mut tracks = Vec::new();
        organic(&response["contents"], &mut tracks, &mut HashSet::new(), 0);
        Ok(tracks)
    }

    async fn post_client(
        &self,
        path: &str,
        mut body: Value,
        mut headers: HeaderMap,
    ) -> Result<Value, String> {
        let client = self.host.client().await?;
        if has_bearer(&headers) {
            headers.insert(
                "X-YouTube-Client-Name",
                reqwest::header::HeaderValue::from_static("7"),
            );
            headers.insert(
                "X-YouTube-Client-Version",
                reqwest::header::HeaderValue::from_static(TV_BROWSE_VERSION),
            );
            headers.insert(
                "Origin",
                reqwest::header::HeaderValue::from_static("https://www.youtube.com"),
            );
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "context".into(),
                    json!({"client": {"clientName": "TVHTML5", "clientVersion": TV_BROWSE_VERSION, "hl": "ru", "gl": "RU", "platform": "TV", "deviceMake": "Samsung", "deviceModel": "SmartTV"}}),
                );
            }
            post(&client, path, TV_AGENT, &body, headers).await
        } else {
            strip_bearer(&mut headers);
            if let Some(obj) = body.as_object_mut() {
                obj.insert("context".into(), json!({"client": web()}));
            }
            post(&client, path, WEB_AGENT, &body, headers).await
        }
    }

    pub async fn browse_request(
        &self,
        body: Value,
        headers: HeaderMap,
    ) -> Result<Value, String> {
        let authed = !headers.is_empty();
        let data = self.post_client("browse", body, headers).await.map_err(|e| {
            if authed && (e.contains("401") || e.contains("403")) {
                "YouTube просит войти заново".into()
            } else {
                e
            }
        })?;
        if authed && data["responseContext"]["mainAppWebResponseContext"]["loggedOut"] == true {
            return Err("YouTube просит войти заново".into());
        }
        Ok(data)
    }

    pub async fn account_profile(&self, headers: HeaderMap) -> Result<AccountProfile, String> {
        if headers.is_empty() {
            return Ok(AccountProfile::default());
        }
        let mut profile = AccountProfile::default();
        for (endpoint, body) in [
            ("account/account_menu", json!({})),
            (
                "account/accounts_list",
                json!({"accountReadMask": {"returnOwner": true, "returnBrandAccounts": true}}),
            ),
            ("account/accounts_list", json!({})),
        ] {
            match self.post_client(endpoint, body, headers.clone()).await {
                Ok(data) => profile.merge_missing(parse_account_profile(&data)),
                Err(error) => log::debug!("YouTube: {endpoint} не вернул профиль: {error}"),
            }
            if !profile.name.is_empty() && (!profile.handle.is_empty() || !profile.email.is_empty())
            {
                break;
            }
        }
        if has_bearer(&headers)
            && (profile.name.is_empty() || profile.handle.is_empty())
            && let Ok(data) = self
                .data_api_get(
                    "channels",
                    &[("part", "snippet"), ("mine", "true")],
                    &headers,
                )
                .await
        {
            let snippet = &data["items"][0]["snippet"];
            profile.merge_missing(AccountProfile {
                name: snippet["title"].as_str().unwrap_or("").to_string(),
                handle: snippet["customUrl"].as_str().unwrap_or("").to_string(),
                email: String::new(),
            });
        }
        profile.normalize();
        Ok(profile)
    }

    async fn data_api_get(
        &self,
        path: &str,
        query: &[(&str, &str)],
        headers: &HeaderMap,
    ) -> Result<Value, String> {
        let Some(auth) = bearer_headers(headers) else {
            return Err("Нет токена YouTube".into());
        };
        let response = self
            .host
            .client()
            .await?
            .get(format!("https://www.googleapis.com/youtube/v3/{path}"))
            .query(query)
            .headers(auth)
            .send()
            .await
            .map_err(|_| "Нет соединения с Google")?;
        if matches!(response.status().as_u16(), 401) {
            return Err("YouTube просит войти заново".into());
        }
        response_json(response).await
    }

    async fn data_api_playlists(&self, headers: &HeaderMap) -> Result<Vec<Playlist>, String> {
        let data = self
            .data_api_get(
                "playlists",
                &[
                    ("part", "snippet,contentDetails"),
                    ("mine", "true"),
                    ("maxResults", "50"),
                ],
                headers,
            )
            .await?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for item in data["items"].as_array().into_iter().flatten() {
            let id = item["id"].as_str().unwrap_or("");
            let title = item["snippet"]["title"].as_str().unwrap_or("").trim().to_string();
            if !valid_playlist_id(id) || id.starts_with("RD") || title.is_empty() {
                continue;
            }
            if !seen.insert(id.to_string()) {
                continue;
            }
            let count = item["contentDetails"]["itemCount"]
                .as_u64()
                .map(|n| n.to_string())
                .unwrap_or_default();
            let thumb = ["medium", "high", "default"]
                .into_iter()
                .filter_map(|k| item["snippet"]["thumbnails"][k]["url"].as_str())
                .map(safe_thumb)
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            out.push(Playlist {
                id: id.to_string(),
                title,
                count,
                thumb,
            });
        }
        Ok(out)
    }

    async fn data_api_playlist_items(
        &self,
        playlist_id: &str,
        headers: &HeaderMap,
        limit: usize,
    ) -> Result<Vec<Track>, String> {
        let max_s = limit.min(50).to_string();
        let data = self
            .data_api_get(
                "playlistItems",
                &[
                    ("part", "snippet"),
                    ("playlistId", playlist_id),
                    ("maxResults", &max_s),
                ],
                headers,
            )
            .await?;
        Ok(parse_data_api_tracks(&data["items"], false, limit))
    }

    async fn data_api_liked(
        &self,
        headers: &HeaderMap,
        limit: usize,
    ) -> Result<Vec<Track>, String> {
        let max_s = limit.min(50).to_string();
        let data = self
            .data_api_get(
                "videos",
                &[
                    ("part", "snippet"),
                    ("myRating", "like"),
                    ("maxResults", &max_s),
                ],
                headers,
            )
            .await?;
        Ok(parse_data_api_tracks(&data["items"], true, limit))
    }

    pub async fn playlists(&self, headers: HeaderMap) -> Result<Vec<Playlist>, String> {
        if headers.is_empty() {
            return Err("Войдите в аккаунт YouTube в настройках".into());
        }
        let bearer = has_bearer(&headers);
        let browse_ids = if bearer {
            ["FElibrary", "FEplaylist_aggregation"]
        } else {
            ["FEplaylist_aggregation", "FElibrary"]
        };
        let mut last_err: Option<String> = None;
        for bid in browse_ids {
            match self
                .browse_request(json!({"browseId": bid}), headers.clone())
                .await
            {
                Ok(page) => {
                    let list = parse_playlists(&page);
                    if !list.is_empty() {
                        return Ok(list);
                    }
                }
                Err(e) if e.contains("войти заново") => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        if bearer {
            match self.data_api_playlists(&headers).await {
                Ok(list) if !list.is_empty() => return Ok(list),
                Ok(_) => return Ok(Vec::new()),
                Err(e) if e.contains("войти заново") => return Err(e),
                Err(e) => {
                    if let Some(browse_err) = last_err {
                        return Err(browse_err);
                    }
                    let _ = e;
                }
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    pub async fn playlist_tracks(
        &self,
        playlist_id: &str,
        headers: HeaderMap,
        max_tracks: usize,
    ) -> Result<Vec<Track>, String> {
        if !valid_playlist_id(playlist_id) {
            return Err("Неверный ID плейлиста YouTube".into());
        }
        let limit = max_tracks.clamp(1, 100);
        let bearer = has_bearer(&headers);
        let mut tracks = Vec::new();
        let mut seen = HashSet::new();
        let mut visited_tokens = HashSet::new();
        let mut body = json!({"browseId": format!("VL{playlist_id}")});
        let mut first_err: Option<String> = None;
        for page_idx in 0..3 {
            match self.browse_request(body, headers.clone()).await {
                Ok(page) => {
                    let content = browse_contents(&page);
                    organic_limit(content, &mut tracks, &mut seen, 0, limit);
                    if tracks.len() >= limit {
                        break;
                    }
                    let Some(token) = continuation_token(content) else {
                        break;
                    };
                    if !visited_tokens.insert(token.clone()) {
                        break;
                    }
                    body = json!({"continuation": token});
                }
                Err(e) if e.contains("войти заново") => return Err(e),
                Err(e) => {
                    if page_idx == 0 {
                        first_err = Some(e);
                    }
                    break;
                }
            }
        }
        if tracks.is_empty() && bearer && !matches!(playlist_id, "LM" | "LL") {
            match self.data_api_playlist_items(playlist_id, &headers, limit).await {
                Ok(api_tracks) if !api_tracks.is_empty() => return Ok(api_tracks),
                Ok(_) if first_err.is_none() => return Ok(Vec::new()),
                Err(e) if e.contains("войти заново") => return Err(e),
                _ => {}
            }
        }
        if tracks.is_empty()
            && let Some(err) = first_err
        {
            return Err(err);
        }
        Ok(tracks)
    }

    pub async fn liked_tracks(
        &self,
        headers: HeaderMap,
        max_tracks: usize,
    ) -> Result<Vec<Track>, String> {
        if headers.is_empty() {
            return Err("Войдите в аккаунт YouTube в настройках".into());
        }
        let bearer = has_bearer(&headers);
        let mut last_err: Option<String> = None;
        for id in ["LM", "LL"] {
            match self.playlist_tracks(id, headers.clone(), max_tracks).await {
                Ok(tracks) if !tracks.is_empty() => return Ok(tracks),
                Ok(_) => {}
                Err(e) if e.contains("войти заново") => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        if bearer {
            match self.data_api_liked(&headers, max_tracks).await {
                Ok(tracks) => return Ok(tracks),
                Err(e) if e.contains("войти заново") => return Err(e),
                Err(e) => last_err = last_err.or(Some(e)),
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    pub async fn search_playlists(
        &self,
        query: &str,
        mut headers: HeaderMap,
    ) -> Result<Vec<Playlist>, String> {
        strip_bearer(&mut headers);
        let body = json!({
            "context": {"client": web()},
            "query": query,
            "params": "EgIQAw=="
        });
        let client = self.host.client().await?;
        let data = post(&client, "search", WEB_AGENT, &body, headers).await?;
        self.note_visitor(&data);
        let section = &data["contents"]["twoColumnSearchResultsRenderer"]["primaryContents"]
            ["sectionListRenderer"]["contents"];
        Ok(parse_playlists(section))
    }
    pub async fn related(&self, id: &str, mut headers: HeaderMap) -> Result<Vec<Track>, String> {
        strip_bearer(&mut headers);
        let client = self.host.client().await?;
        let data = post(
            &client,
            "next",
            WEB_AGENT,
            &json!({"context":{"client":web()},"videoId":id}),
            headers,
        )
        .await?;
        self.note_visitor(&data);
        let items = &data["contents"]["twoColumnWatchNextResults"]["secondaryResults"]["secondaryResults"]
            ["results"];
        let mut result = vec![];
        organic(items, &mut result, &mut HashSet::from([id.to_owned()]), 0);
        Ok(result)
    }
    async fn visitor(&self) -> String {
        let current = || {
            self.visitor
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(3600))
                .map(|(_, v)| v.clone())
        };
        if let Some(value) = current() {
            return value;
        }
        let _ = self.search("музыка", HeaderMap::new()).await;
        current().unwrap_or_default()
    }
    fn note_visitor(&self, value: &Value) {
        if let Some(visitor) = value["responseContext"]["visitorData"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 1024)
        {
            *self.visitor.lock().unwrap() = Some((Instant::now(), visitor.into()));
        }
    }
    pub async fn resolve(&self, id: &str, auth: HeaderMap, prefer_tv: bool) -> Result<(Track, Stream), String> {
        if !valid_id(id) {
            return Err("Неверный ID видео YouTube".into());
        }
        let client = self.host.client().await?;
        let visitor = if auth.is_empty() {
            self.visitor().await
        } else {
            String::new()
        };
        let mut backup = None;
        if !prefer_tv && auth.is_empty() && !self.scripts.has_warm_cache() {
            self.scripts.warm_background(client.clone());
            match vr(&client, id, &visitor).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    log::debug!("YouTube: быстрый плеер ANDROID_VR недоступен: {error}");
                    backup = Some(Err(error));
                }
            }
        }
        let tv=async{
            let prepared=self.scripts.get(&client).await?;
            let mut context=json!({"clientName":"TVHTML5","clientVersion":TV_VERSION,"deviceMake":"Samsung","deviceModel":"UKS9800",
                "userAgent":TV_AGENT,"osName":"Tizen","osVersion":"2.4.0","hl":"ru","gl":"RU"});
            if !visitor.is_empty(){context["visitorData"]=visitor.clone().into();}
            let mut headers=auth.clone();
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&visitor) && !visitor.is_empty() {
                headers.insert("X-Goog-Visitor-Id", value);
            }
            headers.insert("X-YouTube-Client-Name",reqwest::header::HeaderValue::from_static("7"));
            headers.insert("X-YouTube-Client-Version",reqwest::header::HeaderValue::from_static(TV_VERSION));
            headers.insert("Origin",reqwest::header::HeaderValue::from_static("https://www.youtube.com"));
            let body=json!({"context":{"client":context},"videoId":id,
                "playbackContext":{"contentPlaybackContext":{"html5Preference":"HTML5_PREF_WANTS","signatureTimestamp":prepared.timestamp}}});
            let start=Instant::now();
            let data=post(&client,"player",TV_AGENT,&body,headers).await?;
            formats(&data,id,"TVHTML5",TV_AGENT,Some(&prepared),start+ad_wait(&data)).await
        }.await;
        match tv {
            Ok(v) => Ok(v),
            Err(e) if !auth.is_empty() || login_required(&e) => Err(e),
            Err(error) => {
                log::debug!("YouTube: телевизионный плеер недоступен: {error}");
                match backup {
                    Some(Ok(value)) => Ok(value),
                    Some(Err(_)) => Err(error),
                    None => vr(&client, id, &visitor).await,
                }
            }
        }
    }
    pub async fn check_cookies(&self, headers: HeaderMap) -> Result<(), String> {
        let body = json!({"context":{"client":web()},"videoId":"jNQXAC9IVRw"});
        let data = post(
            &self.host.client().await?,
            "player",
            WEB_AGENT,
            &body,
            headers,
        )
        .await?;
        if data["responseContext"]["mainAppWebResponseContext"]["loggedOut"] != false {
            return Err("YouTube не принял куки аккаунта".into());
        }
        Ok(())
    }
}
async fn vr(client: &Client, id: &str, visitor: &str) -> Result<(Track, Stream), String> {
    let body = json!({"context":{"client":{"clientName":"ANDROID_VR","clientVersion":VR_VERSION,"deviceMake":"Oculus",
        "deviceModel":"Quest 3","androidSdkVersion":32,"osName":"Android","osVersion":"12L","visitorData":visitor,"hl":"ru","gl":"RU"}},
        "videoId":id});
    let data = post(client, "player", VR_AGENT, &body, HeaderMap::new()).await?;
    formats(&data, id, "ANDROID_VR", VR_AGENT, None, Instant::now()).await
}
fn web() -> Value {
    json!({"clientName":"WEB","clientVersion":WEB_VERSION,"hl":"ru","gl":"RU"})
}
fn has_bearer(headers: &HeaderMap) -> bool {
    headers
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer "))
}
fn bearer_headers(headers: &HeaderMap) -> Option<HeaderMap> {
    let value = headers.get(reqwest::header::AUTHORIZATION)?;
    if !value.to_str().ok().is_some_and(|v| v.starts_with("Bearer ")) {
        return None;
    }
    let mut out = HeaderMap::new();
    out.insert(reqwest::header::AUTHORIZATION, value.clone());
    Some(out)
}
fn strip_bearer(headers: &mut HeaderMap) {
    if has_bearer(headers) {
        headers.remove(reqwest::header::AUTHORIZATION);
    }
}
async fn post(
    client: &Client,
    path: &str,
    agent: &str,
    body: &Value,
    headers: HeaderMap,
) -> Result<Value, String> {
    let response = client
        .post(format!(
            "https://www.youtube.com/youtubei/v1/{path}?prettyPrint=false"
        ))
        .header("User-Agent", agent)
        .headers(headers)
        .json(body)
        .send()
        .await
        .map_err(|_| "Нет соединения с YouTube")?;
    response_json(response).await
}
fn label(v: &Value) -> String {
    if let Some(s) = v.as_str().or_else(|| v["simpleText"].as_str()) {
        return s.chars().take(300).collect();
    }
    v["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["text"].as_str())
        .flat_map(str::chars)
        .take(300)
        .collect()
}
pub fn safe_thumb(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 {
        return String::new();
    }
    let Ok(url) = Url::parse(trimmed) else {
        return String::new();
    };
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return String::new();
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return String::new();
    };
    if matches!(
        host.as_str(),
        "i.ytimg.com" | "i9.ytimg.com" | "yt3.ggpht.com" | "lh3.googleusercontent.com"
    ) || host.ends_with(".ytimg.com")
        || host.ends_with(".ggpht.com")
        || host.ends_with(".googleusercontent.com")
    {
        trimmed.to_string()
    } else {
        String::new()
    }
}
fn collect_thumb_candidates(value: &Value, out: &mut Vec<(String, i64)>, depth: u8) {
    if depth > 10 {
        return;
    }
    match value {
        Value::Array(list) => {
            for item in list {
                collect_thumb_candidates(item, out, depth + 1);
            }
        }
        Value::Object(map) => {
            if let Some(url) = map.get("url").and_then(Value::as_str) {
                let clean = safe_thumb(url);
                if !clean.is_empty() {
                    let width = map.get("width").and_then(Value::as_i64).unwrap_or(0);
                    out.push((clean, width));
                }
            }
            for key in [
                "thumbnail",
                "thumbnails",
                "thumbnailRenderer",
                "playlistVideoThumbnailRenderer",
                "playlistCustomThumbnailRenderer",
                "header",
                "tileHeaderRenderer",
                "contentImage",
                "collectionThumbnailViewModel",
                "primaryThumbnail",
                "thumbnailViewModel",
                "image",
                "sources",
            ] {
                if let Some(child) = map.get(key) {
                    collect_thumb_candidates(child, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}
fn thumb_url(item: &Value) -> String {
    let mut candidates = Vec::new();
    collect_thumb_candidates(item, &mut candidates, 0);
    candidates
        .into_iter()
        .min_by_key(|(_, width)| {
            if *width <= 0 {
                10_000
            } else {
                (*width - 360).abs()
            }
        })
        .map(|(url, _)| url)
        .unwrap_or_default()
}
fn parse_data_api_tracks(items: &Value, direct_video: bool, limit: usize) -> Vec<Track> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in items.as_array().into_iter().flatten() {
        if out.len() >= limit {
            break;
        }
        let id = if direct_video {
            item["id"].as_str()
        } else {
            item["snippet"]["resourceId"]["videoId"].as_str()
        };
        let Some(id) = id.filter(|s| valid_id(s)) else {
            continue;
        };
        let title = item["snippet"]["title"].as_str().unwrap_or("").trim();
        if title.is_empty() || matches!(title, "Deleted video" | "Private video") {
            continue;
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        let author = item["snippet"]["videoOwnerChannelTitle"]
            .as_str()
            .or_else(|| item["snippet"]["channelTitle"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(Track {
            id: id.to_string(),
            title: title.chars().take(300).collect(),
            author: author.chars().take(300).collect(),
        });
    }
    out
}
pub(crate) fn browse_contents<'a>(root: &'a Value) -> &'a Value {
    fn find_shelf<'a>(value: &'a Value, depth: u8) -> Option<&'a Value> {
        if depth > 14 {
            return None;
        }
        match value {
            Value::Object(map) => {
                for key in [
                    "playlistVideoListRenderer",
                    "playlistVideoListContinuation",
                    "musicPlaylistShelfRenderer",
                    "musicPlaylistShelfContinuation",
                ] {
                    if let Some(hit) = map.get(key)
                        && hit.is_object()
                    {
                        return Some(hit);
                    }
                }
                for child in map.values() {
                    if let Some(hit) = find_shelf(child, depth + 1) {
                        return Some(hit);
                    }
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|item| find_shelf(item, depth + 1)),
            _ => None,
        }
    }
    find_shelf(root, 0).unwrap_or(root)
}
pub(crate) fn continuation_token(root: &Value) -> Option<String> {
    fn walk(value: &Value, depth: u8) -> Option<String> {
        if depth > 14 {
            return None;
        }
        match value {
            Value::Object(map) => {
                if let Some(token) = value["continuationItemRenderer"]["continuationEndpoint"]
                    ["continuationCommand"]["token"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                {
                    return Some(token.to_string());
                }
                for kind in ["nextContinuationData", "reloadContinuationData"] {
                    if let Some(token) = value[kind]["continuation"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                    {
                        return Some(token.to_string());
                    }
                }
                for (key, child) in map {
                    if matches!(key.as_str(), "menu" | "navigationEndpoint" | "header") {
                        continue;
                    }
                    if let Some(token) = walk(child, depth + 1) {
                        return Some(token);
                    }
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|item| walk(item, depth + 1)),
            _ => None,
        }
    }
    walk(root, 0)
}
fn organic(value: &Value, out: &mut Vec<Track>, seen: &mut HashSet<String>, depth: u8) {
    organic_limit(value, out, seen, depth, 30);
}
pub(crate) fn organic_limit(
    value: &Value,
    out: &mut Vec<Track>,
    seen: &mut HashSet<String>,
    depth: u8,
    limit: usize,
) {
    if out.len() >= limit || depth >= 16 {
        return;
    }
    match value {
        Value::Array(list) => {
            for v in list {
                organic_limit(v, out, seen, depth + 1, limit);
            }
        }
        Value::Object(_) => {
            for name in [
                "videoRenderer",
                "compactVideoRenderer",
                "playlistVideoRenderer",
                "playlistPanelVideoRenderer",
                "gridVideoRenderer",
                "tileRenderer",
                "lockupViewModel",
            ] {
                let Some(item) = value.get(name) else {
                    continue;
                };
                let track = match name {
                    "lockupViewModel" if item["contentType"] == "LOCKUP_CONTENT_TYPE_VIDEO" => {
                        let meta = &item["metadata"]["lockupMetadataViewModel"];
                        let title = &meta["title"]["content"];
                        let author = label(
                            &meta["metadata"]["contentMetadataViewModel"]["metadataRows"][0]
                                ["metadataParts"][0]["text"]["content"],
                        );
                        identified(item["contentId"].as_str(), label(title), author)
                    }
                    "tileRenderer"
                        if matches!(
                            item["contentType"].as_str(),
                            None | Some("") | Some("TILE_CONTENT_TYPE_VIDEO")
                        ) && find(item, "adBadgeViewModel", 0).is_none() =>
                    {
                        let id = find(item, "watchEndpoint", 0)
                            .and_then(|node| node["videoId"].as_str());
                        let title = find(item, "tileMetadataRenderer", 0)
                            .map(|node| label(&node["title"]))
                            .unwrap_or_default();
                        let author = find(item, "lineItemRenderer", 0)
                            .map(|node| label(&node["text"]))
                            .unwrap_or_default();
                        identified(id, title, author)
                    }
                    "lockupViewModel" | "tileRenderer" => None,
                    _ => {
                        let author = ["shortBylineText", "ownerText", "longBylineText"]
                            .into_iter()
                            .map(|k| label(&item[k]))
                            .find(|s| !s.is_empty())
                            .unwrap_or_default();
                        identified(item["videoId"].as_str(), label(&item["title"]), author)
                    }
                };
                if let Some(track) = track
                    && seen.insert(track.id.clone())
                {
                    out.push(track);
                }
                return;
            }
            for name in [
                "contents",
                "itemSectionRenderer",
                "sectionListRenderer",
                "playlistVideoListRenderer",
                "playlistVideoListContinuation",
                "shelfRenderer",
                "tvShelfRenderer",
                "content",
                "verticalListRenderer",
                "items",
                "richItemRenderer",
                "compactAutoplayRenderer",
                "results",
                "tvBrowseRenderer",
                "tvSurfaceContentRenderer",
                "tvPageContentRenderer",
                "gridRenderer",
                "horizontalListRenderer",
                "richGridRenderer",
                "richShelfRenderer",
                "twoColumnBrowseResultsRenderer",
                "onResponseReceivedActions",
                "appendContinuationItemsAction",
                "continuationItems",
                "tabs",
                "tabRenderer",
            ] {
                if let Some(child) = value.get(name) {
                    organic_limit(child, out, seen, depth + 1, limit);
                }
            }
        }
        _ => {}
    }
}
fn text_label(v: &Value) -> String {
    let s = label(v);
    if !s.is_empty() {
        return s;
    }
    label(&v["content"])
}

pub(crate) fn parse_account_profile(value: &Value) -> AccountProfile {
    fn walk(
        value: &Value,
        depth: u8,
        header: &mut AccountProfile,
        selected_item: &mut AccountProfile,
        fallback_item: &mut AccountProfile,
    ) {
        if depth > 16 {
            return;
        }
        match value {
            Value::Array(list) => {
                for item in list {
                    walk(item, depth + 1, header, selected_item, fallback_item);
                }
            }
            Value::Object(map) => {
                if let Some(hdr) = map.get("activeAccountHeaderRenderer") {
                    header.merge_missing(AccountProfile {
                        name: text_label(&hdr["accountName"]),
                        handle: text_label(&hdr["channelHandle"]),
                        email: text_label(&hdr["email"]),
                    });
                }
                if let Some(hdr) = map.get("googleAccountHeaderRenderer") {
                    let name = text_label(&hdr["name"]);
                    let name = if name.is_empty() {
                        text_label(&hdr["accountName"])
                    } else {
                        name
                    };
                    header.merge_missing(AccountProfile {
                        name,
                        handle: text_label(&hdr["channelHandle"]),
                        email: text_label(&hdr["email"]),
                    });
                }
                if let Some(item) = map
                    .get("accountItemRenderer")
                    .or_else(|| map.get("accountItem"))
                {
                    let byline = text_label(&item["accountByline"]);
                    let explicit_handle = text_label(&item["channelHandle"]);
                    let explicit_email = text_label(&item["email"]);
                    let handle = if !explicit_handle.is_empty() {
                        explicit_handle
                    } else if byline.starts_with('@') {
                        byline.clone()
                    } else {
                        String::new()
                    };
                    let email = if !explicit_email.is_empty() {
                        explicit_email
                    } else if byline.contains('@') && !byline.starts_with('@') {
                        byline
                    } else {
                        String::new()
                    };
                    let mut candidate = AccountProfile {
                        name: text_label(&item["accountName"]),
                        handle,
                        email,
                    };
                    candidate.normalize();
                    if item["isSelected"] == true {
                        selected_item.merge_missing(candidate);
                    } else {
                        fallback_item.merge_missing(candidate);
                    }
                }
                if let Some(ch) = map.get("accountChannelRenderer") {
                    let mut candidate = AccountProfile {
                        name: text_label(&ch["title"]),
                        handle: text_label(&ch["channelHandle"]),
                        email: text_label(&ch["email"]),
                    };
                    candidate.normalize();
                    fallback_item.merge_missing(candidate);
                }
                for child in map.values() {
                    walk(child, depth + 1, header, selected_item, fallback_item);
                }
            }
            _ => {}
        }
    }

    let mut header = AccountProfile::default();
    let mut selected_item = AccountProfile::default();
    let mut fallback_item = AccountProfile::default();
    walk(
        value,
        0,
        &mut header,
        &mut selected_item,
        &mut fallback_item,
    );
    header.merge_missing(selected_item);
    if header.name.is_empty() || (header.handle.is_empty() && header.email.is_empty()) {
        header.merge_missing(fallback_item);
    }
    header.normalize();
    header
}

pub(crate) fn parse_playlists(value: &Value) -> Vec<Playlist> {
    fn walk(
        value: &Value,
        out: &mut Vec<Playlist>,
        seen: &mut HashSet<String>,
        depth: u8,
    ) {
        if out.len() >= 100 || depth >= 16 {
            return;
        }
        match value {
            Value::Array(list) => {
                for item in list {
                    walk(item, out, seen, depth + 1);
                }
            }
            Value::Object(map) => {
                if let Some(tile) = map.get("tileRenderer") {
                    if tile["contentType"] == "TILE_CONTENT_TYPE_PLAYLIST" {
                        let id = tile["onSelectCommand"]["watchEndpoint"]["playlistId"]
                            .as_str()
                            .or_else(|| {
                                tile["onSelectCommand"]["browseEndpoint"]["browseId"]
                                    .as_str()
                                    .map(|s| s.strip_prefix("VL").unwrap_or(s))
                            })
                            .unwrap_or("");
                        let title = label(&tile["metadata"]["tileMetadataRenderer"]["title"]);
                        let lines0 = &tile["metadata"]["tileMetadataRenderer"]["lines"][0];
                        let count = find(lines0, "lineItemRenderer", 0)
                            .map(|n| label(&n["text"]))
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| label(lines0));
                        push_playlist(id, title, count, thumb_url(tile), out, seen);
                    }
                    return;
                }
                for key in ["gridPlaylistRenderer", "playlistRenderer", "compactPlaylistRenderer"] {
                    if let Some(item) = map.get(key) {
                        let id = item["playlistId"]
                            .as_str()
                            .or_else(|| {
                                item["navigationEndpoint"]["browseEndpoint"]["browseId"]
                                    .as_str()
                                    .map(|s| s.strip_prefix("VL").unwrap_or(s))
                            })
                            .unwrap_or("");
                        let title = label(&item["title"]);
                        let count = ["videoCountShortText", "videoCountText"]
                            .into_iter()
                            .map(|k| label(&item[k]))
                            .find(|s| !s.is_empty())
                            .unwrap_or_default();
                        push_playlist(id, title, count, thumb_url(item), out, seen);
                        return;
                    }
                }
                if let Some(lockup) = map.get("lockupViewModel") {
                    if lockup["contentType"] == "LOCKUP_CONTENT_TYPE_PLAYLIST" {
                        let id = lockup["contentId"]
                            .as_str()
                            .map(|s| s.strip_prefix("VL").unwrap_or(s))
                            .unwrap_or("");
                        let title = label(
                            &lockup["metadata"]["lockupMetadataViewModel"]["title"]["content"],
                        );
                        let count = find(lockup, "thumbnailBadgeViewModel", 0)
                            .map(|b| label(&b["text"]))
                            .unwrap_or_default();
                        push_playlist(id, title, count, thumb_url(lockup), out, seen);
                    }
                    return;
                }
                for child in map.values() {
                    walk(child, out, seen, depth + 1);
                }
            }
            _ => {}
        }
    }
    fn push_playlist(
        id: &str,
        title: String,
        count: String,
        thumb: String,
        out: &mut Vec<Playlist>,
        seen: &mut HashSet<String>,
    ) {
        if !valid_playlist_id(id) || id.starts_with("RD") || title.is_empty() {
            return;
        }
        if seen.insert(id.to_string()) {
            out.push(Playlist {
                id: id.to_string(),
                title,
                count,
                thumb,
            });
        }
    }
    let mut out = Vec::new();
    walk(value, &mut out, &mut HashSet::new(), 0);
    out
}

fn has_live_badge(item: &Value) -> bool {
    if item["badges"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| {
            matches!(
                b["metadataBadgeRenderer"]["style"].as_str(),
                Some("BADGE_STYLE_TYPE_LIVE_NOW")
            ) || label(&b["metadataBadgeRenderer"]["label"])
                .to_lowercase()
                .contains("эфир")
        })
    {
        return true;
    }
    item["thumbnailOverlays"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|o| {
            matches!(
                o["thumbnailOverlayTimeStatusRenderer"]["style"].as_str(),
                Some("LIVE")
            )
        })
}

pub(crate) fn parse_search_items(
    value: &Value,
    want_kind: &str,
    out: &mut Vec<SearchItem>,
    seen: &mut HashSet<String>,
    depth: u8,
) {
    if out.len() >= 30 || depth >= 16 {
        return;
    }
    let allow_video = want_kind != "playlist";
    let allow_playlist = want_kind == "playlist" || want_kind == "all";
    match value {
        Value::Array(list) => {
            for item in list {
                parse_search_items(item, want_kind, out, seen, depth + 1);
            }
        }
        Value::Object(map) => {
            if let Some(video) = map.get("videoRenderer") {
                if allow_video {
                    let id = video["videoId"].as_str().unwrap_or("");
                    let title = label(&video["title"]);
                    if valid_id(id)
                        && !title.is_empty()
                        && seen.insert(format!("video:{id}"))
                    {
                        let author = ["shortBylineText", "ownerText", "longBylineText"]
                            .into_iter()
                            .map(|k| label(&video[k]))
                            .find(|s| !s.is_empty())
                            .unwrap_or_default();
                        let mut duration = label(&video["lengthText"]);
                        if duration.is_empty() && has_live_badge(video) {
                            duration = "Эфир".into();
                        }
                        let views = ["shortViewCountText", "viewCountText"]
                            .into_iter()
                            .map(|k| label(&video[k]))
                            .find(|s| !s.is_empty())
                            .unwrap_or_default();
                        let published = label(&video["publishedTimeText"]);
                        let mut thumb = thumb_url(video);
                        if thumb.is_empty() {
                            thumb = format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg");
                        }
                        out.push(SearchItem {
                            kind: "video".into(),
                            id: id.into(),
                            title,
                            author,
                            duration,
                            views,
                            published,
                            count: String::new(),
                            thumb,
                        });
                    }
                }
                return;
            }
            if let Some(pl) = map.get("playlistRenderer") {
                if allow_playlist {
                    let id = pl["playlistId"]
                        .as_str()
                        .or_else(|| {
                            pl["navigationEndpoint"]["browseEndpoint"]["browseId"]
                                .as_str()
                                .map(|s| s.strip_prefix("VL").unwrap_or(s))
                        })
                        .unwrap_or("");
                    let title = label(&pl["title"]);
                    if valid_playlist_id(id)
                        && !id.starts_with("RD")
                        && !title.is_empty()
                        && seen.insert(format!("playlist:{id}"))
                    {
                        let author = ["shortBylineText", "longBylineText"]
                            .into_iter()
                            .map(|k| label(&pl[k]))
                            .find(|s| !s.is_empty())
                            .unwrap_or_default();
                        let count = ["videoCountShortText", "videoCountText"]
                            .into_iter()
                            .map(|k| label(&pl[k]))
                            .find(|s| !s.is_empty())
                            .unwrap_or_default();
                        out.push(SearchItem {
                            kind: "playlist".into(),
                            id: id.into(),
                            title,
                            author,
                            duration: String::new(),
                            views: String::new(),
                            published: String::new(),
                            count,
                            thumb: thumb_url(pl),
                        });
                    }
                }
                return;
            }
            if let Some(lockup) = map.get("lockupViewModel") {
                let ctype = lockup["contentType"].as_str().unwrap_or("");
                let meta = &lockup["metadata"]["lockupMetadataViewModel"];
                let title = label(&meta["title"]["content"]);
                let rows = meta["metadata"]["contentMetadataViewModel"]["metadataRows"]
                    .as_array();
                let mut author = String::new();
                let mut views = String::new();
                let mut published = String::new();
                if let Some(rows) = rows {
                    if let Some(first) = rows.first() {
                        author = label(&first["metadataParts"][0]["text"]["content"]);
                    }
                    let mut extra = rows
                        .iter()
                        .skip(1)
                        .flat_map(|r| r["metadataParts"].as_array().into_iter().flatten())
                        .map(|p| label(&p["text"]["content"]))
                        .filter(|s| !s.is_empty());
                    views = extra.next().unwrap_or_default();
                    published = extra.next().unwrap_or_default();
                }
                let badge = find(lockup, "thumbnailBadgeViewModel", 0)
                    .map(|b| label(&b["text"]))
                    .unwrap_or_default();
                if ctype == "LOCKUP_CONTENT_TYPE_VIDEO" && allow_video {
                    let id = lockup["contentId"].as_str().unwrap_or("");
                    if valid_id(id)
                        && !title.is_empty()
                        && seen.insert(format!("video:{id}"))
                    {
                        let mut thumb = thumb_url(lockup);
                        if thumb.is_empty() {
                            thumb = format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg");
                        }
                        out.push(SearchItem {
                            kind: "video".into(),
                            id: id.into(),
                            title,
                            author,
                            duration: badge,
                            views,
                            published,
                            count: String::new(),
                            thumb,
                        });
                    }
                } else if ctype == "LOCKUP_CONTENT_TYPE_PLAYLIST" && allow_playlist {
                    let id = lockup["contentId"]
                        .as_str()
                        .map(|s| s.strip_prefix("VL").unwrap_or(s))
                        .unwrap_or("");
                    if valid_playlist_id(id)
                        && !id.starts_with("RD")
                        && !title.is_empty()
                        && seen.insert(format!("playlist:{id}"))
                    {
                        out.push(SearchItem {
                            kind: "playlist".into(),
                            id: id.into(),
                            title,
                            author,
                            duration: String::new(),
                            views: String::new(),
                            published: String::new(),
                            count: badge,
                            thumb: thumb_url(lockup),
                        });
                    }
                }
                return;
            }
            for name in [
                "contents",
                "itemSectionRenderer",
                "sectionListRenderer",
                "shelfRenderer",
                "richItemRenderer",
                "gridRenderer",
                "horizontalListRenderer",
                "richGridRenderer",
                "richShelfRenderer",
                "twoColumnSearchResultsRenderer",
                "primaryContents",
                "twoColumnBrowseResultsRenderer",
                "tabs",
                "tabRenderer",
                "content",
                "items",
                "results",
                "onResponseReceivedActions",
                "appendContinuationItemsAction",
                "continuationItems",
            ] {
                if let Some(child) = map.get(name) {
                    parse_search_items(child, want_kind, out, seen, depth + 1);
                }
            }
        }
        _ => {}
    }
}
pub fn valid_playlist_id(id: &str) -> bool {
    (2..=180).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}
pub fn playlist_id_from_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim())
        .or_else(|_| Url::parse(&format!("https://{}", raw.trim())))
        .ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    if host != "youtube.com" && !host.ends_with(".youtube.com") {
        return None;
    }
    let path = url.path().trim_matches('/');
    let has_v = url.query_pairs().any(|(k, v)| k == "v" && !v.is_empty());
    if path == "playlist" || (path == "watch" && !has_v) {
        return url
            .query_pairs()
            .find(|(k, v)| k == "list" && valid_playlist_id(v))
            .map(|(_, v)| v.into_owned());
    }
    None
}
fn identified(id: Option<&str>, title: String, author: String) -> Option<Track> {
    let id = id.filter(|id| valid_id(id))?;
    (!title.is_empty()).then(|| Track {
        id: id.into(),
        title,
        author,
    })
}
fn find<'a>(value: &'a Value, property: &str, depth: u8) -> Option<&'a Value> {
    if depth > 16 {
        return None;
    }
    match value {
        Value::Object(fields) => fields
            .get(property)
            .or_else(|| fields.values().find_map(|v| find(v, property, depth + 1))),
        Value::Array(items) => items.iter().find_map(|v| find(v, property, depth + 1)),
        _ => None,
    }
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
pub fn video_id(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim())
        .or_else(|_| Url::parse(&format!("https://{}", raw.trim())))
        .ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path().trim_matches('/');
    if host == "youtu.be" {
        return valid_id(path).then(|| path.into());
    }
    if host != "youtube.com" && !host.ends_with(".youtube.com") {
        return None;
    }
    if path == "watch" {
        return url
            .query_pairs()
            .find(|(k, v)| k == "v" && valid_id(v))
            .map(|(_, v)| v.into_owned());
    }
    let (kind, id) = path.split_once('/')?;
    (matches!(kind, "shorts" | "embed" | "live") && valid_id(id)).then(|| id.into())
}
pub fn login_required(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "login_required",
        "age_check_required",
        "sign in",
        "войдите",
        "content_check_required",
    ]
    .iter()
    .any(|s| lower.contains(s))
}
async fn formats(
    data: &Value,
    id: &str,
    client: &'static str,
    agent: &'static str,
    prepared: Option<&Arc<Prepared>>,
    ready: Instant,
) -> Result<(Track, Stream), String> {
    let status = data["playabilityStatus"]["status"]
        .as_str()
        .ok_or("Нет статуса видео YouTube")?;
    if status != "OK" {
        let reason = label(&data["playabilityStatus"]["reason"]);
        return Err(format!("YouTube: {status} {reason}"));
    }
    let mut title = label(&data["videoDetails"]["title"]).trim().to_string();
    if title.is_empty() {
        title = label(&data["microformat"]["playerMicroformatRenderer"]["title"])
            .trim()
            .to_string();
    }
    let mut author = label(&data["videoDetails"]["author"]).trim().to_string();
    if author.is_empty() {
        author = label(&data["microformat"]["playerMicroformatRenderer"]["ownerChannelName"])
            .trim()
            .to_string();
    }
    let track = Track {
        id: id.into(),
        title,
        author,
    };
    let mut candidates: Vec<_> = data["streamingData"]["adaptiveFormats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(item["itag"].as_u64(), Some(249..=251 | 774))
                && item["mimeType"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("audio/webm"))
        })
        .collect();
    candidates.sort_by_key(|v| {
        let rate = v["averageBitrate"]
            .as_u64()
            .or_else(|| v["bitrate"].as_u64())
            .unwrap_or(0);
        std::cmp::Reverse((
            v["audioTrack"]["audioIsDefault"] != false,
            v["isDrc"] != true,
            rate,
            v["itag"].as_u64(),
        ))
    });
    for item in candidates {
        let Ok(url) = link(item, prepared).await else {
            continue;
        };
        let length = item["contentLength"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        let duration = item["approxDurationMs"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_default() as f64
            / 1000.0;
        let bitrate = item["averageBitrate"]
            .as_u64()
            .or_else(|| item["bitrate"].as_u64())
            .unwrap_or(0);
        return Ok((
            track,
            Stream {
                url,
                agent,
                client,
                length,
                duration,
                ready,
                itag: item["itag"].as_u64().unwrap(),
                bitrate,
            },
        ));
    }
    Err("YouTube не вернул доступную аудиодорожку Opus".into())
}
pub(crate) fn valid_media_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|h| h.ends_with(".googlevideo.com"))
}
async fn link(item: &Value, prepared: Option<&Arc<Prepared>>) -> Result<Url, String> {
    let query: BTreeMap<String, String> = reqwest::Url::parse(&format!(
        "https://www.youtube.com/?{}",
        item["signatureCipher"].as_str().unwrap_or("")
    ))
    .map_err(|_| "Неверная ссылка YouTube")?
    .query_pairs()
    .into_owned()
    .collect();
    let raw = item["url"]
        .as_str()
        .or_else(|| query.get("url").map(String::as_str))
        .ok_or("Нет ссылки YouTube")?;
    let mut url = Url::parse(raw).map_err(|_| "Неверная ссылка YouTube")?;
    if !valid_media_url(&url) {
        return Err("Неверный адрес аудио YouTube".into());
    }
    let signature = query.get("s").cloned().unwrap_or_default();
    let n = url
        .query_pairs()
        .find(|(k, _)| k == "n")
        .map(|(_, n)| n.into_owned())
        .unwrap_or_default();
    if signature.is_empty() && n.is_empty() {
        return Ok(url);
    }
    let prepared = prepared.ok_or("Требуется подпись плеера YouTube")?;
    let sp = query.get("sp").map(String::as_str).unwrap_or("signature");
    if !matches!(sp, "sig" | "signature") {
        return Err("Неизвестный параметр подписи YouTube".into());
    }
    let (new_sig, new_n) = prepared.decode(signature.clone(), n.clone()).await?;
    if (!signature.is_empty() && new_sig.is_empty())
        || (!n.is_empty()
            && (new_n.is_empty() || new_n == n || new_n.starts_with("enhanced_except_")))
    {
        return Err("Плеер не принял подпись YouTube".into());
    }
    let pairs: Vec<_> = url
        .query_pairs()
        .into_owned()
        .filter(|(key, _)| key != "n" && (new_sig.is_empty() || key != sp))
        .collect();
    let mut params = url.query_pairs_mut();
    params.clear().extend_pairs(pairs);
    if !new_sig.is_empty() {
        params.append_pair(sp, &new_sig);
    }
    if !new_n.is_empty() {
        params.append_pair("n", &new_n);
    }
    drop(params);
    Ok(url)
}

fn ad_wait(data: &Value) -> Duration {
    fn scan(value: &Value, total: &mut u64, depth: u8) {
        if depth > 24 || *total >= 60_000 {
            return;
        }
        match value {
            Value::Object(fields) => {
                for (key, child) in fields {
                    if key == "instreamVideoAdRenderer" {
                        let seconds = child["playerVars"].as_str().and_then(|vars| {
                            vars.split('&').find_map(|item| {
                                item.strip_prefix("length_seconds=")?.parse::<f64>().ok()
                            })
                        });
                        let delay = child["skipOffsetMilliseconds"]
                            .as_u64()
                            .or_else(|| seconds.map(|v| (v * 1000.0) as u64))
                            .unwrap_or(0);
                        *total = total.saturating_add(delay);
                    } else {
                        scan(child, total, depth + 1);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    scan(item, total, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut total = 0;
    for (key, config, property, expected) in [
        (
            "adPlacements",
            "adPlacementConfig",
            "kind",
            "AD_PLACEMENT_KIND_START",
        ),
        (
            "adSlots",
            "adSlotMetadata",
            "triggerEvent",
            "SLOT_TRIGGER_EVENT_BEFORE_CONTENT",
        ),
    ] {
        for entry in data[key].as_array().into_iter().flatten() {
            if find(entry, config, 0).is_some_and(|metadata| metadata[property] == expected) {
                scan(entry, &mut total, 0);
            }
        }
    }
    Duration::from_millis(total.min(60_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderName, HeaderValue};

    #[test]
    fn strip_bearer_removes_oauth_and_keeps_cookies() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer ya29.test-token"));
        h.insert(HeaderName::from_static("x-test"), HeaderValue::from_static("ok"));
        strip_bearer(&mut h);
        assert!(!h.contains_key(AUTHORIZATION));
        assert_eq!(h.get("x-test").unwrap(), "ok");

        let mut cookies = HeaderMap::new();
        cookies.insert(AUTHORIZATION, HeaderValue::from_static("SAPISIDHASH 123_abc"));
        cookies.insert(COOKIE, HeaderValue::from_static("SID=abc; SAPISID=xyz"));
        strip_bearer(&mut cookies);
        assert_eq!(cookies.get(AUTHORIZATION).unwrap(), "SAPISIDHASH 123_abc");
        assert_eq!(cookies.get(COOKIE).unwrap(), "SID=abc; SAPISID=xyz");
    }

    #[test]
    fn validates_playlist_ids_urls_and_thumbnails() {
        assert!(valid_playlist_id("PL123abc_-XYZ"));
        assert!(valid_playlist_id("LM"));
        assert!(!valid_playlist_id(""));
        assert!(!valid_playlist_id("a"));
        assert!(!valid_playlist_id("PL bad/char"));

        assert_eq!(
            playlist_id_from_url("https://www.youtube.com/playlist?list=PLabc123_-").as_deref(),
            Some("PLabc123_-")
        );
        assert_eq!(
            playlist_id_from_url("https://music.youtube.com/watch?list=PLmusic99").as_deref(),
            Some("PLmusic99")
        );
        assert!(
            playlist_id_from_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc123")
                .is_none()
        );
        assert!(playlist_id_from_url("https://evil.com/playlist?list=PLabc123").is_none());

        assert_eq!(
            safe_thumb("https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg"),
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg"
        );
        assert_eq!(
            safe_thumb("https://yt3.googleusercontent.com/abc=s200"),
            "https://yt3.googleusercontent.com/abc=s200"
        );
        assert_eq!(safe_thumb("http://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg"), "");
        assert_eq!(safe_thumb("https://evil.example/img.jpg"), "");
        assert_eq!(safe_thumb("https://i.ytimg.com:8443/a.jpg"), "");
    }

    #[test]
    fn parses_playlists_across_tv_web_and_lockup_and_skips_rd_mixes() {
        let data = json!({
            "contents": [
                {
                    "tileRenderer": {
                        "contentType": "TILE_CONTENT_TYPE_PLAYLIST",
                        "header": {
                            "tileHeaderRenderer": {
                                "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/vi/1/mqdefault.jpg", "width": 320}]}
                            }
                        },
                        "metadata": {
                            "tileMetadataRenderer": {
                                "title": {"simpleText": "Lo-Fi Beats"},
                                "lines": [{"lineRenderer": {"items": [{"lineItemRenderer": {"text": {"simpleText": "42 видео"}}}]}}]
                            }
                        },
                        "onSelectCommand": {"watchEndpoint": {"playlistId": "PL_tv_lofi_1"}}
                    }
                },
                {
                    "tileRenderer": {
                        "contentType": "TILE_CONTENT_TYPE_PLAYLIST",
                        "metadata": {"tileMetadataRenderer": {"title": {"simpleText": "My Mix"}}},
                        "onSelectCommand": {"watchEndpoint": {"playlistId": "RDMM123456"}}
                    }
                },
                {
                    "gridPlaylistRenderer": {
                        "playlistId": "PL_web_grid_2",
                        "title": {"runs": [{"text": "В машину"}]},
                        "videoCountShortText": {"simpleText": "18"},
                        "thumbnail": {"thumbnails": [{"url": "https://i9.ytimg.com/vi/2/mqdefault.jpg", "width": 320}]}
                    }
                },
                {
                    "playlistRenderer": {
                        "playlistId": "PL_search_3",
                        "title": {"simpleText": "Synthwave Radio"},
                        "videoCountText": {"runs": [{"text": "60 видео"}]}
                    }
                },
                {
                    "lockupViewModel": {
                        "contentType": "LOCKUP_CONTENT_TYPE_PLAYLIST",
                        "contentId": "VLPL_lockup_4",
                        "metadata": {"lockupMetadataViewModel": {"title": {"content": "Deep Focus"}}},
                        "contentImage": {
                            "collectionThumbnailViewModel": {
                                "primaryThumbnail": {
                                    "thumbnailViewModel": {
                                        "image": {"sources": [{"url": "https://i.ytimg.com/vi/3/hqdefault.jpg", "width": 480}]},
                                        "overlays": [{"thumbnailOverlayBadgeViewModel": {"thumbnailBadges": [{"thumbnailBadgeViewModel": {"text": "25 видео"}}]}}]
                                    }
                                }
                            }
                        }
                    }
                }
            ]
        });

        let parsed = parse_playlists(&data);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].id, "PL_tv_lofi_1");
        assert_eq!(parsed[0].title, "Lo-Fi Beats");
        assert_eq!(parsed[0].count, "42 видео");
        assert_eq!(parsed[0].thumb, "https://i.ytimg.com/vi/1/mqdefault.jpg");

        assert_eq!(parsed[1].id, "PL_web_grid_2");
        assert_eq!(parsed[1].title, "В машину");
        assert_eq!(parsed[1].count, "18");

        assert_eq!(parsed[2].id, "PL_search_3");
        assert_eq!(parsed[2].count, "60 видео");

        assert_eq!(parsed[3].id, "PL_lockup_4");
        assert_eq!(parsed[3].title, "Deep Focus");
        assert_eq!(parsed[3].count, "25 видео");
        assert_eq!(parsed[3].thumb, "https://i.ytimg.com/vi/3/hqdefault.jpg");
    }

    #[test]
    fn parses_playlist_tracks_shelf_and_continuation() {
        let page = json!({
            "contents": {
                "twoColumnBrowseResultsRenderer": {
                    "tabs": [{
                        "tabRenderer": {
                            "content": {
                                "sectionListRenderer": {
                                    "contents": [{
                                        "itemSectionRenderer": {
                                            "contents": [{
                                                "playlistVideoListRenderer": {
                                                    "contents": [
                                                        {
                                                            "playlistVideoRenderer": {
                                                                "videoId": "dQw4w9WgXcQ",
                                                                "title": {"runs": [{"text": "Track One"}]},
                                                                "shortBylineText": {"runs": [{"text": "Artist One"}]}
                                                            }
                                                        },
                                                        {
                                                            "continuationItemRenderer": {
                                                                "continuationEndpoint": {
                                                                    "continuationCommand": {"token": "NEXT_PAGE_TOK"}
                                                                }
                                                            }
                                                        }
                                                    ]
                                                }
                                            }]
                                        }
                                    }]
                                }
                            }
                        }
                    }]
                },
                "unrelatedShelf": {
                    "videoRenderer": {
                        "videoId": "jNQXAC9IVRw",
                        "title": {"simpleText": "Unrelated Suggestion"}
                    }
                }
            }
        });
        let shelf = browse_contents(&page);
        let mut tracks = Vec::new();
        organic_limit(shelf, &mut tracks, &mut HashSet::new(), 0, 100);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "dQw4w9WgXcQ");
        assert_eq!(tracks[0].title, "Track One");
        assert_eq!(tracks[0].author, "Artist One");
        assert_eq!(continuation_token(shelf).as_deref(), Some("NEXT_PAGE_TOK"));
    }

    #[test]
    fn parses_account_profile_from_menu_and_accounts_list() {
        let menu = json!({
            "actions": [{
                "openPopupAction": {
                    "popup": {
                        "multiPageMenuRenderer": {
                            "header": {
                                "activeAccountHeaderRenderer": {
                                    "accountName": {"simpleText": "NewFPV"},
                                    "channelHandle": {"simpleText": "@newfpv"},
                                    "email": {"simpleText": "user@example.com"}
                                }
                            }
                        }
                    }
                }
            }]
        });
        assert_eq!(
            parse_account_profile(&menu),
            AccountProfile {
                name: "NewFPV".into(),
                handle: "@newfpv".into(),
                email: "user@example.com".into(),
            }
        );

        let accounts_list = json!({
            "contents": [{
                "accountSectionListRenderer": {
                    "contents": [
                        {
                            "accountItemRenderer": {
                                "isSelected": false,
                                "accountName": {"simpleText": "Другой канал"},
                                "accountByline": {"simpleText": "@other"}
                            }
                        },
                        {
                            "accountItemRenderer": {
                                "isSelected": true,
                                "accountName": {"runs": [{"text": "Основной канал"}]},
                                "accountByline": {"simpleText": "main@example.com"}
                            }
                        }
                    ]
                }
            }]
        });
        assert_eq!(
            parse_account_profile(&accounts_list),
            AccountProfile {
                name: "Основной канал".into(),
                handle: String::new(),
                email: "main@example.com".into(),
            }
        );

        let tv_accounts = json!({
            "contents": [{
                "accountSectionListRenderer": {
                    "contents": [{
                        "accountItemSectionRenderer": {
                            "header": {
                                "googleAccountHeaderRenderer": {
                                    "name": {"simpleText": "Alex"},
                                    "email": {"simpleText": "alex@gmail.com"}
                                }
                            },
                            "contents": [{
                                "accountItem": {
                                    "accountName": {"runs": [{"text": "Alex TV"}]},
                                    "accountByline": {"runs": [{"text": "@alextv"}]},
                                    "isSelected": true
                                }
                            }]
                        }
                    }]
                }
            }]
        });
        assert_eq!(
            parse_account_profile(&tv_accounts),
            AccountProfile {
                name: "Alex".into(),
                handle: "@alextv".into(),
                email: "alex@gmail.com".into(),
            }
        );
    }

    #[test]
    fn encodes_youtube_search_filter_params() {
        let f = |kind: &str, sort: &str, date: &str, duration: &str, live: bool| SearchFilter {
            kind: kind.into(),
            sort: sort.into(),
            date: date.into(),
            duration: duration.into(),
            live,
        };
        assert_eq!(encode_search_params(&f("all", "relevance", "", "", false)), None);
        assert_eq!(
            encode_search_params(&f("video", "relevance", "", "", false)).as_deref(),
            Some("EgIQAQ==")
        );
        assert_eq!(
            encode_search_params(&f("playlist", "relevance", "", "", false)).as_deref(),
            Some("EgIQAw==")
        );
        assert_eq!(
            encode_search_params(&f("all", "date", "", "", false)).as_deref(),
            Some("CAI=")
        );
        assert_eq!(
            encode_search_params(&f("all", "views", "", "", false)).as_deref(),
            Some("CAM=")
        );
        assert_eq!(
            encode_search_params(&f("all", "relevance", "today", "", false)).as_deref(),
            Some("EgIIAg==")
        );
        assert_eq!(
            encode_search_params(&f("all", "relevance", "", "short", false)).as_deref(),
            Some("EgIYAQ==")
        );
        assert_eq!(
            encode_search_params(&f("all", "relevance", "", "long", false)).as_deref(),
            Some("EgIYAg==")
        );
        assert_eq!(
            encode_search_params(&f("all", "relevance", "", "medium", false)).as_deref(),
            Some("EgIYAw==")
        );
        assert_eq!(
            encode_search_params(&f("all", "relevance", "", "", true)).as_deref(),
            Some("EgJAAQ==")
        );
        assert!(
            encode_search_params(&f("video", "views", "week", "medium", false)).is_some()
        );
        assert_eq!(
            encode_search_params(&f("playlist", "relevance", "", "short", true)).as_deref(),
            Some("EgIQAw==")
        );
    }

    #[tokio::test]
    async fn parses_search_items_and_microformat_title_fallback() {
        let search_resp = json!({
            "contents": [
                {
                    "videoRenderer": {
                        "videoId": "dQw4w9WgXcQ",
                        "title": {"runs": [{"text": "Never Gonna Give You Up"}]},
                        "ownerText": {"runs": [{"text": "Rick Astley"}]},
                        "lengthText": {"simpleText": "3:33"},
                        "shortViewCountText": {"simpleText": "1.5B views"},
                        "publishedTimeText": {"simpleText": "15 years ago"},
                        "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg", "width": 320}]}
                    }
                },
                {
                    "playlistRenderer": {
                        "playlistId": "PL_mix_123",
                        "title": {"simpleText": "80s Classics"},
                        "longBylineText": {"simpleText": "Curator"},
                        "videoCountShortText": {"simpleText": "50"}
                    }
                }
            ]
        });

        let mut vids = Vec::new();
        parse_search_items(&search_resp, "video", &mut vids, &mut HashSet::new(), 0);
        assert_eq!(vids.len(), 1);
        assert_eq!(vids[0].id, "dQw4w9WgXcQ");
        assert_eq!(vids[0].duration, "3:33");
        assert_eq!(vids[0].views, "1.5B views");

        let mut all = Vec::new();
        parse_search_items(&search_resp, "all", &mut all, &mut HashSet::new(), 0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].kind, "playlist");
        assert_eq!(all[1].count, "50");

        let player_resp = json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"title": "", "author": ""},
            "microformat": {
                "playerMicroformatRenderer": {
                    "title": {"simpleText": "ЗОМБИ (feat. DK, Серёга Пират)"},
                    "ownerChannelName": "Джарахов"
                }
            },
            "streamingData": {
                "adaptiveFormats": [{
                    "itag": 251,
                    "mimeType": "audio/webm; codecs=\"opus\"",
                    "url": "https://rr1---sn-test.googlevideo.com/videoplayback",
                    "approxDurationMs": "317000"
                }]
            }
        });
        let (track, stream) = formats(&player_resp, "s0lMgCAmlgY", "ANDROID_VR", "vr", None, Instant::now())
            .await
            .unwrap();
        assert_eq!(track.title, "ЗОМБИ (feat. DK, Серёга Пират)");
        assert_eq!(track.author, "Джарахов");
        assert!((stream.duration - 317.0).abs() < 0.01);
    }
}
