//! Structured logging: one JSON object per line on stderr, so a log shipper can
//! forward it unchanged. `CAMBIUM_LOG=debug|info|warn|error` (default info).
use serde_json::{Map, Value};

fn level_num(l: &str) -> u8 {
    match l {
        "debug" => 0,
        "info" => 1,
        "warn" => 2,
        _ => 3,
    }
}

pub fn enabled(level: &str) -> bool {
    let min = std::env::var("CAMBIUM_LOG").unwrap_or_else(|_| "info".into());
    level_num(level) >= level_num(&min)
}
/// Current time as RFC 3339 with milliseconds (UTC).
pub fn ts() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let s = crate::audit::rfc3339(d.as_secs() as i64);
    format!("{}.{:03}Z", &s[..19], d.subsec_millis())
}
pub fn emit(level: &str, msg: &str, fields: Value) {
    if !enabled(level) {
        return;
    }
    let mut m = Map::new();
    m.insert("ts".into(), Value::String(ts()));
    m.insert("level".into(), Value::String(level.into()));
    m.insert("msg".into(), Value::String(msg.into()));
    if let Value::Object(f) = fields {
        for (k, v) in f {
            m.insert(k, v);
        }
    }
    // never panic on a closed/broken log pipe (e.g. the log shipper died): drop the line instead
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{}", Value::Object(m));
}
pub fn info(msg: &str, fields: Value) {
    emit("info", msg, fields)
}
pub fn warn(msg: &str, fields: Value) {
    emit("warn", msg, fields)
}
pub fn error(msg: &str, fields: Value) {
    emit("error", msg, fields)
}
