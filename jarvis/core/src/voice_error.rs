//! Типизированные отказы голосового тракта: классификация ошибок провайдеров,
//! безопасное логирование без секретов и короткие голосовые подсказки для штатной Алисы.

use jiff::tz::TimeZone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Config,
    Auth,
    Vpn,
    Connect,
    Stream,
    Audio,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Config => "config",
            Stage::Auth => "auth",
            Stage::Vpn => "vpn",
            Stage::Connect => "connect",
            Stage::Stream => "stream",
            Stage::Audio => "audio",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    MissingConfig,
    AuthInvalid,
    AccessDenied,
    QuotaMinute,
    QuotaDaily,
    QuotaUnknown,
    BillingRequired,
    ModelUnavailable,
    VpnBlocked,
    NetworkError,
    StreamDropped,
    AudioError,
}

impl FailureKind {
    pub fn code(self) -> &'static str {
        match self {
            FailureKind::MissingConfig => "missing_config",
            FailureKind::AuthInvalid => "auth_invalid",
            FailureKind::AccessDenied => "access_denied",
            FailureKind::QuotaMinute => "quota_minute",
            FailureKind::QuotaDaily => "quota_daily",
            FailureKind::QuotaUnknown => "quota_exceeded",
            FailureKind::BillingRequired => "billing_required",
            FailureKind::ModelUnavailable => "model_unavailable",
            FailureKind::VpnBlocked => "vpn_blocked",
            FailureKind::NetworkError => "network_error",
            FailureKind::StreamDropped => "stream_dropped",
            FailureKind::AudioError => "audio_error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceFailure {
    pub provider: String,
    pub stage: Stage,
    pub kind: FailureKind,
    pub retry_after_s: Option<u64>,
    pub reset_at: Option<i64>,
    pub key_count: usize,
    pub models: Vec<String>,
    pub has_other_models: bool,
    pub detail: String,
    pub action: String,
    pub retryable: bool,
    pub expected: bool,
}

impl VoiceFailure {
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    pub fn cooldown_key(&self) -> String {
        format!("{}:{}", self.provider, self.code())
    }

    pub fn log_line(&self) -> String {
        format!(
            "диагностика голоса: provider={} stage={} code={} keys={} detail={} | действие: {}",
            self.provider,
            self.stage.as_str(),
            self.code(),
            self.key_count,
            self.detail,
            self.action
        )
    }

