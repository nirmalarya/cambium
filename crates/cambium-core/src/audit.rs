//! Append-only audit trail.
//!
//! Every row stores the SHA-256 of the previous row, so editing or removing a
//! row breaks the chain (`verify_audit`). The tables also refuse UPDATE and
//! DELETE through database triggers. Rows are exported as JSON lines for a SIEM.
use crate::engine::{Engine, Result};
use crate::p;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default)]
pub struct AuditEvent {
    pub actor: String,
    pub action: String,
    pub repo: Option<String>,
    pub target: Option<String>,
    pub outcome: String,
    pub detail: Value,
    pub request_id: Option<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn row_hash(prev: &str, ts: i64, e: &AuditEvent, detail: &str) -> String {
    let canon = json!([prev, ts, e.actor, e.action, e.repo, e.target, e.outcome, detail, e.request_id]).to_string();
    hex::encode(Sha256::digest(canon.as_bytes()))
}

impl Engine {
    /// Append one event; returns its sequence number.
    pub fn audit(&self, e: AuditEvent) -> Result<i64> {
        let ts = now();
        let pg = self.db.is_pg();
        let ev = e.clone();
        let seq = self.db.tx(move |t| {
            // one writer at a time across nodes keeps the hash chain linear
            if pg { t.batch("SELECT pg_advisory_xact_lock(7272)")?; }
            let prev = t.one("SELECT hash FROM audit_log ORDER BY seq DESC LIMIT 1", &[])?.map(|r| r.s(0)).unwrap_or_else(|| "0".repeat(64));
            let detail = if ev.detail.is_null() { "{}".to_string() } else { ev.detail.to_string() };
            let h = row_hash(&prev, ts, &ev, &detail);
            let r = t.one("INSERT INTO audit_log(ts,actor,action,repo,target,outcome,detail,request_id,prev_hash,hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) RETURNING seq",
                &p![ts, &ev.actor, &ev.action, ev.repo.clone(), ev.target.clone(), &ev.outcome, detail, ev.request_id.clone(), prev, h])?;
            Ok(r.unwrap().i(0))
        })?;
        if let Ok(path) = std::env::var("CAMBIUM_AUDIT_FILE") {
            // optional file sink for log shippers; the database stays the source of truth
            if let Ok(mut rows) = self.audit_export(seq - 1, 1) {
                if let Some(r) = rows.pop() {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                        let _ = writeln!(f, "{r}");
                    }
                }
            }
        }
        Ok(seq)
    }
    /// Events after `since` (exclusive), oldest first, as SIEM-ready JSON objects.
    pub fn audit_export(&self, since: i64, limit: i64) -> Result<Vec<Value>> {
        Ok(self.db.query("SELECT seq,ts,actor,action,repo,target,outcome,detail,request_id,prev_hash,hash FROM audit_log WHERE seq>?1 ORDER BY seq LIMIT ?2",
            p![since, limit])?.into_iter().map(|r| {
                let detail: Value = serde_json::from_str(&r.s(7)).unwrap_or(Value::Null);
                json!({"seq": r.i(0), "ts": r.i(1), "time": rfc3339(r.i(1)), "product": "cambium", "kind": "audit",
                       "actor": r.s(2), "action": r.s(3), "repo": r.os(4), "target": r.os(5), "outcome": r.s(6),
                       "detail": detail, "request_id": r.os(8), "prev_hash": r.s(9), "hash": r.s(10)})
            }).collect())
    }
    /// Walk the chain; Ok((rows, None)) when intact, else the first bad seq.
    pub fn verify_audit(&self) -> Result<(i64, Option<i64>)> {
        let mut prev = "0".repeat(64);
        let mut n = 0;
        let mut since = 0;
        loop {
            let rows = self.db.query("SELECT seq,ts,actor,action,repo,target,outcome,detail,request_id,prev_hash,hash FROM audit_log WHERE seq>?1 ORDER BY seq LIMIT 5000", p![since])?;
            if rows.is_empty() {
                return Ok((n, None));
            }
            for r in rows {
                let e = AuditEvent {
                    actor: r.s(2),
                    action: r.s(3),
                    repo: r.os(4),
                    target: r.os(5),
                    outcome: r.s(6),
                    detail: Value::Null,
                    request_id: r.os(8),
                };
                if r.s(9) != prev || row_hash(&prev, r.i(1), &e, &r.s(7)) != r.s(10) {
                    return Ok((n, Some(r.i(0))));
                }
                prev = r.s(10);
                since = r.i(0);
                n += 1;
            }
        }
    }
}

/// Seconds since the epoch as an RFC 3339 UTC timestamp.
pub fn rfc3339(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}
