//! Инструменты для модели: умный дом (Яндекс, Home Assistant, Xiaomi, Smart Life — всё живёт в плагине
//! «Умный дом»), команды Алисе, инструменты плагинов. Простые команды колонка делает без модели (fastcmd).

use crate::{fastcmd, plugins::Host, settings::Store};
use serde_json::{Value, json};
use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

static DEFS: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(include_str!("tooldefs.json")).unwrap());

fn def(name: &str) -> Value {
    DEFS.get(name).cloned().unwrap_or(Value::Null)
}

pub type MusicFn = Arc<dyn Fn(String) -> crate::plugins::BoxFut<String> + Send + Sync>;
pub type MusicFastFn=Arc<dyn Fn(String)->crate::plugins::BoxFut<Option<String>>+Send+Sync>;

pub struct Tools {
    pub host: Arc<Host>,
    pub store: Store,
    /// Включить с YouTube (None — нельзя).
    pub music: Option<MusicFn>,
    /// Управлять тем, что играет с YouTube.
    pub music_ctl: Option<MusicFn>,
    pub music_fast:Option<MusicFastFn>,
    /// Добавить в очередь YouTube.
    pub music_queue: Option<MusicFn>,
    /// Отменить незавершённый запуск музыки (например, по тайм-ауту инструмента).
    pub music_cancel: Option<Arc<dyn Fn() -> crate::plugins::BoxFut<()> + Send + Sync>>,
    /// Общая память: «вспомни», «забудь/запомни».
    pub memory: Option<Arc<crate::memory::Memory>>,
}

const DAYS: [&str; 7] = ["воскресенье", "понедельник", "вторник", "среда", "четверг", "пятница", "суббота"];

fn room_line(room: &str) -> String {
    format!(
        "\nКолонка стоит в комнате «{room}». Если хозяин не назвал комнату («выключи свет», «включи кондиционер») — \
имеется в виду эта комната; «весь свет» — все лампы и выключатели света во всём доме."
    )
}

fn as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

impl Tools {
    pub fn new(host: Arc<Host>, store: Store) -> Tools {
        Tools { host, store, music: None, music_ctl: None, music_fast:None, music_queue: None, music_cancel: None, memory: None }
    }
    pub async fn cancel_loading_music(&self) {
        if let Some(cancel) = &self.music_cancel {
            cancel().await;
        }
    }
    /// Каталог из контекста определяет доступность инструментов уже в первой реплике.
    pub async fn prepare(&self)->(String,Vec<Value>) {
        let context=self.context().await;
        let definitions=self.definitions().await;
        (context,definitions)
    }

    /// Запрос к плагину, который даёт возможность cap.
    pub async fn service(&self, cap: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let n = self.host.provider(cap)?;
        n.request(method, params, timeout).await
    }

    /// Галочка «через что управлять домом» и плагин на месте.
    pub fn use_ch(&self, ch: &str) -> bool {
        if ch == "yandex" && self.host.provider("yandexhome").is_err() {
            return false;
        }
        self.store.with(|s| s.use_home(ch))
    }

    fn ready(&self, ch: &str) -> bool {
        let cap = match ch {
            "homeassistant" | "xiaomi" | "smartlife" => ch,
            _ => return false,
        };
        self.store.with(|s| s.home_connected(ch)) && self.host.provider(cap).is_ok()
    }

    fn room(&self) -> String {
        self.store.with(|s| s.speaker_room.trim().to_string())
    }

    async fn yandex(&self, op: &str, args: Value, timeout: Duration) -> Result<Value, String> {
        let (token, alice) = (self.store.with(|s| s.yandex_token.clone()), self.use_ch("alice"));
        self.service("yandexhome", "yandexhome", json!({"op": op, "args": args, "token": token, "room": self.room(), "alice": alice}), timeout).await
    }

    async fn vendor(&self, ch: &str, op: &str, args: Value, timeout: Duration) -> Result<Value, String> {
        let mut p = json!({"op": op, "args": args});
        if ch == "homeassistant" {
            p["config"] = self.store.with(|s| s.home_assistant.clone());
        }
        self.service(ch, ch, p, timeout).await
    }