    /// Короткая безопасная и подробная фраза для дословной озвучки штатной Алисой.
    pub fn alice_phrase(&self, tz: &TimeZone) -> String {
        let svc = provider_spoken(&self.provider);
        let svc_gen = provider_spoken_genitive(&self.provider);
        let model_ctx = spoken_models_context(&self.models);
        let alt_hint = if self.has_other_models {
            "или отметьте запасную модель в настройках"
        } else {
            "или добавьте ключ с другого аккаунта"
        };
        match self.kind {
            FailureKind::MissingConfig => {
                format!("Джарвис не настроен: подключите {svc} в настройках.")
            }
            FailureKind::AuthInvalid => {
                if self.key_count > 1 {
                    format!(
                        "У {svc_gen} отклонены {}. Проверьте ключи в настройках.",
                        all_keys_nominative(self.key_count)
                    )
                } else {
                    format!("Не удалось войти в {svc}. Проверьте ключ или войдите заново в настройках.")
                }
            }
            FailureKind::AccessDenied => {
                format!("{svc} отказал в доступе. Проверьте аккаунт и настройки подключения.")
            }
            FailureKind::QuotaMinute => {
                let wait = self
                    .retry_after_s
                    .map(spoken_wait)
                    .unwrap_or_else(|| "минуту".into());
                if self.key_count > 1 {
                    format!(
                        "У {svc_gen} {} упёрлись в минутный лимит{model_ctx}. Попробуйте через {wait}.",
                        all_keys_nominative(self.key_count)
                    )
                } else {
                    format!("У {svc_gen} минутный лимит{model_ctx}. Попробуйте через {wait}.")
                }
            }
            FailureKind::QuotaDaily => {
                let keys_part = if self.key_count > 1 {
                    format!("исчерпаны {}: дневной лимит{model_ctx}", all_keys_nominative(self.key_count))
                } else {
                    format!("закончился дневной лимит{model_ctx}")
                };
                if let Some(reset_at) = self.reset_at
                    && let Some(hhmm) = format_local_hhmm(reset_at, tz)
                {
                    return format!(
                        "У {svc_gen} {keys_part}. Обновится около {hhmm}, {alt_hint}."
                    );
                }
                format!("У {svc_gen} {keys_part}. Попробуйте позже {alt_hint}.")
            }
            FailureKind::QuotaUnknown => {
                let keys_part = if self.key_count > 1 {
                    format!("{} упёрлись в лимит квоты{model_ctx}", all_keys_nominative(self.key_count))
                } else {
                    format!("достигнут лимит квоты{model_ctx}")
                };
                if let Some(secs) = self.retry_after_s {
                    let wait = spoken_wait(secs);
                    format!("У {svc_gen} {keys_part}. Перепроверим через {wait}, {alt_hint}.")
                } else {
                    format!("У {svc_gen} {keys_part}. Попробуйте позже {alt_hint}.")
                }
            }
            FailureKind::BillingRequired => {
                if self.key_count > 1 {
                    format!(
                        "У {svc_gen} на {} нулевая квота или не включён биллинг{model_ctx}.",
                        all_keys_prepositional(self.key_count)
                    )
                } else {
                    format!("У {svc_gen} недостаточно оплаченных кредитов или не включён биллинг{model_ctx}.")
                }
            }
            FailureKind::ModelUnavailable => {
                format!("Выбранная голосовая модель {svc_gen} недоступна. Проверьте настройки ассистента.")
            }
            FailureKind::VpnBlocked => {
                format!("Проблема подключения к {svc} через ВПН. Проверьте ВПН в настройках.")
            }
            FailureKind::NetworkError => {
                format!("Не удаётся связаться с {svc}. Возможно, проблема с интернетом или ВПН.")
            }
            FailureKind::StreamDropped => {
                format!("Голосовое соединение с {svc} оборвалось.")
            }
            FailureKind::AudioError => {
                "Не удалось воспроизвести голосовой ответ.".into()
            }
        }
    }
}

fn all_keys_nominative(n: usize) -> String {
    let mod100 = n % 100;
    let mod10 = n % 10;
    let word = if (11..=14).contains(&mod100) {
        "ключей"
    } else if mod10 == 1 {
        "ключ"
    } else if (2..=4).contains(&mod10) {
        "ключа"
    } else {
        "ключей"
    };
    format!("все {n} {word}")
}

fn all_keys_prepositional(n: usize) -> String {
    format!("всех {n} ключах")
}

fn spoken_model_name(id: &str) -> &'static str {
    match id {
        "gemini-3.8-live" => "Джемини 3.8",
        "gemini-3.8-live-extended-thinking" => "Джемини 3.8 думающей",
        "gemini-3.1-flash-live-preview" => "Джемини 3.1 Флеш",
        "gemini-2.5-flash-native-audio-preview-12-2025" => "Джемини 2.5 Флеш",
        _ => "",
    }
}

fn spoken_models_context(models: &[String]) -> String {
    match models.len() {
        0 => String::new(),
        1 => {
            let name = spoken_model_name(&models[0]);
            if name.is_empty() {
                String::new()
            } else {
                format!(" на модели {name}")
            }
        }
        n => format!(" на всех {n} выбранных моделях"),
    }
}

pub fn provider_display(provider: &str) -> &'static str {
    match provider {
        "chatgpt" => "ChatGPT",
        "gemini" => "Gemini",
        "openai" | "live" => "OpenAI",
        "grok" => "Grok",
        "elevenlabs" | "eleven_realtime" => "ElevenLabs",
        "classic" => "Джарвис",
        _ => "AI",
    }
}

fn provider_spoken(provider: &str) -> &'static str {
    match provider {
        "chatgpt" => "Чат Джи Пи Ти",
        "gemini" => "Джемини",
        "openai" | "live" => "Опен Эй Ай",
        "grok" => "Грок",
        "elevenlabs" | "eleven_realtime" => "Элевен Лабс",
        "classic" => "Джарвис",
        _ => "нейросеть",
    }
}

