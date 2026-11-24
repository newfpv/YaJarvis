//! Пул ключей и квот Gemini: учёт минутных, дневных и постоянных отказов по паре (ключ, модель),
//! автопереключение ключей и отмеченных пользователем моделей, сохранение на диск без хранения самих ключей.

use crate::{
    settings::{Profile, write_atomic},
    voice_error::{FailureKind, Stage, VoiceFailure, format_local_hhmm},
};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub const DEFAULT_LIVE_MODEL: &str = "gemini-3.8-live";
const SUSPECTED_COOLDOWN_S: i64 = 90;
const SUSPECTED_ESCALATION_WINDOW_S: i64 = 15 * 60;
const MODEL_UNAVAILABLE_COOLDOWN_S: i64 = 10 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Minute,
    Daily,
    Suspected,
    KeyInvalid,
    QuotaZero,
    ModelUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cooldown {
    pub kind: LimitKind,
    /// Unix-время разблокировки в секундах; 0 — до ручного сброса (неверный ключ / нулевая квота).
    pub until: i64,
    pub why: String,
    #[serde(default)]
    pub hits: u32,
    #[serde(default)]
    pub updated_at: i64,
}

impl Cooldown {
    pub fn active_at(&self, now: i64) -> bool {
        self.until == 0 || now < self.until
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_error: Option<Cooldown>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, Cooldown>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoreData {
    keys: BTreeMap<String, KeyQuota>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub key_index: usize,
    pub key: String,
    pub tail: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitEvent {
    pub kind: LimitKind,
    pub until: i64,
    pub summary: String,
}

pub struct GeminiPool {
    path: PathBuf,
    data: Mutex<StoreData>,
}

pub fn now_unix() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn key_id(key: &str) -> String {
    let hash = Sha256::digest(key.as_bytes());
    hex::encode(&hash[..8])
}

pub fn key_tail(key: &str) -> String {
    let n = key.chars().count();
    if n <= 4 {
        key.to_string()
    } else {
        format!("…{}", key.chars().skip(n - 4).collect::<String>())
    }
}

/// Дневные квоты Google AI Studio сбрасываются в полночь по тихоокеанскому времени (America/Los_Angeles).
pub fn next_pt_midnight(now: i64) -> i64 {
    let ts = jiff::Timestamp::from_second(now).unwrap_or_else(|_| jiff::Timestamp::now());
    let pt = TimeZone::get("America/Los_Angeles")
        .unwrap_or_else(|_| TimeZone::fixed(jiff::tz::offset(-8)));
    let zoned = ts.to_zoned(pt.clone());
    let next_day = zoned
        .date()
        .tomorrow()
        .ok()
        .and_then(|d| d.at(0, 2, 0, 0).to_zoned(pt).ok())
        .map(|z| z.timestamp().as_second());
    next_day.unwrap_or(now + 24 * 3600)
}

pub fn format_remaining(until: i64, now: i64, tz: &TimeZone) -> String {
    if until <= 0 {
        return "до исправления".into();
    }
    let rem = (until - now).max(1);
    if rem < 90 {
        format!("~{rem} с")
    } else if rem < 3600 {
        format!("~{} мин", (rem + 59) / 60)
    } else if let Some(hhmm) = format_local_hhmm(until, tz) {
        format!("до {hhmm}")
    } else {
        format!("~{} ч", (rem + 3599) / 3600)
    }
}

/// Список моделей Gemini, по которым разрешено переключение для данного профиля.
/// Переключаемся ТОЛЬКО между моделями, отмеченными галочками (`profile.gemini_models`).
pub fn selected_live_models(profile: &Profile) -> Vec<String> {
    let mut out = Vec::new();
    for m in &profile.gemini_models {
        let m = m.trim();
        if !m.is_empty() && !out.iter().any(|x| x == m) {
            out.push(m.to_string());
        }
    }
    if out.is_empty() {
        let single = if profile.live_model.trim().is_empty() {
            DEFAULT_LIVE_MODEL
        } else {
            profile.live_model.trim()
        };
        out.push(single.to_string());
    }
    out
}

/// Ключи Gemini из настроек (если `gemini_keys` пуст, используем одиночный `gemini_key`).
pub fn effective_keys(keys: &[String], active_key: &str) -> Vec<String> {
    let mut out: Vec<String> = keys
        .iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .map(String::from)
        .collect();
    let active = active_key.trim();
    if !active.is_empty() && !out.iter().any(|k| k == active) {
        out.insert(0, active.to_string());
    }
    out
}

impl GeminiPool {
    pub fn load(path: PathBuf) -> Arc<Self> {
        let data = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<StoreData>(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            path,
            data: Mutex::new(data),
        })
    }

    fn save_locked(&self, data: &StoreData) {
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(data) {
            let _ = write_atomic(&self.path, &bytes, 0o600);
        }
    }

    fn active_cooldown<'a>(kq: &'a KeyQuota, model: &str, now: i64) -> Option<&'a Cooldown> {
        if let Some(err) = kq.key_error.as_ref().filter(|c| c.active_at(now)) {
            return Some(err);
        }
        kq.models.get(model).filter(|c| c.active_at(now))
    }

