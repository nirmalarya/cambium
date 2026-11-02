//! Observability and request hygiene for every HTTP request:
//! request ids, a JSON access log, Prometheus metrics, security headers, and
//! the audit trail for state changes and access denials.
use crate::St;
use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use cambium_core::audit::AuditEvent;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Per-request context, visible to handlers through a task-local.
pub struct ReqCtx {
    pub id: String,
    pub actor: Mutex<Option<String>>,
}
tokio::task_local! { pub static REQ: ReqCtx; }

pub fn request_id() -> Option<String> {
    REQ.try_with(|r| r.id.clone()).ok()
}
/// Called by authentication once the caller is known.
pub fn set_actor(user: &str) {
    let _ = REQ.try_with(|r| *r.actor.lock().unwrap() = Some(user.to_string()));
}
fn actor() -> Option<String> {
    REQ.try_with(|r| r.actor.lock().unwrap().clone()).ok().flatten()
}

const BUCKETS: [f64; 12] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];
#[derive(Default)]
struct Hist {
    counts: [u64; 12],
    sum: f64,
    n: u64,
}
#[derive(Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<(String, String, u16), u64>>,
    latency: Mutex<BTreeMap<String, Hist>>,
    pub rate_limited: AtomicU64,
    pub auth_failures: AtomicU64,
    pub audit_errors: AtomicU64,
    pub shutting_down: AtomicBool,
}
pub fn metrics() -> &'static Metrics {
    static M: OnceLock<Metrics> = OnceLock::new();
    M.get_or_init(Metrics::default)
}

impl Metrics {
    fn record(&self, route: &str, method: &str, status: u16, secs: f64) {
        *self.requests.lock().unwrap().entry((route.into(), method.into(), status)).or_default() += 1;
        let mut l = self.latency.lock().unwrap();
        let h = l.entry(route.into()).or_default();
        for (i, b) in BUCKETS.iter().enumerate() {
            if secs <= *b {
                h.counts[i] += 1;
            }
        }
        h.sum += secs;
        h.n += 1;
    }
    /// Prometheus text exposition format.
    pub fn render(&self, st: &St) -> String {
        let mut o = String::new();
        o += "# HELP cambium_http_requests_total HTTP requests by route, method and status.\n# TYPE cambium_http_requests_total counter\n";
        for ((r, m, s), n) in self.requests.lock().unwrap().iter() {
            o += &format!("cambium_http_requests_total{{route=\"{r}\",method=\"{m}\",status=\"{s}\"}} {n}\n");
        }
        o += "# HELP cambium_http_errors_total Responses with status >= 500.\n# TYPE cambium_http_errors_total counter\n";
        let errs: u64 = self.requests.lock().unwrap().iter().filter(|((_, _, s), _)| *s >= 500).map(|(_, n)| n).sum();
        o += &format!("cambium_http_errors_total {errs}\n");
        o += "# HELP cambium_http_request_duration_seconds Request latency.\n# TYPE cambium_http_request_duration_seconds histogram\n";
        for (r, h) in self.latency.lock().unwrap().iter() {
            for (i, b) in BUCKETS.iter().enumerate() {
                o += &format!("cambium_http_request_duration_seconds_bucket{{route=\"{r}\",le=\"{b}\"}} {}\n", h.counts[i]);
            }
            o += &format!("cambium_http_request_duration_seconds_bucket{{route=\"{r}\",le=\"+Inf\"}} {}\n", h.n);
            o += &format!(
                "cambium_http_request_duration_seconds_sum{{route=\"{r}\"}} {}\ncambium_http_request_duration_seconds_count{{route=\"{r}\"}} {}\n",
                h.sum, h.n
            );
        }
        for (name, help, v) in [
            ("cambium_rate_limited_total", "Requests rejected with 429.", &self.rate_limited),
            ("cambium_auth_failures_total", "Requests rejected with 401.", &self.auth_failures),
            ("cambium_audit_write_errors_total", "Audit events that could not be written.", &self.audit_errors),
        ] {
            o += &format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {}\n", v.load(Ordering::Relaxed));
        }
        // WAL: head sequence, age of the newest entry, and changesets whose derived
        // manifest is not yet materialised (work the read path will still have to do).
        if let Ok(Some(r)) = st.db.one("SELECT max(seq), max(created_at) FROM wal", vec![]) {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
            o +=
                &format!("# HELP cambium_wal_head_seq Highest WAL sequence number.\n# TYPE cambium_wal_head_seq gauge\ncambium_wal_head_seq {}\n", r.i(0));
            o += &format!("# HELP cambium_wal_last_append_age_seconds Seconds since the newest WAL entry.\n# TYPE cambium_wal_last_append_age_seconds gauge\ncambium_wal_last_append_age_seconds {}\n",
                if r.oi(1).is_some() { now - r.i(1) } else { 0 });
        }
        if let Ok(Some(r)) = st.db.one(
            "SELECT count(*) FROM changesets c WHERE NOT EXISTS (SELECT 1 FROM derived d WHERE d.kind='manifest' AND d.cs_id=c.id)",
            vec![],
        ) {
            o += &format!("# HELP cambium_wal_derived_lag Changesets without a materialised manifest.\n# TYPE cambium_wal_derived_lag gauge\ncambium_wal_derived_lag {}\n", r.i(0));
        }
        if let Some(p) = st.db.pool_stats() {
            for (k, v) in [("size", &p.size), ("busy", &p.busy), ("queued", &p.queued), ("healthy", &p.healthy)] {
                o += &format!("cambium_db_pool_{k} {}\n", v.load(Ordering::Relaxed));
            }
            o += &format!(
                "# TYPE cambium_db_pool_reconnects_total counter\ncambium_db_pool_reconnects_total {}\n",
                p.reconnects.load(Ordering::Relaxed)
            );
            o += &format!("# TYPE cambium_db_pool_job_panics_total counter\ncambium_db_pool_job_panics_total {}\n", p.panics.load(Ordering::Relaxed));
        }
        o += &format!("cambium_up 1\ncambium_shutting_down {}\n", self.shutting_down.load(Ordering::Relaxed) as u8);
        o
    }
}