    fn has_token(&self) -> bool {
        self.store.with(|s| !s.yandex_token.is_empty())
    }

    async fn has_catalog(&self) -> bool {
        self.yandex("has-catalog", Value::Null, Duration::from_secs(1)).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
    }

    pub async fn prewarm(&self) {
        if self.has_token() && self.use_ch("yandex") {
            let _ = self.yandex("prewarm", Value::Null, Duration::from_secs(6)).await;
        }
    }

    /// Быстрая команда Яндекс Умного дома без модели.
    pub async fn fast(&self, text: &str) -> Option<String> {
        if !self.has_token() || !self.use_ch("yandex") {
            return None;
        }
        let v = self.yandex("fast", text.into(), Duration::from_secs(6)).await.ok()?;
        v.get("handled").and_then(Value::as_bool).unwrap_or(false).then(|| as_str(&v["answer"]))
    }

    /// Smart Life без модели.
    pub async fn smartlife_fast(&self, text: &str) -> Option<String> {
        if !self.ready("smartlife") || !self.use_ch("smartlife") {
            return None;
        }
        match self.vendor("smartlife", "fast", text.into(), Duration::from_secs(6)).await {
            Err(e) => Some(e),
            Ok(v) => v.get("handled").and_then(Value::as_bool).unwrap_or(false).then(|| as_str(&v["answer"])),
        }
    }

    pub async fn ha_alive(&self) -> bool {
        self.ready("homeassistant")
            && self.vendor("homeassistant", "alive", Value::Null, Duration::from_secs(2)).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
    }

    /// Пылесос в Home Assistant — быстрым путём, без модели.
    pub async fn ha_vacuum(&self, text: &str) -> Option<String> {
        if !self.ready("homeassistant") || !self.use_ch("homeassistant") {
            return None;
        }
        let intent = self.vendor("homeassistant", "intent", text.into(), Duration::from_secs(1)).await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
        if !intent || !self.ha_alive().await {
            return None;
        }
        Some(match self.vendor("homeassistant", "command", text.into(), Duration::from_secs(20)).await {
            Ok(v) => as_str(&v),
            Err(e) => e,
        })
    }

    /// Короткая команда умного дома или фраза сценария — её можно отдать Алисе целиком.
    pub async fn home_command(&self, text: &str) -> Option<String> {
        if fastcmd::youtube_request(text) || fastcmd::clarification(text).is_some() {
            return None;
        }
        if self.direct_home_owns(text).await {
            return None;
        }
        if let Some(b) = fastcmd::home_command_local(text) {
            return Some(b);
        }
        let bare = fastcmd::strip_lead(&fastcmd::norm_text(text));
        if bare.is_empty() || bare.split_whitespace().count() > 9 || !self.has_token() {
            return None;
        }
        let m = self.yandex("scenario-match", bare.clone().into(), Duration::from_secs(6)).await.ok()?;
        (m.as_bool() == Some(true)).then_some(bare)
    }

    async fn direct_home_owns(&self, text: &str) -> bool {
        if self.ready("xiaomi") && self.use_ch("xiaomi") {
            if let Ok(Value::Array(devices)) =
                self.vendor("xiaomi", "devices", Value::Null, Duration::from_millis(600)).await
                && devices.iter().any(|d| device_matches_command(text, d))
            {
                return true;
            }
        }
        if self.use_ch("homeassistant") && self.ha_alive().await {
            if let Ok(Value::Array(entities)) =
                self.vendor("homeassistant", "catalog", json!(false), Duration::from_millis(600)).await
                && entities.iter().any(|d| device_matches_command(text, d))
            {
                return true;
            }
        }
        false
    }