    /// Порядок попыток: сначала все свободные ключи на 1-й отмеченной модели (начиная с активного),
    /// затем (если отмечено несколько моделей) все свободные ключи на 2-й модели и т.д.
    pub fn candidates(
        &self,
        keys: &[String],
        active_key: &str,
        models: &[String],
        now: i64,
    ) -> Vec<Candidate> {
        let all_keys = effective_keys(keys, active_key);
        if all_keys.is_empty() || models.is_empty() {
            return vec![];
        }
        let start = all_keys
            .iter()
            .position(|k| k == active_key.trim())
            .unwrap_or(0);
        let ordered_keys: Vec<(usize, &String)> = (0..all_keys.len())
            .map(|offset| {
                let idx = (start + offset) % all_keys.len();
                (idx, &all_keys[idx])
            })
            .collect();

        let data = self.data.lock().unwrap();
        let mut out = Vec::new();
        for model in models {
            for &(idx, key) in &ordered_keys {
                let id = key_id(key);
                let blocked = data
                    .keys
                    .get(&id)
                    .and_then(|kq| Self::active_cooldown(kq, model, now))
                    .is_some();
                if !blocked {
                    out.push(Candidate {
                        key_index: idx,
                        key: key.clone(),
                        tail: key_tail(key),
                        model: model.clone(),
                    });
                }
            }
        }
        out
    }

    /// Успешный ответ на (ключ, модель): снимаем накопленные подозрения и истёкшие блокировки.
    pub fn record_success(&self, key: &str, model: &str) {
        let id = key_id(key);
        let mut data = self.data.lock().unwrap();
        let Some(kq) = data.keys.get_mut(&id) else {
            return;
        };
        let mut changed = false;
        if kq.key_error.is_some() {
            kq.key_error = None;
            changed = true;
        }
        if kq.models.remove(model).is_some() {
            changed = true;
        }
        if kq.key_error.is_none() && kq.models.is_empty() {
            data.keys.remove(&id);
        }
        if changed {
            self.save_locked(&data);
        }
    }