/// Route template with ids removed, so metric labels stay bounded.
pub fn route_of(path: &str) -> String {
    let s: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    while i < s.len() {
        let prev = if i > 0 { s[i - 1] } else { "" };
        let v = match (prev, s.first().copied()) {
            ("repos", Some("v1")) if i == 2 => ":r".to_string(),
            ("ui", _) if i == 1 && !matches!(s[i], "login" | "logout") => ":r".to_string(),
            ("refs" | "changesets" | "reviews" | "resolve" | "blobs", _) => ":id".to_string(),
            _ if s[i].len() > 40 => ":id".to_string(),
            _ => s[i].to_string(),
        };
        out.push(v);
        i += 1;
        if out.len() > 6 {
            break;
        }
    }
    format!("/{}", out.join("/"))
}
fn repo_of(path: &str) -> Option<String> {
    let s: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match s.as_slice() {
        ["v1", "repos", r, ..] => Some(r.to_string()),
        ["ui", r, ..] if !matches!(*r, "login" | "logout") => Some(r.to_string()),
        _ => None,
    }
}
/// Audit action name for a state-changing route (None = read-only or not audited).
fn action_of(method: &str, route: &str) -> Option<&'static str> {
    if method == "GET" || method == "HEAD" {
        return None;
    }
    Some(match route {
        "/v1/repos" => "repo.create",
        "/v1/repos/:r/refs/:id" => "ref.update",
        "/v1/repos/:r/commit" => "ref.commit",
        "/v1/repos/:r/merge" => "ref.merge",
        "/v1/repos/:r/rebase" => "ref.rebase",
        "/v1/repos/:r/acl" => "acl.change",
        "/v1/repos/:r/reviews" | "/ui/:r/reviews" => "review.create",
        "/v1/repos/:r/reviews/:id/comments" | "/ui/:r/reviews/:id/comments" => "review.comment",
        "/v1/repos/:r/reviews/:id/approve" | "/ui/:r/reviews/:id/approve" => "review.approve",
        "/v1/repos/:r/reviews/:id/merge" | "/ui/:r/reviews/:id/merge" => "review.merge",
        "/v1/repos/:r/reviews/:id/close" | "/ui/:r/reviews/:id/close" => "review.close",
        "/v1/repos/:r/protected" => "branch.protect",
        "/ui/login" => "auth.login",
        "/ui/logout" => "auth.logout",
        _ => return None, // tree/diff/blob uploads are reads or content-addressed writes
    })
}
pub fn audit(st: &St, e: AuditEvent) {
    if let Err(err) = st.eng.audit(e) {
        metrics().audit_errors.fetch_add(1, Ordering::Relaxed);
        cambium_core::log::error("audit write failed", json!({"error": err.to_string(), "request_id": request_id()}));
    }
}

