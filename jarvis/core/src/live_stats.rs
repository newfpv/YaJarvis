//! Local cumulative audio counters, not provider quota. Cumulative reports are idempotent.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    since: i64,
    updated: i64,
    sessions: u64,
    connected_ms: u64,
    sent_ms: u64,
    received_ms: u64,
    limits: u64,
    recent: Vec<Report>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Report {
    session: String,
    started_at: i64,
    opened: bool,
    closed: bool,
    connected_ms: u64,
    sent_ms: u64,
    received_ms: u64,
    limit: bool,
    limit_at: i64,
}
impl Stats {
    pub fn apply(&mut self, p: &Value) -> Result<(), String> {
        let mut report: Report =
            serde_json::from_value(p.clone()).map_err(|_| "Некорректная статистика голоса")?;
        if report.session.is_empty()
            || report.session.len() > 128
            || report.started_at <= 0
            || report.started_at > crate::chatgpt_accounts::now() + 60
        {
            return Err("Некорректное соединение".into());
        }
        for n in [report.connected_ms, report.sent_ms, report.received_ms] {
            if n > 604800000 {
                return Err("Некорректный счётчик".into());
            }
        }
        let at = self.recent.iter().position(|v| v.session == report.session);
        if at.is_none() && self.recent.len() >= 64 && report.started_at <= self.recent[0].started_at
        {
            return Ok(());
        }
        let old = at.map(|i| self.recent[i].clone()).unwrap_or_default();
        report.opened |= old.opened;
        report.closed |= old.closed;
        report.limit |= old.limit;
        report.limit_at = report.limit_at.max(old.limit_at);
        report.connected_ms = report.connected_ms.max(old.connected_ms);
        report.sent_ms = report.sent_ms.max(old.sent_ms);
        report.received_ms = report.received_ms.max(old.received_ms);
        self.sessions += u64::from(report.opened && !old.opened);
        self.limits += u64::from(report.limit && !old.limit);
        self.connected_ms += report.connected_ms - old.connected_ms;
        self.sent_ms += report.sent_ms - old.sent_ms;
        self.received_ms += report.received_ms - old.received_ms;
        if self.since == 0 {
            self.since = report.started_at;
        }
        self.updated = crate::chatgpt_accounts::now();
        if let Some(i) = at {
            self.recent[i] = report;
        } else {
            self.recent.push(report);
        }
        self.recent.sort_by_key(|v| v.started_at);
        while self.recent.len() > 64 {
            self.recent.remove(0);
        }
        Ok(())
    }
    pub fn summary(&self) -> Value {
        json!({"since":self.since,"updated_at":self.updated,"sessions":self.sessions,"connected_ms":self.connected_ms,"sent_ms":self.sent_ms,"received_ms":self.received_ms,"limit_count":self.limits,
          "limit_events":self.recent.iter().rev().filter(|r|r.limit).take(8).map(|r|json!({"at":if r.limit_at>0{r.limit_at}else{r.started_at},"message":"OpenAI сообщил об ограничении GPT-Live"})).collect::<Vec<_>>()})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_out_of_order_and_reloaded_reports_do_not_double_count() {
        let mut s = Stats::default();
        let report = json!({"session":"s","started_at":100,"opened":true,"connected_ms":1000,"sent_ms":800,"received_ms":500});
        s.apply(&report).unwrap();
        s.apply(&report).unwrap();
        let mut end = report.clone();
        end["connected_ms"] = 2000.into();
        end["closed"] = true.into();
        end["limit"] = true.into();
        s.apply(&end).unwrap();
        s.apply(&report).unwrap();
        let mut reloaded: Stats =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        reloaded.apply(&end).unwrap();
        let public = reloaded.summary();
        assert_eq!(public["sessions"], 1);
        assert_eq!(public["connected_ms"], 2000);
        assert_eq!(public["sent_ms"], 800);
        assert_eq!(public["limit_count"], 1);
        assert!(public.get("recent").is_none());
    }
    #[test]
    fn rejected_connection_is_not_a_conversation_and_history_is_bounded() {
        let mut s = Stats::default();
        for i in 1..100 {
            s.apply(&json!({"session":format!("s{i}"),"started_at":i,"limit":true,"closed":true}))
                .unwrap();
        }
        assert_eq!(s.summary()["sessions"], 0);
        assert_eq!(s.summary()["limit_count"], 99);
        assert_eq!(s.recent.len(), 64);
        assert_eq!(s.summary()["limit_events"].as_array().unwrap().len(), 8);
    }
}