    /// Зафиксировать отказ ключа/модели. Возвращает `Some(LimitEvent)` если ключ/модель отправлены в паузу.
    pub fn record_failure(
        &self,
        key: &str,
        key_index: usize,
        model: &str,
        failure: &VoiceFailure,
        now: i64,
        tz: &TimeZone,
    ) -> Option<LimitEvent> {
        let id = key_id(key);
        let tail = key_tail(key);
        let mut data = self.data.lock().unwrap();
        let kq = data.keys.entry(id).or_default();

        let (key_wide, cooldown, summary) = match failure.kind {
            FailureKind::AuthInvalid => {
                let cd = Cooldown {
                    kind: LimitKind::KeyInvalid,
                    until: 0,
                    why: "ключ отклонён (неверный или отозван)".into(),
                    hits: 1,
                    updated_at: now,
                };
                let msg = format!(
                    "ключ #{} ({tail}) отклонён (ошибка ключа): {}",
                    key_index + 1,
                    failure.detail
                );
                (true, cd, msg)
            }
            FailureKind::AccessDenied => {
                let cd = Cooldown {
                    kind: LimitKind::KeyInvalid,
                    until: 0,
                    why: "доступ запрещён (регион или права ключа)".into(),
                    hits: 1,
                    updated_at: now,
                };
                let msg = format!(
                    "ключ #{} ({tail}) получил отказ в доступе (403): {}",
                    key_index + 1,
                    failure.detail
                );
                (true, cd, msg)
            }
            FailureKind::BillingRequired => {
                let cd = Cooldown {
                    kind: LimitKind::QuotaZero,
                    until: 0,
                    why: "квота 0 или требуется биллинг".into(),
                    hits: 1,
                    updated_at: now,
                };
                let msg = format!(
                    "ключ #{} ({tail}) на {model}: квота 0 / нужен биллинг ({})",
                    key_index + 1,
                    failure.detail
                );
                (false, cd, msg)
            }
            FailureKind::QuotaMinute => {
                let wait = failure.retry_after_s.unwrap_or(65).clamp(15, 600) as i64;
                let until = now + wait;
                let cd = Cooldown {
                    kind: LimitKind::Minute,
                    until,
                    why: format!("минутный лимит ({wait} с)"),
                    hits: 1,
                    updated_at: now,
                };
                let msg = format!(
                    "ключ #{} ({tail}) на {model} упёрся в минутный лимит (пауза ~{wait} с)",
                    key_index + 1
                );
                (false, cd, msg)
            }
            FailureKind::QuotaDaily => {
                let until = next_pt_midnight(now);
                let rem = format_remaining(until, now, tz);
                let cd = Cooldown {
                    kind: LimitKind::Daily,
                    until,
                    why: format!("дневной лимит ({rem})"),
                    hits: 1,
                    updated_at: now,
                };
                let msg = format!(
                    "ключ #{} ({tail}) на {model} исчерпал дневной лимит ({rem})",
                    key_index + 1
                );
                (false, cd, msg)
            }
            FailureKind::QuotaUnknown => {
                let prev = kq.models.get(model);
                let escalate_to_daily = prev.is_some_and(|p| {
                    p.kind == LimitKind::Suspected
                        && p.hits >= 1
                        && now.saturating_sub(p.updated_at) <= SUSPECTED_ESCALATION_WINDOW_S
                });
                let raw_note = if failure.detail.is_empty() {
                    String::new()
                } else {
                    format!(" [ответ сервера: {}]", failure.detail)
                };
                if escalate_to_daily {
                    let until = next_pt_midnight(now);
                    let rem = format_remaining(until, now, tz);
                    let cd = Cooldown {
                        kind: LimitKind::Daily,
                        until,
                        why: format!("повторный отказ квоты, похоже дневной лимит ({rem})"),
                        hits: prev.map_or(2, |p| p.hits.saturating_add(1)),
                        updated_at: now,
                    };
                    let msg = format!(
                        "ключ #{} ({tail}) на {model} повторно упёрся в квоту — перевожу в дневной лимит ({rem}){raw_note}",
                        key_index + 1
                    );
                    (false, cd, msg)
                } else {
                    let wait = failure
                        .retry_after_s
                        .map(|s| s.clamp(30, 300) as i64)
                        .unwrap_or(SUSPECTED_COOLDOWN_S);
                    let until = now + wait;
                    let cd = Cooldown {
                        kind: LimitKind::Suspected,
                        until,
                        why: format!("лимит квоты (проверим через ~{wait} с)"),
                        hits: 1,
                        updated_at: now,
                    };
                    let msg = format!(
                        "ключ #{} ({tail}) на {model} упёрся в лимит квоты (пауза ~{wait} с; при повторе пометим как дневной){raw_note}",
                        key_index + 1
                    );
                    (false, cd, msg)
                }
            }
            FailureKind::ModelUnavailable => {
                let until = now + MODEL_UNAVAILABLE_COOLDOWN_S;
                let cd = Cooldown {
                    kind: LimitKind::ModelUnavailable,
                    until,
                    why: "модель недоступна для этого ключа".into(),
                    hits: 1,
                    updated_at: now,
                };
                let msg = format!(
                    "ключ #{} ({tail}): модель {model} недоступна (пауза 10 мин)",
                    key_index + 1
                );
                (false, cd, msg)
            }
            // Сетевые/VPN/потоковые сбои не должны портить квоту ключа!
            FailureKind::MissingConfig
            | FailureKind::VpnBlocked
            | FailureKind::NetworkError
            | FailureKind::StreamDropped
            | FailureKind::AudioError => return None,
        };

        let event = LimitEvent {
            kind: cooldown.kind,
            until: cooldown.until,
            summary,
        };
        if key_wide {
            kq.key_error = Some(cooldown);
        } else {
            kq.models.insert(model.to_string(), cooldown);
        }
        self.save_locked(&data);
        Some(event)
    }