    /// Текст для инструкций бэкенда: время, устройства с возможностями, сценарии с фразами.
    pub async fn context(&self) -> String {
        let now = self.store.with(|s| s.now());
        let mut b = format!("\n\nСейчас {}, {}.", now.strftime("%d.%m.%Y %H:%M"), DAYS[now.weekday().to_sunday_zero_offset() as usize]);
        // Independent providers must not add their cold-start delays together.
        // Definitions are still prepared after context, so their first catalog is ready.
        let ha=async{if self.use_ch("homeassistant") && self.ha_alive().await{as_str(&self.vendor("homeassistant","context",Value::Null,Duration::from_secs(4)).await.unwrap_or_default())}else{String::new()}};
        let vendor=|ch|async move{if self.use_ch(ch) && self.ready(ch){as_str(&self.vendor(ch,"context",Value::Null,Duration::from_secs(5)).await.unwrap_or_default())}else{String::new()}};
        let yandex=async{if self.use_ch("yandex"){self.yandex("context",Value::Null,Duration::from_secs(5)).await.map(|v|as_str(&v)).unwrap_or_default()}else{String::new()}};
        let (ha,xiaomi,smartlife,yandex)=tokio::join!(ha,vendor("xiaomi"),vendor("smartlife"),yandex);
        b+=&ha;b+=&xiaomi;b+=&smartlife;
        if !self.use_ch("yandex") {
            let room = self.room();
            if !room.is_empty() {
                b += &room_line(&room);
            }
            return b;
        }
        b+=&yandex;
        b
    }

    pub async fn definitions(&self) -> Vec<Value> {
        let mut defs = vec![];
        let available=|ch|async move {self.ready(ch) && self.use_ch(ch) && self.vendor(ch,"devices",Value::Null,Duration::from_secs(5)).await.ok().and_then(|v|v.as_array().map(Vec::len)).unwrap_or(0)>0};
        let (ha,xiaomi,smartlife)=tokio::join!(async{self.use_ch("homeassistant") && self.ha_alive().await},available("xiaomi"),available("smartlife"));
        if ha {
            defs.push(def("home_assistant"));
        }
        for (available,name) in [(xiaomi,"xiaomi_home"),(smartlife,"smart_life")] {
            if available{defs.push(def(name));}
        }
        if self.music.is_some() && self.host.provider("youtube").is_ok() {
            let who = if self.use_ch("yandexmusic") {
                "Обычную музыку по названию включает Алиса. Но если просят YouTube, свои лайки/понравившиеся или плейлист (и не упомянули Яндекс/Алису), всегда используй play_music, а не alice_command."
            } else {
                "Яндекс Музыка выключена в настройках: любую музыку, песню или радио включай только этим инструментом, сразу, запрос придумай сам, если не назвали."
            };
            defs.push(json!({"type": "function", "name": "play_music",
                "description": format!("Найти по всему YouTube и включить звук видео, плейлиста или лайков аккаунта на колонке: любой ролик, кавер, ремикс, концерт, подкаст, песню, сохранённый/публичный плейлист или понравившиеся треки. {who} Если сказали только «включи на YouTube трек», сначала спроси название или исполнителя. Подбирай сам, когда об этом попросили. Не обещай «сейчас поищу» без вызова."),
                "parameters": {"type": "object", "properties": {
                    "query": {"type": "string", "description": "что искать или включить: исполнитель и название, «мои лайки» / «понравившиеся», или «плейлист <название>»"},
                    "queue": {"type": "boolean", "description": "true — не включать сразу, а добавить в очередь после того, что играет («добавь в очередь», «потом включи»)"},
                    "keep_talking": {"type": "boolean", "description": "true — если кроме включения музыки хозяин попросил ещё о чём-то (ответить на вопрос, рассказать, не отключаться)"}},
                    "required": ["query"]}}));
        }
        if self.music_ctl.is_some() && self.host.provider("youtube").is_ok() {
            defs.push(json!({"type": "function", "name": "music_control",
                "description": "Управлять тем, что сейчас играет с YouTube: пауза, продолжить, перемотка вперёд/назад, с начала, следующее видео, похожее (радио), повтор (на репит), поставь лайк, убери лайк, выключить, что играет.",
                "parameters": {"type": "object", "properties": {
                    "command": {"type": "string", "description": "команда по-русски, например «пауза», «продолжи», «перемотай вперёд на 2 минуты», «назад на 30 секунд», «сначала», «следующее», «включи похожее», «включи повтор», «выключи повтор», «поставь лайк», «убери лайк», «выключи музыку», «что играет»"}},
                    "required": ["command"]}}));
        }
        if self.use_ch("alice") {
            let mut alice_tool = def("alice_command");
            if self.music.is_some()
                && self.host.provider("youtube").is_ok()
                && let Some(desc) = alice_tool.get("description").and_then(Value::as_str)
            {
                alice_tool["description"] = Value::String(format!(
                    "{desc} Лайки и плейлисты YouTube не передавай сюда — для них используй play_music."
                ));
            }
            defs.push(alice_tool);
        }
        if self.memory.as_ref().is_some_and(|m| m.enabled()) {
            defs.push(def("recall_conversation"));
            defs.push(def("edit_memory"));
        }
        defs.extend(self.host.tools());
        if self.has_token() && self.use_ch("yandex") && self.has_catalog().await {
            defs.push(def("smart_home"));
        }
        defs
    }

