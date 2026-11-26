use serde_json::Value;
#[derive(Clone, Debug)]
pub struct Failure {
    pub reason: &'static str,
    pub retry: i64,
}
impl Failure {
    pub fn wire(&self) -> String {
        format!("CGACCOUNT|{}|{}", self.reason, self.retry)
    }
    pub fn parse(s: &str) -> Option<Self> {
        let mut p = s.split('|');
        if p.next() != Some("CGACCOUNT") {
            return None;
        }
        let reason = match p.next()? {
            "auth" => "auth",
            "limit" => "limit",
            "access" => "access",
            _ => return None,
        };
        Some(Self {
            reason,
            retry: p.next()?.parse().ok()?,
        })
    }
    pub fn message(&self) -> String {
        match self.reason {
            "auth" => "Нужно заново войти в аккаунт ChatGPT".into(),
            "access" => "Аккаунту недоступен этот сервис ChatGPT".into(),
            _ => "Ограничение ChatGPT: выберите другой аккаунт или повторите позже".into(),
        }
    }
}
pub fn classify(status: u16, p: &Value, retry: i64) -> Option<Failure> {
    let code = p["error"]["code"]
        .as_str()
        .or_else(|| p["detail"]["code"].as_str())
        .or_else(|| p["code"].as_str())
        .or_else(|| p["error"].as_str())
        .unwrap_or("");
    let reason = match code {
        "invalid_grant"
        | "invalid_token"
        | "token_expired"
        | "token_revoked"
        | "refresh_token_reused"
        | "refresh_token_expired"
        | "refresh_token_invalidated"
        | "account_deactivated"
        | "account_disabled" => "auth",
        "rate_limit_exceeded"
        | "usage_limit_reached"
        | "insufficient_quota"
        | "quota_exceeded"
        | "voice_usage_limit_reached" => "limit",
        "subscription_required" | "not_entitled" | "voice_not_available" => "access",
        _ if p.is_object() && status == 401 => "auth",
        _ if p.is_object() && status == 429 => "limit",
        _ => return None,
    };
    Some(Failure {
        reason,
        retry: retry.clamp(1, 86400),
    })
}
pub async fn rejected(mut response: reqwest::Response) -> String {
    let status = response.status().as_u16();
    let retry = response
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len() + chunk.len() > 16384 {
            break;
        }
        bytes.extend(chunk);
    }
    if let Ok(p) = serde_json::from_slice::<Value>(&bytes) {
        if let Some(f) = classify(status, &p, retry) {
            return f.wire();
        }
    }
    format!("ChatGPT ответил HTTP {status}; причина не подтверждает проблему аккаунта")
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn network_errors_do_not_rotate_accounts() {
        for status in [400, 403, 429, 500, 502, 503] {
            assert!(classify(status, &Value::Null, 60).is_none());
        }
        assert!(
            classify(
                403,
                &json!({"error":{"code":"unsupported_country_region_territory"}}),
                60
            )
            .is_none()
        );
        assert!(classify(400, &json!({"error":{"code":"invalid_voice"}}), 60).is_none());
    }
    #[test]
    fn confirmed_account_errors_and_retry() {
        assert_eq!(
            classify(400, &json!({"error":"invalid_grant"}), 60)
                .unwrap()
                .reason,
            "auth"
        );
        let f = classify(429, &json!({"error":{"code":"rate_limit_exceeded"}}), 120).unwrap();
        assert_eq!(Failure::parse(&f.wire()).unwrap().retry, 120);
        assert!(Failure::parse("network timeout").is_none());
    }
}