fn provider_spoken_genitive(provider: &str) -> &'static str {
    match provider {
        "chatgpt" => "Чат Джи Пи Ти",
        "gemini" => "Джемини",
        "openai" | "live" => "Опен Эй Ай",
        "grok" => "Грока",
        "elevenlabs" | "eleven_realtime" => "Элевен Лабс",
        "classic" => "Джарвиса",
        _ => "сервиса",
    }
}

fn spoken_wait(secs: u64) -> String {
    if secs <= 75 {
        "минуту".into()
    } else if secs < 150 {
        "пару минут".into()
    } else {
        let mins = secs.div_ceil(60);
        if mins >= 60 {
            let hours = mins.div_ceil(60);
            format!("{hours} ч.")
        } else {
            format!("{mins} мин.")
        }
    }
}

pub fn format_local_hhmm(unix_ts: i64, tz: &TimeZone) -> Option<String> {
    let ts = jiff::Timestamp::from_second(unix_ts).ok()?;
    let zoned = ts.to_zoned(tz.clone());
    Some(format!("{:02}:{:02}", zoned.hour(), zoned.minute()))
}

/// Убрать возможные API-ключи, токены и query-секреты из текста ошибки перед записью в лог/панель.
pub fn sanitize_detail(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(240));
    let mut rest = raw.trim();
    while !rest.is_empty() {
        if let Some(pos) = find_secret_marker(rest) {
            out.push_str(&rest[..pos.start]);
            out.push_str(pos.replacement);
            rest = &rest[pos.end..];
        } else {
            out.push_str(rest);
            break;
        }
    }
    let clean: String = out
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // Обрезаем стандартный хвост Google WS Close ("For more information on this error, head to: h").
    let trimmed = clean
        .split("For more information on this error")
        .next()
        .unwrap_or(&clean)
        .trim()
        .trim_end_matches(['.', ' ']);
    if trimmed.chars().count() > 220 {
        format!("{}…", trimmed.chars().take(220).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

struct SecretSpan {
    start: usize,
    end: usize,
    replacement: &'static str,
}

fn find_secret_marker(s: &str) -> Option<SecretSpan> {
    let patterns: &[(&str, &'static str)] = &[
        ("AIza", "***"),
        ("sk-", "***"),
        ("xai-", "***"),
        ("key=", "key=***"),
        ("Bearer ", "Bearer ***"),
        ("bearer ", "bearer ***"),
    ];
    let mut best: Option<(usize, usize, &'static str)> = None;
    for &(prefix, repl) in patterns {
        if let Some(idx) = s.find(prefix) {
            if best.is_none_or(|(b, _, _)| idx < b) {
                best = Some((idx, prefix.len(), repl));
            }
        }
    }
    let (start, plen, replacement) = best?;
    let tail = &s[start + plen..];
    let token_len = tail
        .bytes()
        .take_while(|b| !b.is_ascii_whitespace() && !matches!(b, b'&' | b'"' | b'\'' | b',' | b')' | b']'))
        .count();
    Some(SecretSpan {
        start,
        end: start + plen + token_len,
        replacement,
    })
}

/// Извлечь паузу в секундах из заголовка/текста ошибки (`[retry=42s]`, `retry in 38.5s`, `через 5 мин.`).
pub fn extract_retry_seconds(raw: &str) -> Option<u64> {
    let lower = raw.to_ascii_lowercase();
    for marker in ["retry=", "retrydelay\":", "retrydelay=", "retry-after:", "retry_after="] {
        if let Some(pos) = lower.find(marker) {
            let after = lower[pos + marker.len()..].trim_start_matches([' ', '"', '\'']);
            if let Some(secs) = parse_duration_prefix(after) {
                return Some(secs);
            }
        }
    }
    for marker in ["retry in ", "try again in ", "please retry in "] {
        if let Some(pos) = lower.find(marker) {
            let after = &lower[pos + marker.len()..];
            if let Some(secs) = parse_duration_prefix(after) {
                return Some(secs);
            }
        }
    }
    if let Some(pos) = lower.find("через ") {
        let after = &lower[pos + "через ".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            let unit = after[digits.len()..].trim_start();
            if unit.starts_with("мин") {
                return Some(n.saturating_mul(60));
            }
            if unit.starts_with("с") || unit.starts_with("sec") {
                return Some(n);
            }
            if unit.starts_with("ч") {
                return Some(n.saturating_mul(3600));
            }
        }
    }
    None
}

fn parse_duration_prefix(s: &str) -> Option<u64> {
    let num_str: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if num_str.is_empty() {
        return None;
    }
    let val: f64 = num_str.parse().ok()?;
    if !val.is_finite() || val <= 0.0 {
        return None;
    }
    let rest = s[num_str.len()..].trim_start();
    let mult = if rest.starts_with("ms") {
        0.001
    } else if rest.starts_with("min") || rest.starts_with('m') {
        60.0
    } else if rest.starts_with('h') {
        3600.0
    } else {
        1.0
    };
    Some((val * mult).ceil().clamp(1.0, 86400.0) as u64)
}

fn is_proven_vpn_error(lower: &str) -> bool {
    lower.contains("плагин vpn")
        || lower.contains("vpn применяет")
        || lower.contains("vpn теперь отдельный")
        || lower.contains("неверный socks vpn")
        || lower.contains("ошибка vpn-прокси")
        || lower.contains("vpn не поддерживает udp")
        || lower.contains("vpn blocked")
        || lower.contains("socks5h")
        || lower.starts_with("vpn:")
}

fn contains_http(lower: &str, code: &str) -> bool {
    lower.contains(&format!("http {code}"))
        || lower.contains(&format!("ответил {code}"))
        || lower.contains(&format!("({code}"))
        || lower.contains(&format!("status {code}"))
        || lower.contains(&format!(" {code} "))
        || lower.ends_with(&format!(" {code}"))
}

/// Классифицировать текстовую/протокольную ошибку провайдера в структурированный `VoiceFailure`.
pub fn classify(
    provider: &str,
    stage: Stage,
    raw: &str,
    ws_code: Option<u16>,
    vpn_blocked: bool,
) -> VoiceFailure {
    let detail = sanitize_detail(raw);
    let lower = detail.to_ascii_lowercase();
    let retry_after_s = extract_retry_seconds(&detail);
    let prov = provider.to_string();

    let mk = |stage: Stage, kind: FailureKind, retry_after_s: Option<u64>, action: &str, retryable: bool| VoiceFailure {
        provider: prov.clone(),
        stage,
        kind,
        retry_after_s,
        reset_at: None,
        key_count: 0,
        models: Vec::new(),
        has_other_models: false,
        detail: detail.clone(),
        action: action.into(),
        retryable,
        expected: false,
    };

    if stage == Stage::Audio {
        return mk(stage, FailureKind::AudioError, None, "Проверьте динамик колонки и повторите попытку.", true);
    }

    if stage == Stage::Config {
        if lower.contains("ограничил голосовые звонки") || lower.contains("лимит") {
            return mk(stage, FailureKind::QuotaMinute, retry_after_s, "Подождите окончания ограничения или переключите ассистента на другой движок.", true);
        }
        return mk(stage, FailureKind::MissingConfig, None, "Откройте «Подключения» и настройте выбранный сервис.", false);
    }

    // Подтверждённый сбой локального VPN-туннеля (никогда не объявляем по обычному таймауту!).
    if vpn_blocked || is_proven_vpn_error(&lower) {
        return mk(Stage::Vpn, FailureKind::VpnBlocked, None, "Проверьте ссылку VLESS и статус плагина VPN в «Подключениях».", true);
    }

    // 402 / нулевой баланс / нулевая квота (limit: 0)
    if contains_http(&lower, "402")
        || lower.contains("limit: 0")
        || lower.contains("limit\":0")
        || lower.contains("insufficient_quota")
        || lower.contains("credit balance")
        || lower.contains("billing") && (lower.contains("enable") || lower.contains("not enabled") || lower.contains("account is not active") || lower.contains("paid"))
    {
        return mk(Stage::Auth, FailureKind::BillingRequired, None, "Проверьте биллинг и баланс сервиса или добавьте другой ключ.", false);
    }

    // 401 / невалидный ключ / истёкший токен
    if contains_http(&lower, "401")
        || lower.contains("api_key_invalid")
        || lower.contains("api key not valid")
        || lower.contains("invalid_api_key")
        || lower.contains("incorrect api key")
        || lower.contains("неверный api-ключ")
        || lower.contains("не выполнен вход")
        || lower.contains("не продлить вход")
        || lower.contains("unauthorized")
        || lower.contains("invalid_grant")
    {
        return mk(Stage::Auth, FailureKind::AuthInvalid, None, "Проверьте API-ключ или выполните вход заново в «Подключениях».", false);
    }

    // 429 / квота / rate limit / WS 1011 с квотой
    let is_quota = contains_http(&lower, "429")
        || lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
        || lower.contains("ограничил голосовые звонки")
        || lower.contains("исчерпан")
        || lower.contains("лимит")
        || (ws_code == Some(1011) && (lower.contains("exceeded") || lower.contains("limit") || lower.contains("quota") || lower.is_empty()));

    if is_quota {
        let is_daily = lower.contains("perday")
            || lower.contains("per_day")
            || lower.contains("per day")
            || lower.contains("rpd")
            || lower.contains("daily")
            || lower.contains("дневн");
        let is_minute = lower.contains("perminute")
            || lower.contains("per_minute")
            || lower.contains("per minute")
            || lower.contains("rpm")
            || lower.contains("tpm")
            || lower.contains("concurrent")
            || lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("too many requests")
            || lower.contains("ограничил голосовые звонки")
            || retry_after_s.is_some_and(|s| s <= 600);

        let (kind, action) = if is_daily && !is_minute {
            (
                FailureKind::QuotaDaily,
                "Дневной лимит исчерпан. Дождитесь сброса или добавьте ключ с другого аккаунта.",
            )
        } else if is_minute {
            (
                FailureKind::QuotaMinute,
                "Минутный лимит запросов. Подождите ~1 минуту или добавьте запасной ключ.",
            )
        } else {
            (
                FailureKind::QuotaUnknown,
                "Достигнут лимит запросов. Попробуйте через 1–2 минуты или проверьте квоту аккаунта.",
            )
        };
        return mk(stage, kind, retry_after_s, action, true);
    }

    // 403 / регион / запрет доступа (не утверждаем, что это квота!)
    if contains_http(&lower, "403")
        || lower.contains("forbidden")
        || lower.contains("permission_denied")
        || lower.contains("location is not supported")
        || lower.contains("region")
        || lower.contains("access denied")
    {
        return mk(Stage::Auth, FailureKind::AccessDenied, None, "Проверьте права аккаунта, регион и маршрут VPN в «Подключениях».", false);
    }

    // 404 / модель или голос не поддерживаются
    if contains_http(&lower, "404")
        || lower.contains("model_not_found")
        || lower.contains("unsupported_model")
        || lower.contains("not found")
        || lower.contains("not supported for bidigeneratecontent")
        || lower.contains("голос недоступен")
        || lower.contains("отклонил голос")
        || lower.contains("неверная модель")
        || ws_code == Some(1008)
    {
        return mk(Stage::Connect, FailureKind::ModelUnavailable, None, "Проверьте выбранную модель и голос в настройках ассистента.", false);
    }

    // Обрыв уже открытого потока
    if stage == Stage::Stream
        || lower.contains("обрыв голосового соединения")
        || lower.contains("канал chatgpt закрылся")
        || lower.contains("повреждённый пакет")
    {
        return mk(Stage::Stream, FailureKind::StreamDropped, None, "Повторите обращение; при частых обрывах проверьте стабильность сети или VPN.", true);
    }

    // Всё остальное на этапе подключения (DNS, таймаут, TLS, 5xx, нет сети)
    mk(stage, FailureKind::NetworkError, None, "Проверьте интернет, доступность сервиса и настройки VPN.", true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_quota_auth_vpn_and_network_accurately() {
        let tz = TimeZone::get("Europe/Moscow").unwrap();

        let rpm = classify(
            "gemini",
            Stage::Connect,
            "gemini ответил 429 [retry=45s, quota=GenerateRequestsPerMinutePerProjectPerModel]: Quota exceeded",
            None,
            false,
        );
        assert_eq!(rpm.kind, FailureKind::QuotaMinute);
        assert_eq!(rpm.retry_after_s, Some(45));
        assert!(rpm.alice_phrase(&tz).contains("минутный лимит"));

        let rpd = classify(
            "gemini",
            Stage::Connect,
            "gemini ответил 429 [quota=GenerateRequestsPerDayPerProjectPerModel]: Quota exceeded",
            None,
            false,
        );
        assert_eq!(rpd.kind, FailureKind::QuotaDaily);
        assert!(rpd.alice_phrase(&tz).contains("дневной лимит"));

        let zero = classify(
            "gemini",
            Stage::Connect,
            "gemini ответил 429: Quota exceeded for metric, limit: 0",
            None,
            false,
        );
        assert_eq!(zero.kind, FailureKind::BillingRequired);

        let auth = classify("openai", Stage::Auth, "openai ответил 401: Incorrect API key sk-secret12345", None, false);
        assert_eq!(auth.kind, FailureKind::AuthInvalid);
        assert!(!auth.detail.contains("sk-secret12345"));
        assert!(auth.detail.contains("***"));

        let forbidden = classify("gemini", Stage::Connect, "gemini ответил 403: User location is not supported", None, false);
        assert_eq!(forbidden.kind, FailureKind::AccessDenied);
        assert!(!forbidden.alice_phrase(&tz).contains("лимит"));

        // Обычный таймаут БЕЗ подтверждения от Proxy::route не должен винить VPN!
        let timeout = classify("openai", Stage::Connect, "Провайдер не ответил вовремя", None, false);
        assert_eq!(timeout.kind, FailureKind::NetworkError);
        assert!(timeout.alice_phrase(&tz).contains("Возможно, проблема с интернетом или ВПН"));

        // А при подтверждённом Route::Blocked или ошибке SOCKS — явно VPN.
        let vpn = classify("openai", Stage::Connect, "VPN применяет настройки", None, true);
        assert_eq!(vpn.kind, FailureKind::VpnBlocked);
        assert!(vpn.alice_phrase(&tz).contains("через ВПН"));

        let model = classify("gemini", Stage::Connect, "gemini: models/bad is not found", Some(1008), false);
        assert_eq!(model.kind, FailureKind::ModelUnavailable);

        let stream = classify("chatgpt", Stage::Stream, "Канал ChatGPT закрылся", None, false);
        assert_eq!(stream.kind, FailureKind::StreamDropped);

        let audio = classify("gemini", Stage::Audio, "aplay failed", None, false);
        assert_eq!(audio.kind, FailureKind::AudioError);
    }

    #[test]
    fn sanitizes_secrets_and_extracts_retry_durations() {
        let s = sanitize_detail("err ?key=AIzaSySecret12345&x=1 Bearer tok_abc999 sk-proj-999");
        assert!(!s.contains("AIzaSySecret12345"));
        assert!(!s.contains("tok_abc999"));
        assert!(!s.contains("sk-proj-999"));
        assert_eq!(extract_retry_seconds("429 [retry=37s]"), Some(37));
        assert_eq!(extract_retry_seconds("Please retry in 54.2s."), Some(55));
        assert_eq!(extract_retry_seconds("Повторите через 3 мин."), Some(180));
        assert_eq!(
            sanitize_detail("gemini: You exceeded your current quota. For more information on this error, head to: h"),
            "gemini: You exceeded your current quota"
        );
    }

    #[test]
    fn alice_phrase_states_exact_key_count_and_model_context() {
        let tz = TimeZone::get("Europe/Moscow").unwrap();
        let mut f = classify("gemini", Stage::Connect, "gemini ответил 429 [quota=PerDay]", None, false);
        f.key_count = 11;
        f.models = vec!["gemini-3.8-live".into()];
        f.has_other_models = true;
        f.reset_at = Some(1795593720);
        let p = f.alice_phrase(&tz);
        assert!(p.contains("исчерпаны все 11 ключей"), "{p}");
        assert!(p.contains("Джемини 3.8"), "{p}");
        assert!(p.contains("запасную модель"), "{p}");

        f.key_count = 3;
        f.models = vec!["gemini-3.8-live".into(), "gemini-3.1-flash-live-preview".into()];
        f.has_other_models = false;
        let p2 = f.alice_phrase(&tz);
        assert!(p2.contains("исчерпаны все 3 ключа"), "{p2}");
        assert!(p2.contains("на всех 2 выбранных моделях"), "{p2}");
        assert!(p2.contains("с другого аккаунта"), "{p2}");
    }
}