/// Outermost layer: request id, access log, metrics, security headers, audit.
pub async fn observe(State(st): State<St>, req: Request, next: Next) -> Response {
    let t0 = Instant::now();
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .map(String::from)
        .unwrap_or_else(|| {
            use rand::RngCore;
            let mut b = [0u8; 8];
            rand::thread_rng().fill_bytes(&mut b);
            hex(&b)
        });
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let route = route_of(&path);
    let client =
        req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or("").trim().to_string());
    let ctx = ReqCtx { id: id.clone(), actor: Mutex::new(None) };
    let (mut resp, who) = REQ
        .scope(ctx, async move {
            let r = next.run(req).await;
            (r, actor())
        })
        .await;
    let status = resp.status().as_u16();
    let secs = t0.elapsed().as_secs_f64();
    let m = metrics();
    if !matches!(route.as_str(), "/metrics" | "/healthz" | "/readyz") {
        m.record(&route, &method, status, secs);
    }
    if status == 401 {
        m.auth_failures.fetch_add(1, Ordering::Relaxed);
    }
    let outcome = match status {
        200..=399 => "success",
        401 | 403 => "denied",
        _ => "failure",
    };
    let action = action_of(&method, &route).map(String::from).or_else(|| match status {
        401 => Some("auth.denied".into()),
        403 => Some("authz.denied".into()),
        _ => None,
    });
    if let Some(action) = action {
        audit(
            &st,
            AuditEvent {
                actor: who.clone().unwrap_or_else(|| "anonymous".into()),
                action,
                repo: repo_of(&path),
                target: Some(path.clone()),
                outcome: outcome.into(),
                detail: json!({"method": method, "status": status, "client_ip": client}),
                request_id: Some(id.clone()),
            },
        );
    }
    if !matches!(route.as_str(), "/healthz" | "/readyz" | "/metrics") || status >= 400 {
        cambium_core::log::emit(
            if status >= 500 { "error" } else { "info" },
            "request",
            json!({"request_id": id, "method": method, "route": route,
            "status": status, "duration_ms": (secs * 1000.0 * 100.0).round() / 100.0, "user": who, "client_ip": client}),
        );
    }
    let h = resp.headers_mut();
    let set = |h: &mut axum::http::HeaderMap, k: &'static str, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    };
    set(h, "x-request-id", &id);
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "no-referrer");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    set(h, "permissions-policy", "camera=(), microphone=(), geolocation=()");
    set(
        h,
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    if path.starts_with("/ui") {
        set(h, "cache-control", "no-store");
    }
    if crate::tls_enabled() || std::env::var("CAMBIUM_HSTS").is_ok_and(|v| v == "1") {
        set(h, "strict-transport-security", "max-age=31536000; includeSubDomains");
    }
    resp
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub async fn healthz() -> Response {
    (StatusCode::OK, axum::Json(json!({"status": "ok"}))).into_response()
}
/// Ready = not draining, metadata store answers, blob store answers.
pub async fn readyz(State(st): State<St>) -> Response {
    if metrics().shutting_down.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"status": "draining"}))).into_response();
    }
    let st2 = st.clone();
    let res = tokio::task::spawn_blocking(move || {
        let db = st2.db.ping().map_err(|e| format!("db: {e}"));
        let blobs = st2.blobs.get(&"0".repeat(64)).map(|_| ()).map_err(|e| format!("blobs: {e}"));
        (db, blobs)
    })
    .await;
    match res {
        Ok((Ok(()), Ok(()))) => (StatusCode::OK, axum::Json(json!({"status": "ready", "db": "ok", "blobs": "ok"}))).into_response(),
        Ok((d, b)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"status": "not_ready", "db": d.err().unwrap_or("ok".into()), "blobs": b.err().unwrap_or("ok".into())})),
        )
            .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"status": "not_ready", "error": e.to_string()}))).into_response(),
    }
}
pub async fn metrics_endpoint(State(st): State<St>, h: axum::http::HeaderMap) -> Response {
    if let Ok(want) = std::env::var("CAMBIUM_METRICS_TOKEN") {
        let got = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        if !got.is_some_and(|g| cambium_core::blob::ct_eq(g, &want)) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let st2 = st.clone();
    let body = tokio::task::spawn_blocking(move || metrics().render(&st2)).await.unwrap_or_default();
    ([("content-type", "text/plain; version=0.0.4")], body).into_response()
}