    pub fn reset_key(&self, key: &str) {
        let id = key_id(key);
        let mut data = self.data.lock().unwrap();
        if data.keys.remove(&id).is_some() {
            self.save_locked(&data);
        }
    }

    pub fn clear_all(&self) {
        let mut data = self.data.lock().unwrap();
        if !data.keys.is_empty() {
            data.keys.clear();
            self.save_locked(&data);
        }
    }

    /// Финальный диагноз, когда ни одной свободной пары (ключ, модель) не осталось.
    pub fn all_exhausted_failure(
        &self,
        keys: &[String],
        models: &[String],
        now: i64,
        tz: &TimeZone,
    ) -> VoiceFailure {
        let data = self.data.lock().unwrap();
        let mut earliest_timed: Option<(usize, String, String, Cooldown)> = None;
        let mut any_invalid = false;
        let mut any_zero = false;
        let mut any_model_unavail = false;

        for (idx, key) in keys.iter().enumerate() {
            let id = key_id(key);
            let Some(kq) = data.keys.get(&id) else { continue };
            for model in models {
                let Some(cd) = Self::active_cooldown(kq, model, now) else { continue };
                match cd.kind {
                    LimitKind::KeyInvalid => any_invalid = true,
                    LimitKind::QuotaZero => any_zero = true,
                    LimitKind::ModelUnavailable => any_model_unavail = true,
                    LimitKind::Minute | LimitKind::Suspected | LimitKind::Daily => {
                        if earliest_timed
                            .as_ref()
                            .is_none_or(|(_, _, _, best)| cd.until < best.until)
                        {
                            earliest_timed = Some((idx, key_tail(key), model.clone(), cd.clone()));
                        }
                    }
                }
            }
        }

        let key_count = keys.len();
        let has_other_models = models.len() < 4;

        if let Some((idx, tail, model, cd)) = earliest_timed {
            let rem_s = (cd.until - now).max(1) as u64;
            let rem_label = format_remaining(cd.until, now, tz);
            let (kind, detail, action) = match cd.kind {
                LimitKind::Minute => (
                    FailureKind::QuotaMinute,
                    format!(
                        "Все ключи Gemini ({key_count}) в минутном лимите. Ближайший: ключ #{} ({tail}, {model}) освободится через {rem_label}",
                        idx + 1
                    ),
                    format!(
                        "Подождите {rem_label} или добавьте ключ с другого Google-аккаунта."
                    ),
                ),
                LimitKind::Suspected => (
                    FailureKind::QuotaUnknown,
                    format!(
                        "Все ключи Gemini ({key_count}) упёрлись в лимит квоты. Перепроверим ключ #{} ({tail}, {model}) через {rem_label}",
                        idx + 1
                    ),
                    format!(
                        "Подождите {rem_label}, отметьте запасную модель в профиле или добавьте ключ с другого Google-аккаунта."
                    ),
                ),
                _ => (
                    FailureKind::QuotaDaily,
                    format!(
                        "Дневной лимит Gemini исчерпан на всех ключах ({key_count}). Ближайший сброс: {rem_label} (ключ #{}, {tail}, {model})",
                        idx + 1
                    ),
                    format!(
                        "Дождитесь сброса ({rem_label}), отметьте запасную модель в профиле или добавьте ключ с другого Google-аккаунта."
                    ),
                ),
            };
            return VoiceFailure {
                provider: "gemini".into(),
                stage: Stage::Connect,
                kind,
                retry_after_s: Some(rem_s),
                reset_at: Some(cd.until),
                key_count,
                models: models.to_vec(),
                has_other_models,
                detail,
                action,
                retryable: true,
                expected: false,
            };
        }

        if any_zero {
            return VoiceFailure {
                provider: "gemini".into(),
                stage: Stage::Auth,
                kind: FailureKind::BillingRequired,
                retry_after_s: None,
                reset_at: None,
                key_count,
                models: models.to_vec(),
                has_other_models,
                detail: format!("На всех ключах Gemini ({key_count}) нулевая квота (limit: 0) или требуется биллинг"),
                action: "Проверьте биллинг в Google AI Studio или добавьте другой ключ.".into(),
                retryable: false,
                expected: false,
            };
        }
        if any_invalid {
            return VoiceFailure {
                provider: "gemini".into(),
                stage: Stage::Auth,
                kind: FailureKind::AuthInvalid,
                retry_after_s: None,
                reset_at: None,
                key_count,
                models: models.to_vec(),
                has_other_models,
                detail: format!("Все ключи Gemini ({key_count}) отклонены (неверный ключ или запрещён доступ)"),
                action: "Проверьте ключи Gemini в «Подключениях».".into(),
                retryable: false,
                expected: false,
            };
        }
        if any_model_unavail {
            return VoiceFailure {
                provider: "gemini".into(),
                stage: Stage::Connect,
                kind: FailureKind::ModelUnavailable,
                retry_after_s: None,
                reset_at: None,
                key_count,
                models: models.to_vec(),
                has_other_models,
                detail: "Отмеченные модели Gemini сейчас недоступны".into(),
                action: "Выберите другую модель Gemini в настройках ассистента.".into(),
                retryable: false,
                expected: false,
            };
        }

        VoiceFailure {
            provider: "gemini".into(),
            stage: Stage::Connect,
            kind: FailureKind::QuotaUnknown,
            retry_after_s: None,
            reset_at: None,
            key_count,
            models: models.to_vec(),
            has_other_models,
            detail: "Нет доступных ключей Gemini".into(),
            action: "Проверьте ключи в «Подключениях».".into(),
            retryable: true,
            expected: false,
        }
    }