    pub async fn call(&self, name: &str, args: &str) -> String {
        let a: Value = serde_json::from_str(args).unwrap_or(Value::Null);
        let field = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let ch = match name {
            "home_assistant" => Some("homeassistant"),
            "xiaomi_home" => Some("xiaomi"),
            "smart_life" => Some("smartlife"),
            _ => None,
        };
        if let Some(ch) = ch
            && self.ready(ch)
            && (ch != "homeassistant" || self.use_ch(ch))
        {
            return match self.vendor(ch, "control", args.into(), Duration::from_secs(15)).await {
                Ok(v) => as_str(&v),
                Err(e) => e,
            };
        }
        if name == "play_music"
            && let Some(m) = &self.music
        {
            let q = field("query");
            if q.is_empty() {
                return "Не понял, что включить.".into();
            }
            if a.get("queue").and_then(Value::as_bool) == Some(true)
                && let Some(mq) = &self.music_queue
            {
                return mq(q).await;
            }
            return m(q).await;
        }
        if name == "music_control"
            && let Some(m) = &self.music_ctl
        {
            let c = field("command");
            if c.is_empty() {
                return "Не понял команду для музыки.".into();
            }
            return m(c).await;
        }
        if let Some(m) = &self.memory {
            if name == "recall_conversation" {
                let (q, d) = (field("query"), field("date"));
                if q.is_empty() && d.is_empty() {
                    return "Скажи, что вспомнить: ключевые слова или день.".into();
                }
                return m.search(&q, &d);
            }
            if name == "edit_memory" {
                let words: Vec<String> = a.get("words").and_then(Value::as_array).map(|w| w.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
                return m.edit(&field("forget"), &field("remember"), &words);
            }
        }
        if name == "alice_command" && self.use_ch("alice") {
            let cmd = field("command");
            if cmd.is_empty() {
                return "Не понял команду для Алисы.".into();
            }
            if let Some(question) = fastcmd::clarification(&cmd) {
                return question.into();
            }
            if fastcmd::youtube_request(&cmd) {
                return if self.host.provider("youtube").is_ok() {
                    "Для просьбы включить с YouTube вызови play_music.".into()
                } else {
                    "Для этой команды нужен установленный и включённый плагин YouTube.".into()
                };
            }
            let schedule = fastcmd::alice_schedule(&cmd);
            let c2 = cmd.clone();
            let is_schedule = schedule == fastcmd::AliceSchedule::Ready;
            return match tokio::task::spawn_blocking(move || crate::alice::text(&c2, !is_schedule)).await.unwrap_or_else(|e| Err(e.to_string())) {
                Err(e) => format!("Алиса недоступна: {e}"),
                Ok(()) => {
                    log::info!("инструмент: «{cmd}» → Алисе");
                    if is_schedule {
                        "Команда передана. Алиса ответит своим голосом. Не утверждай, что команда выполнена.".into()
                    } else {
                        format!("Передал Алисе: «{cmd}». Она выполняет — скажи коротко, что сделано.")
                    }
                }
            };
        }
        if name != "smart_home" {
            if let Some(res) = self.host.call_tool(name, args).await {
                log::info!("инструмент плагина {name} → {}", crate::plugins::clip(&res, 200));
                return res;
            }
            return "Нет такого инструмента.".into();
        }
        if !self.use_ch("yandex") {
            return "Управление через Яндекс Умный дом выключено.".into();
        }
        match self.yandex("control", args.into(), Duration::from_secs(15)).await {
            Ok(v) => as_str(&v),
            Err(e) => e,
        }
    }
}

const CMD_VERB_STOPWORDS: &[&str] = &[
    "джарвис",
    "пожалуйста",
    "конкретно",
    "включи",
    "выключи",
    "погаси",
    "отключи",
    "зажги",
    "запусти",
    "останови",
    "сделай",
    "поставь",
    "убавь",
    "прибавь",
    "увеличь",
    "уменьши",
    "открой",
    "закрой",
    "отправь",
    "верни",
];

fn device_matches_command(text: &str, device: &Value) -> bool {
    let norm_cmd = fastcmd::norm_text(text);
    if norm_cmd.is_empty() {
        return false;
    }
    let name_raw = device["name"]
        .as_str()
        .or_else(|| device["attributes"]["friendly_name"].as_str())
        .unwrap_or("");
    let name = fastcmd::norm_text(name_raw);
    let kind = device["type"].as_str().unwrap_or("").to_lowercase();
    let model = device["model"].as_str().unwrap_or("").to_lowercase();
    let domain = device["domain"]
        .as_str()
        .or_else(|| device["entity_id"].as_str().and_then(|id| id.split('.').next()))
        .unwrap_or("")
        .to_lowercase();
    let meta = format!("{} {} {} {}", name, kind, model, domain);

    const PAIRS: &[(&[&str], &[&str])] = &[
        (&["пылесос", "пылик", "робот"], &["vacuum"]),
        (&["свет", "ламп", "люстр", "лент", "торшер", "подсвет"], &["light", "lamp", "bulb", "ceiling", "strip"]),
        (&["розетк", "выключат"], &["outlet", "plug", "switch", "relay"]),
        (&["кондиц", "кондей", "кондер"], &["air-conditioner", "air_conditioner", "climate"]),
        (&["увлажнит"], &["humidifier"]),
        (&["очистит"], &["air-purifier", "purifier"]),
        (&["чайник"], &["kettle"]),
        (&["штор", "занавес"], &["curtain", "cover", "blind"]),
        (&["обогрев"], &["heater"]),
        (&["вентилятор"], &["fan"]),
        (&["телевизор"], &["television", "media_player"]),
    ];

    for (cmd_stems, dev_tokens) in PAIRS {
        if cmd_stems.iter().any(|stem| norm_cmd.contains(stem))
            && dev_tokens.iter().any(|tok| meta.contains(tok))
        {
            return true;
        }
    }

    if !name.is_empty() {
        let name_words: Vec<&str> = name.split_whitespace().collect();
        for cw in norm_cmd.split_whitespace() {
            if cw.chars().count() < 4 || CMD_VERB_STOPWORDS.contains(&cw) {
                continue;
            }
            if name_words.iter().any(|nw| {
                nw.chars().count() >= 4
                    && (*nw == cw || nw.starts_with(cw) || cw.starts_with(nw))
            }) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defs_load() {
        for n in ["home_assistant", "xiaomi_home", "smart_life", "alice_command", "smart_home", "recall_conversation", "edit_memory"] {
            assert_eq!(super::def(n)["name"], n, "{n}");
        }
    }

    #[test]
    fn device_matches_command_routes_xiaomi_and_ha_away_from_alice() {
        let vacuum = json!({
            "name": "Mijia Robot Vacuum 5",
            "room": "",
            "type": "urn:miot-spec-v2:device:vacuum:0000A006:xiaomi-ov31:1",
            "model": "xiaomi.vacuum.ov31"
        });
        assert!(device_matches_command("включи пылесос в спальне конкретно в спальне", &vacuum));
        assert!(!device_matches_command("выключи свет на кухне", &vacuum));

        let ha_light = json!({
            "entity_id": "light.kitchen_ceiling",
            "attributes": {"friendly_name": "Люстра кухня"}
        });
        assert!(device_matches_command("выключи свет на кухне", &ha_light));
        assert!(!device_matches_command("включи кондиционер", &ha_light));
    }
}
