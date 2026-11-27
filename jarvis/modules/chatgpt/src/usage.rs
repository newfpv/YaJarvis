//! Codex quota is separate from the subscription voice allowance.
use super::{ChatGpt, Value, account_error, json, now};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Default)]
pub(super) struct Cache(Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Entry>>>>);
#[derive(Default)]
struct Entry {
    revision: Value,
    attempted: i64,
    data: Value,
}
impl Cache {
    fn entry(&self, id: &str) -> Arc<tokio::sync::Mutex<Entry>> {
        let mut cache = self.0.lock().unwrap();
        // Removed accounts cannot accumulate an unbounded cache.
        if cache.len() >= 16 && !cache.contains_key(id) {
            cache.clear();
        }
        cache.entry(id.into()).or_default().clone()
    }
}
fn window(v: &Value, at: i64) -> Option<(String, Value)> {
    let label = match v["limit_window_seconds"].as_i64()? {
        18000 => "five_hour",
        604800 => "weekly",
        _ => return None,
    };
    let used = v["used_percent"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)?;
    let reset = v["reset_at"]
        .as_i64()
        .filter(|n| *n > 0 && *n < 253402300800)
        .or_else(|| {
            v["reset_after_seconds"]
                .as_i64()
                .filter(|n| *n >= 0 && *n <= 604800 * 2)
                .map(|n| at + n)
        });
    Some((
        label.into(),
        json!({"used_percent":used.min(100.0),"remaining_percent":(100.0-used).max(0.0),"reset_at":reset}),
    ))
}
fn parse(p: &Value, at: i64) -> Result<Value, String> {
    let rate = p
        .get("rate_limit")
        .or_else(|| p.get("rate_limits"))
        .filter(|v| v.is_object())
        .ok_or("OpenAI не передал данные лимитов Codex")?;
    let mut windows = serde_json::Map::new();
    for key in ["primary_window", "secondary_window"] {
        if let Some((key, data)) = window(&rate[key], at) {
            windows.insert(key, data);
        }
    }
    if windows.is_empty() {
        return Err("OpenAI не передал пятичасовой и недельный лимиты Codex".into());
    }
    Ok(json!({"source":"codex","fetched_at":at,"windows":windows,"error":Value::Null}))
}
fn public_error(e: String) -> String {
    account_error::Failure::parse(&e)
        .map(|f| f.message())
        .unwrap_or(e)
}
impl ChatGpt {
    pub(super) async fn account_usage(&self, p: &Value) -> Result<Value, String> {
        let id = p["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Нужен ID аккаунта")?;
        let slot = self.usage_cache.entry(id);
        let mut entry = slot.lock().await;
        let auth = (self.services)("auth.get".into(), json!({"id":id})).await?;
        if entry.revision == auth["_revision"]
            && now() - entry.attempted < 60
            && !entry.data.is_null()
        {
            return Ok(entry.data.clone());
        }
        if entry.revision != auth["_revision"] {
            entry.data = Value::Null;
        }
        entry.revision = auth["_revision"].clone();
        let result = async {
            // This fetch never selects an account or opens/closes a voice session.
            let mut auth = self.token_for(Some(id), 300).await.map_err(public_error)?;
            for attempt in 0..2 {
                let response = self
                    .client()
                    .await?
                    .get(&self.usage_url)
                    .timeout(Duration::from_secs(10))
                    .bearer_auth(auth["access"].as_str().ok_or("Нет токена аккаунта")?)
                    .header(
                        "ChatGPT-Account-Id",
                        auth["account"].as_str().ok_or("Нет ID аккаунта")?,
                    )
                    .header("Accept", "application/json")
                    .header("User-Agent", "codex-cli")
                    .send()
                    .await
                    .map_err(|_| {
                        "Не удалось связаться с OpenAI для проверки лимитов".to_string()
                    })?;
                if response.status().as_u16() == 401 && attempt == 0 {
                    auth = self
                        .token_for(Some(id), i64::MAX / 2)
                        .await
                        .map_err(public_error)?;
                    continue;
                }
                if !response.status().is_success() {
                    return Err(format!(
                        "Не удалось получить лимиты Codex: HTTP {}",
                        response.status().as_u16()
                    ));
                }
                let data = super::json_response(response).await?;
                let current = (self.services)("auth.get".into(), json!({"id":id})).await?;
                if current["_revision"] != auth["_revision"] {
                    return Err("Вход изменился во время проверки; обновите лимиты".into());
                }
                return parse(&data, now());
            }
            Err("Не удалось проверить вход в аккаунт".into())
        }
        .await;
        entry.attempted = now();
        match result {
            Ok(data) => entry.data = data,
            Err(error) => {
                if entry.data.is_null() {
                    entry.data = json!({"source":"codex","fetched_at":Value::Null,"windows":{}});
                }
                entry.data["error"] = error.into();
            }
        }
        entry.data["checked_at"] = entry.attempted.into();
        Ok(entry.data.clone())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifies_windows_by_duration_and_never_invents_missing_data() {
        let p = json!({"rate_limit":{"primary_window":{"used_percent":14,"limit_window_seconds":604800,"reset_after_seconds":100}}});
        let v = parse(&p, 2000).unwrap();
        assert!(v["windows"]["five_hour"].is_null());
        assert_eq!(v["windows"]["weekly"]["remaining_percent"], 86.0);
        assert_eq!(v["windows"]["weekly"]["reset_at"], 2100);
        for used in [Value::Null, json!("91"), json!(-1)] {
            assert!(parse(&json!({"rate_limit":{"primary_window":{"used_percent":used,"limit_window_seconds":18000}}}),2000).is_err());
        }
        assert!(parse(&json!({"rate_limit":{"primary_window":null}}), 2000).is_err());
    }
    #[test]
    fn public_result_contains_no_identity_or_unrelated_model_limits() {
        let v=parse(&json!({"account_id":"private","access_token":"secret","rate_limit":{"primary_window":{"used_percent":91,"limit_window_seconds":18000,"reset_at":1795379325},"secondary_window":{"used_percent":100,"limit_window_seconds":604800}},"additional_rate_limits":[{"rate_limit":{"primary_window":{"used_percent":0}}}]}),2000).unwrap();
        assert_eq!(v["windows"]["five_hour"]["remaining_percent"], 9.0);
        assert_eq!(v["windows"]["weekly"]["remaining_percent"], 0.0);
        assert!(!v.to_string().contains("secret"));
        assert!(!v.to_string().contains("private"));
    }
}