    /// Сводка по пулу ключей и каждой модели Gemini (сколько ключей свободно на каждой модели).
    pub fn pool_summary(
        &self,
        keys: &[String],
        active_key: &str,
        checked_models: &[String],
        catalog_models: &[(String, String)],
        tz: &TimeZone,
    ) -> Value {
        let now = now_unix();
        let data = self.data.lock().unwrap();
        let total_keys = keys.len();
        let active_index = keys.iter().position(|k| k == active_key.trim()).unwrap_or(0);
        let active_tail = keys.get(active_index).map(|k| key_tail(k)).unwrap_or_default();
        let mut any_limited = false;
        let list: Vec<(String, String)> = if !catalog_models.is_empty() {
            catalog_models.to_vec()
        } else {
            checked_models.iter().map(|m| (m.clone(), m.clone())).collect()
        };

        let models: Vec<Value> = list
            .iter()
            .map(|(mid, mname)| {
                let checked = checked_models.iter().any(|m| m == mid);
                let mut free_keys = 0usize;
                let mut earliest: Option<&Cooldown> = None;
                let mut any_invalid = false;
                for key in keys {
                    let id = key_id(key);
                    let cd = data.keys.get(&id).and_then(|q| Self::active_cooldown(q, mid, now));
                    if let Some(cd) = cd {
                        any_limited = true;
                        if matches!(cd.kind, LimitKind::KeyInvalid | LimitKind::QuotaZero) {
                            any_invalid = true;
                        } else if cd.until > 0 && earliest.is_none_or(|best| cd.until < best.until) {
                            earliest = Some(cd);
                        }
                    } else {
                        free_keys += 1;
                    }
                }
                let (status, label, next_until) = if total_keys == 0 {
                    ("none", "нет ключей".into(), 0)
                } else if free_keys == total_keys {
                    ("ready", format!("{free_keys} из {total_keys} свободно"), 0)
                } else if free_keys > 0 {
                    ("partial", format!("{free_keys} из {total_keys} свободно"), earliest.map_or(0, |c| c.until))
                } else if let Some(cd) = earliest {
                    let rem = format_remaining(cd.until, now, tz);
                    let (st, kind_lbl) = match cd.kind {
                        LimitKind::Minute => ("minute", "минутный"),
                        LimitKind::Suspected => ("suspected", "лимит"),
                        _ => ("daily", "дневной"),
                    };
                    (st, format!("0 из {total_keys} · {kind_lbl} {rem}"), cd.until)
                } else if any_invalid {
                    ("invalid", format!("0 из {total_keys} · ошибка ключа/биллинга"), 0)
                } else {
                    ("daily", format!("0 из {total_keys} · лимит"), 0)
                };
                json!({
                    "id": mid,
                    "name": mname,
                    "checked": checked,
                    "free_keys": free_keys,
                    "total_keys": total_keys,
                    "status": status,
                    "label": label,
                    "next_until": next_until,
                })
            })
            .collect();

        json!({
            "total_keys": total_keys,
            "active_index": active_index,
            "active_tail": active_tail,
            "checked_models": checked_models,
            "any_limited": any_limited,
            "models": models,
        })
    }

    /// Снимок состояния всех ключей Gemini для `/api/state`.
    pub fn snapshot(
        &self,
        keys: &[String],
        active_key: &str,
        checked_models: &[String],
        catalog_models: &[(String, String)],
        tz: &TimeZone,
    ) -> Vec<Value> {
        let now = now_unix();
        let data = self.data.lock().unwrap();
        let total_models = catalog_models.len().max(checked_models.len()).max(1);

        keys.iter()
            .map(|key| {
                let id = key_id(key);
                let tail = key_tail(key);
                let active = key == active_key.trim();
                let kq = data.keys.get(&id);

                if let Some(err) = kq.and_then(|q| q.key_error.as_ref()).filter(|c| c.active_at(now)) {
                    let status = if err.kind == LimitKind::QuotaZero { "quota_zero" } else { "invalid" };
                    return json!({
                        "tail": tail,
                        "active": active,
                        "status": status,
                        "label": err.why,
                        "detail": "Нажмите «Сбросить», если ключ исправлен, или замените его.",
                        "until": err.until,
                        "spent": total_models,
                        "models": total_models,
                        "model_states": [],
                    });
                }

                let mut model_states = Vec::new();
                let mut spent_all = 0usize;
                let mut checked_blocked = 0usize;
                let mut earliest_checked: Option<&Cooldown> = None;

                let list: Vec<(String, String)> = if !catalog_models.is_empty() {
                    catalog_models.to_vec()
                } else {
                    checked_models.iter().map(|m| (m.clone(), m.clone())).collect()
                };

                for (mid, mname) in &list {
                    let is_checked = checked_models.iter().any(|m| m == mid);
                    let cd = kq.and_then(|q| q.models.get(mid)).filter(|c| c.active_at(now));
                    if let Some(cd) = cd {
                        spent_all += 1;
                        if is_checked {
                            checked_blocked += 1;
                            if cd.until > 0
                                && earliest_checked.is_none_or(|best| cd.until < best.until)
                            {
                                earliest_checked = Some(cd);
                            }
                        }
                        let rem = format_remaining(cd.until, now, tz);
                        let (st, lbl) = match cd.kind {
                            LimitKind::Minute => ("minute", format!("минутный · {rem}")),
                            LimitKind::Suspected => ("suspected", format!("лимит · {rem}")),
                            LimitKind::Daily => ("daily", format!("дневной · {rem}")),
                            LimitKind::QuotaZero => ("quota_zero", "квота 0 / биллинг".into()),
                            LimitKind::ModelUnavailable => ("invalid", format!("недоступна · {rem}")),
                            LimitKind::KeyInvalid => ("invalid", "ошибка ключа".into()),
                        };
                        model_states.push(json!({
                            "id": mid,
                            "name": mname,
                            "checked": is_checked,
                            "status": st,
                            "label": lbl,
                            "until": cd.until,
                        }));
                    } else {
                        model_states.push(json!({
                            "id": mid,
                            "name": mname,
                            "checked": is_checked,
                            "status": "ready",
                            "label": "готов",
                            "until": 0,
                        }));
                    }
                }

                let checked_count = checked_models.len().max(1);
                let (status, label, until, detail) = if spent_all == 0 {
                    (
                        "ready",
                        if active { "активен · готов".into() } else { "в запасе · готов".into() },
                        0i64,
                        String::new(),
                    )
                } else if checked_blocked >= checked_count {
                    if let Some(cd) = earliest_checked {
                        let rem = format_remaining(cd.until, now, tz);
                        let (st, lbl, hint) = match cd.kind {
                            LimitKind::Minute => (
                                "minute",
                                format!("минутный лимит · {rem}"),
                                "Временная пауза по частоте запросов, скоро вернётся сам.",
                            ),
                            LimitKind::Suspected => (
                                "suspected",
                                format!("лимит · проверим {rem}"),
                                "Сервер не уточнил тип лимита; если повторится, пометим как дневной.",
                            ),
                            _ => (
                                "daily",
                                format!("дневной лимит · {rem}"),
                                "Бесплатная дневная квота обновится утром (полночь по Pacific Time).",
                            ),
                        };
                        (st, lbl, cd.until, hint.into())
                    } else {
                        (
                            "quota_zero",
                            "квота 0 / недоступен".into(),
                            0,
                            "Проверьте биллинг или модель для этого ключа.".into(),
                        )
                    }
                } else {
                    (
                        "partial",
                        format!(
                            "{} · лимит у {spent_all} из {total_models} моделей",
                            if active { "активен" } else { "в запасе" }
                        ),
                        earliest_checked.map_or(0, |c| c.until),
                        "Остальные отмеченные модели на этом ключе ещё доступны.".into(),
                    )
                };

                json!({
                    "tail": tail,
                    "active": active,
                    "status": status,
                    "label": label,
                    "detail": detail,
                    "until": until,
                    "spent": spent_all,
                    "models": total_models,
                    "model_states": model_states,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_error::classify;

    #[test]
    fn rotates_keys_first_then_checked_models_and_escalates_suspected_limit() {
        let dir = std::env::temp_dir().join(format!("jarvis-gpool-{}", crate::settings::rand_hex(6)));
        let pool = GeminiPool::load(dir.join("gemini-quota.json"));
        let tz = TimeZone::get("Europe/Moscow").unwrap();
        let keys = vec!["AIzaKeyOne1111".into(), "AIzaKeyTwo2222".into()];
        let models = vec!["gemini-3.8-live".into(), "gemini-3.1-flash-live-preview".into()];
        let now = 1_795_440_000i64;

        // Исходный порядок: сначала оба ключа на первой модели, потом на второй.
        let cands = pool.candidates(&keys, &keys[0], &models, now);
        assert_eq!(cands.len(), 4);
        assert_eq!((cands[0].key_index, cands[0].model.as_str()), (0, "gemini-3.8-live"));
        assert_eq!((cands[1].key_index, cands[1].model.as_str()), (1, "gemini-3.8-live"));
        assert_eq!((cands[2].key_index, cands[2].model.as_str()), (0, "gemini-3.1-flash-live-preview"));

        // Ключ 1 ловит минутный лимит на 3.8-live -> сразу переходим на ключ 2 на 3.8-live.
        let rpm = classify("gemini", Stage::Connect, "gemini ответил 429 [retry=45s, quota=PerMinute]", None, false);
        let ev = pool.record_failure(&keys[0], 0, "gemini-3.8-live", &rpm, now, &tz).unwrap();
        assert_eq!(ev.kind, LimitKind::Minute);
        let cands = pool.candidates(&keys, &keys[0], &models, now + 10);
        assert_eq!((cands[0].key_index, cands[0].model.as_str()), (1, "gemini-3.8-live"));

        // Ключ 2 ловит непонятный WS 1011 (1-й раз) -> Suspected на 90с.
        let ws_quota = classify("gemini", Stage::Connect, "You exceeded your current quota", Some(1011), false);
        let ev1 = pool.record_failure(&keys[1], 1, "gemini-3.8-live", &ws_quota, now + 10, &tz).unwrap();
        assert_eq!(ev1.kind, LimitKind::Suspected);

        // Теперь на 3.8-live оба ключа заняты -> переключаемся на отмеченную 3.1-flash!
        let cands = pool.candidates(&keys, &keys[0], &models, now + 20);
        assert_eq!((cands[0].key_index, cands[0].model.as_str()), (0, "gemini-3.1-flash-live-preview"));

        // После окончания 90с паузы ключ 2 снова ловит тот же отказ -> эскалация в Daily!
        let ev2 = pool.record_failure(&keys[1], 1, "gemini-3.8-live", &ws_quota, now + 120, &tz).unwrap();
        assert_eq!(ev2.kind, LimitKind::Daily);
        assert!(ev2.until > now + 3600);

        // Состояние сохраняется на диск без самих ключей.
        let raw = fs::read_to_string(dir.join("gemini-quota.json")).unwrap();
        assert!(!raw.contains("AIzaKeyOne1111"));
        assert!(!raw.contains("AIzaKeyTwo2222"));

        let reloaded = GeminiPool::load(dir.join("gemini-quota.json"));
        let cands_single = reloaded.candidates(&keys, &keys[1], &["gemini-3.8-live".into()], now + 130);
        assert_eq!(cands_single.len(), 1);
        assert_eq!(cands_single[0].key_index, 0); // у ключа 0 минутный лимит уже прошёл!

        // Ручной сброс возвращает ключ в работу.
        reloaded.reset_key(&keys[1]);
        let cands_after_reset = reloaded.candidates(&keys, &keys[1], &["gemini-3.8-live".into()], now + 130);
        assert_eq!(cands_after_reset.len(), 2);
        assert_eq!(cands_after_reset[0].key_index, 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn single_model_profile_does_not_switch_models_unless_checked() {
        let mut p = Profile {
            engine: "gemini".into(),
            live_model: "gemini-3.8-live".into(),
            ..Default::default()
        };
        assert_eq!(selected_live_models(&p), vec!["gemini-3.8-live"]);
        p.gemini_models = vec!["gemini-3.8-live".into(), "gemini-3.1-flash-live-preview".into()];
        assert_eq!(selected_live_models(&p), vec!["gemini-3.8-live", "gemini-3.1-flash-live-preview"]);
    }
}
