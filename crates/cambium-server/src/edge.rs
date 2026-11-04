//! Cache tiers: `cmbd daemon` (one per machine) -> `cmbd edge` (one per
//! region) -> `cmbd` (authoritative). Every tier speaks the same API and talks only
//! to its one upstream (CAMBIUM_UPSTREAM), so tiers stack and failures stay isolated.
//! Immutable objects are served locally after the first fetch: blobs (checked
//! against their SHA-256 id) and complete trees (checked against their tree id).
//! Mutable things (refs, roots, writes, reviews) always go upstream.
//!
//! Permissions stay with the authoritative server. A cached object is served only
//! after the caller's own credentials were accepted upstream for that object; a
//! positive answer is remembered for CAMBIUM_EDGE_AUTHZ_TTL seconds (default 60,
//! 0 = ask every time), which bounds how long a revoked grant can still be served.
//! Denials are never remembered.
//!
//! The cache can be capped with CAMBIUM_EDGE_CACHE_MB (default 0 = no cap): every
//! 30 s the oldest cached objects are removed until it is back under 90% of the cap
//! (an object removed is simply fetched again on its next use). `/healthz` answers
//! locally; `/readyz` checks the upstream. On SIGTERM the tier fails `/readyz` for
//! CAMBIUM_SHUTDOWN_DRAIN_S (default 5) and then finishes in-flight requests.
use axum::{
    body::Bytes,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use cambium_core::blob::{BlobStore, FsBlobStore};
use cambium_core::limits::parse_range;
use std::sync::Arc;

struct Edge {
    upstream: String,
    agent: ureq::Agent,
    store: FsBlobStore,
    /// complete trees, one file per (repo, tree id, path)
    trees: std::path::PathBuf,
    /// (credential hash, object) -> when the upstream last said yes
    authz: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    authz_ttl: std::time::Duration,
    /// "daemon" or "edge", reported in x-cambium-tier
    tier: &'static str,
    /// set on SIGTERM: /readyz fails so traffic moves elsewhere before we stop
    draining: std::sync::atomic::AtomicBool,
}
impl Edge {
    fn authz_key(h: &HeaderMap, object: &str) -> Option<String> {
        let cred = h.get("authorization").and_then(|v| v.to_str().ok())?;
        Some(cambium_core::blob::hash_bytes(format!("{cred}\n{object}").as_bytes()))
    }
    fn allowed(&self, h: &HeaderMap, object: &str) -> bool {
        let Some(k) = Self::authz_key(h, object) else { return false };
        self.authz.lock().unwrap().get(&k).is_some_and(|t| t.elapsed() < self.authz_ttl)
    }
    fn remember(&self, h: &HeaderMap, object: &str) {
        if self.authz_ttl.is_zero() {
            return;
        }
        if let Some(k) = Self::authz_key(h, object) {
            let mut m = self.authz.lock().unwrap();
            if m.len() > 100_000 {
                m.retain(|_, t| t.elapsed() < self.authz_ttl);
            }
            m.insert(k, std::time::Instant::now());
        }
    }
}
/// `/v1/repos/<repo>/trees` (batched tree fetch)
fn tree_batch_req(path_q: &str) -> Option<String> {
    let path = path_q.split('?').next()?;
    let repo = path.strip_prefix("/v1/repos/")?.strip_suffix("/trees")?;
    (!repo.is_empty() && !repo.contains('/')).then(|| repo.to_string())
}
/// Cache a complete tree whose content matches its id.
fn keep_tree(file: &std::path::Path, id: &str, v: &serde_json::Value, body: &[u8]) {
    let entries = serde_json::from_value::<Vec<cambium_core::tree::TreeEntry>>(v["entries"].clone());
    if v["complete"] == true && entries.is_ok_and(|en| cambium_core::tree::Tree { entries: en }.id() == id) {
        let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }
}
/// `/v1/repos/<repo>/trees/<id>` with `?path=<dir>`
fn tree_req(path_q: &str) -> Option<(String, String, String)> {
    let (path, q) = path_q.split_once('?').unwrap_or((path_q, ""));
    let rest = path.strip_prefix("/v1/repos/")?;
    let (repo, rest) = rest.split_once('/')?;
    let id = rest.strip_prefix("trees/")?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let dir = q.split('&').find_map(|kv| kv.strip_prefix("path=")).unwrap_or("").to_string();
    Some((repo.to_string(), id.to_string(), dir))
}

const FWD_REQ: &[&str] = &["authorization", "content-type", "cookie", "range", "accept", "idempotency-key", "x-request-id"];
const FWD_RESP: &[&str] = &["content-type", "content-range", "accept-ranges", "set-cookie", "location", "retry-after", "x-request-id"];

fn upstream_call(
    e: &Edge,
    method: &str,
    path_q: &str,
    h: &HeaderMap,
    range: Option<&str>,
    body: &[u8],
) -> std::result::Result<ureq::Response, String> {
    let mut r = e.agent.request(method, &format!("{}{}", e.upstream, path_q));
    for k in FWD_REQ {
        if *k == "range" {
            continue;
        }
        if let Some(v) = h.get(*k).and_then(|v| v.to_str().ok()) {
            r = r.set(k, v);
        }
    }
    if let Some(rg) = range {
        r = r.set("range", rg);
    }
    let res = if body.is_empty() && (method == "GET" || method == "HEAD") { r.call() } else { r.send_bytes(body) };
    match res {
        Ok(x) => Ok(x),
        Err(ureq::Error::Status(_, x)) => Ok(x),
        Err(err) => Err(format!("upstream unreachable: {err}")),
    }
}
fn to_response(x: ureq::Response, tag: Option<&str>) -> Response {
    let status = StatusCode::from_u16(x.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut hdrs = HeaderMap::new();
    for k in FWD_RESP {
        for v in x.all(k) {
            if let Ok(v) = HeaderValue::from_str(v) {
                hdrs.append(*k, v);
            }
        }
    }
    if let Some(t) = tag {
        hdrs.insert("x-cambium-edge", HeaderValue::from_str(t).unwrap());
    }
    let mut body = vec![];
    let _ = std::io::Read::read_to_end(&mut x.into_reader(), &mut body);
    (status, hdrs, body).into_response()
}
fn serve_local(data: Vec<u8>, range: Option<&str>, tag: &str) -> Response {
    let len = data.len() as u64;
    let edge = ("x-cambium-edge", tag.to_string());
    match range {
        None => ([("accept-ranges", "bytes".to_string()), ("content-length", len.to_string()), edge], data).into_response(),
        Some(r) => match parse_range(r, len) {
            Ok((a, b)) => (
                StatusCode::PARTIAL_CONTENT,
                [("accept-ranges", "bytes".to_string()), ("content-range", format!("bytes {a}-{b}/{len}")), edge],
                data[a as usize..=b as usize].to_vec(),
            )
                .into_response(),
            Err(()) => (StatusCode::RANGE_NOT_SATISFIABLE, [("content-range", format!("bytes */{len}")), edge]).into_response(),
        },
    }
}
fn blob_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v1/blobs/")?;
    (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())).then_some(id)
}

fn handle(e: &Edge, method: String, path_q: String, h: HeaderMap, body: Bytes) -> Response {
    let mut r = handle_inner(e, method, path_q, h, body);
    r.headers_mut().insert("x-cambium-tier", HeaderValue::from_static(e.tier));
    r
}
fn handle_inner(e: &Edge, method: String, path_q: String, h: HeaderMap, body: Bytes) -> Response {
    // the tier's own health: liveness never depends on the upstream; readiness does
    match (method.as_str(), path_q.split('?').next().unwrap_or("")) {
        ("GET", "/healthz") => return (StatusCode::OK, "ok").into_response(),
        ("GET", "/readyz") => {
            if e.draining.load(std::sync::atomic::Ordering::Relaxed) {
                return (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response();
            }
            return match e.agent.get(&format!("{}/healthz", e.upstream)).timeout(std::time::Duration::from_secs(2)).call() {
                Ok(_) => (StatusCode::OK, "ready").into_response(),
                Err(err) => (StatusCode::SERVICE_UNAVAILABLE, format!("upstream not reachable: {err}")).into_response(),
            };
        }
        _ => {}
    }
    if method == "POST" {
        if let Some(repo) = tree_batch_req(&path_q) {
            if let Some(r) = tree_batch(e, &repo, &path_q, &h, &body) {
                return r;
            }
        }
    }
    handle_get(e, method, path_q, h, body)
}
/// Batched tree fetch through a tier: answer what is cached (for callers the upstream
/// recently allowed), forward only the misses upstream, cache what comes back.
fn tree_batch(e: &Edge, repo: &str, path_q: &str, h: &HeaderMap, body: &[u8]) -> Option<Response> {
    let req: serde_json::Value = serde_json::from_slice(body).ok()?;
    let items = req["items"].as_array()?.clone();
    let key = |it: &serde_json::Value| -> Option<(String, String, String, std::path::PathBuf)> {
        let (id, dir) = (it["id"].as_str()?.to_string(), it["path"].as_str().unwrap_or("").to_string());
        let object = format!("tree\n{repo}\n{id}\n{dir}");
        let file = e.trees.join(cambium_core::blob::hash_bytes(object.as_bytes()));
        Some((id, dir, object, file))
    };
    let mut out: Vec<Option<serde_json::Value>> = vec![None; items.len()];
    let mut misses = vec![];
    for (i, it) in items.iter().enumerate() {
        let (_, _, object, file) = key(it)?;
        let cached = e.allowed(h, &object).then(|| std::fs::read(&file).ok()).flatten();
        match cached.and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
            Some(v) => out[i] = Some(v),
            None => misses.push(i),
        }
    }
    let tag = if misses.is_empty() {
        "hit"
    } else if misses.len() == items.len() {
        "miss"
    } else {
        "partial"
    };
    if !misses.is_empty() {
        let fwd = serde_json::json!({"items": misses.iter().map(|i| items[*i].clone()).collect::<Vec<_>>()});
        let x = match upstream_call(e, "POST", path_q, h, None, fwd.to_string().as_bytes()) {
            Ok(x) => x,
            Err(m) => return Some((StatusCode::BAD_GATEWAY, m).into_response()),
        };
        if x.status() != 200 {
            return Some(to_response(x, Some("miss")));
        }
        let v: serde_json::Value = serde_json::from_reader(x.into_reader()).ok()?;
        let trees = v["trees"].as_array()?;
        for (slot, t) in misses.iter().zip(trees) {
            // reuse authorization only for callers who saw the complete tree: a filtered
            // answer must never let this caller be served the unfiltered cached copy
            if t.get("error").is_none() && t["complete"] == true {
                let (id, _, object, file) = key(&items[*slot])?;
                e.remember(h, &object);
                keep_tree(&file, &id, t, t.to_string().as_bytes());
            }
            out[*slot] = Some(t.clone());
        }
    }
    let body = serde_json::json!({"trees": out.into_iter().map(|t| t.unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>()});
    Some(([("content-type", "application/json"), ("x-cambium-edge", tag)], body.to_string()).into_response())
}
fn handle_get(e: &Edge, method: String, path_q: String, h: HeaderMap, body: Bytes) -> Response {
    let bad = |m: String| (StatusCode::BAD_GATEWAY, m).into_response();
    let path = path_q.split('?').next().unwrap_or("").to_string();
    let range = h.get("range").and_then(|v| v.to_str().ok()).map(String::from);
    if method == "GET" {
        if let Some(id) = blob_id(&path) {
            let object = format!("blob\n{id}");
            if let Ok(Some(data)) = e.store.get(id) {
                // cached: the authoritative server decides whether this caller may read it
                if !e.allowed(&h, &object) {
                    let probe = match upstream_call(e, "GET", &path_q, &h, Some("bytes=0-0"), &[]) {
                        Ok(x) => x,
                        Err(m) => return bad(m),
                    };
                    if !matches!(probe.status(), 200 | 206 | 416) {
                        return to_response(probe, Some("denied"));
                    }
                    e.remember(&h, &object);
                }
                return serve_local(data, range.as_deref(), "hit");
            }
            let x = match upstream_call(e, "GET", &path_q, &h, None, &[]) {
                Ok(x) => x,
                Err(m) => return bad(m),
            };
            if x.status() != 200 {
                return to_response(x, Some("miss"));
            }
            let mut data = vec![];
            if std::io::Read::read_to_end(&mut x.into_reader(), &mut data).is_err() {
                return bad("upstream read failed".into());
            }
            if cambium_core::blob::hash_bytes(&data) != id {
                return bad("upstream blob failed integrity check".into());
            }
            let _ = e.store.put(&data);
            e.remember(&h, &object);
            return serve_local(data, range.as_deref(), "miss");
        }
        if let Some((repo, id, dir)) = tree_req(&path_q) {
            let object = format!("tree\n{repo}\n{id}\n{dir}");
            let file = e.trees.join(cambium_core::blob::hash_bytes(object.as_bytes()));
            if e.allowed(&h, &object) {
                if let Ok(body) = std::fs::read(&file) {
                    return ([("content-type", "application/json"), ("x-cambium-edge", "hit")], body).into_response();
                }
            }
            let x = match upstream_call(e, "GET", &path_q, &h, None, &[]) {
                Ok(x) => x,
                Err(m) => return bad(m),
            };
            if x.status() != 200 {
                return to_response(x, Some("miss"));
            }
            let mut body = vec![];
            if std::io::Read::read_to_end(&mut x.into_reader(), &mut body).is_err() {
                return bad("upstream read failed".into());
            }
            // keep only complete trees whose content matches their id, and reuse the
            // authorization only for callers who saw the tree complete
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                if v["complete"] == true {
                    e.remember(&h, &object);
                    keep_tree(&file, &id, &v, &body);
                }
            }
            return ([("content-type", "application/json"), ("x-cambium-edge", "miss")], body).into_response();
        }
    }
    match upstream_call(e, &method, &path_q, &h, range.as_deref(), &body) {
        Ok(x) => to_response(x, None),
        Err(m) => bad(m),
    }
}

/// Keep the cache under `cap` bytes: when it is over, remove the oldest objects
/// (by the time they were cached) until it is under 90% of the cap.
fn evict(dirs: &[std::path::PathBuf], cap: u64) -> (u64, usize) {
    let mut files = vec![];
    let mut walk = dirs.to_vec();
    while let Some(d) = walk.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for f in rd.flatten() {
            let Ok(m) = f.metadata() else { continue };
            if m.is_dir() {
                walk.push(f.path());
            } else {
                files.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), f.path()));
            }
        }
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= cap {
        return (total, 0);
    }
    files.sort();
    let (target, mut removed) = (cap / 10 * 9, 0);
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            removed += 1;
        }
    }
    (total, removed)
}

fn app_tier_with(upstream: String, cache_dir: String, tier: &'static str) -> (Router, Arc<Edge>) {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(std::time::Duration::from_secs(300)).build();
    let trees = std::path::PathBuf::from(format!("{cache_dir}-trees"));
    std::fs::create_dir_all(&trees).expect("tree cache dir");
    let ttl = std::env::var("CAMBIUM_EDGE_AUTHZ_TTL").ok().and_then(|v| v.parse().ok()).unwrap_or(60u64);
    let e = Arc::new(Edge {
        upstream: upstream.trim_end_matches('/').to_string(),
        agent,
        store: FsBlobStore::new(&cache_dir).expect("edge cache dir"),
        trees,
        authz: Default::default(),
        authz_ttl: std::time::Duration::from_secs(ttl),
        tier,
        draining: Default::default(),
    });
    let cap_mb: u64 = std::env::var("CAMBIUM_EDGE_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if cap_mb > 0 {
        let dirs = vec![std::path::PathBuf::from(&cache_dir), e.trees.clone()];
        std::thread::spawn(move || loop {
            let (bytes, removed) = evict(&dirs, cap_mb * 1024 * 1024);
            if removed > 0 {
                cambium_core::log::info("cache trimmed", serde_json::json!({"removed": removed, "bytes": bytes, "cap_mb": cap_mb}));
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
    }
    let e2 = e.clone();
    let router = Router::new().fallback(move |req: Request| {
        let e = e.clone();
        async move {
            let (parts, body) = req.into_parts();
            let Ok(body) = axum::body::to_bytes(body, 64 * 1024 * 1024).await else {
                return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response();
            };
            let pq = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
            let m = parts.method.as_str().to_string();
            tokio::task::spawn_blocking(move || handle(&e, m, pq, parts.headers, body))
                .await
                .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "edge worker failed").into_response())
        }
    });
    (router, e2)
}

/// `cmbd edge` (regional tier) or `cmbd daemon` (per-machine tier): same code, the
/// daemon just defaults to a loopback address and its own cache directory.
pub fn run(tier: &'static str) {
    let upstream =
        std::env::var("CAMBIUM_UPSTREAM").unwrap_or_else(|_| panic!("cmbd {tier} needs CAMBIUM_UPSTREAM (the next tier's base URL)"));
    let (default_data, default_addr) =
        if tier == "daemon" { ("./cambium-daemon", "127.0.0.1:7880") } else { ("./cambium-edge", "127.0.0.1:7879") };
    let data = std::env::var("CAMBIUM_DATA").unwrap_or_else(|_| default_data.into());
    let dir = format!("{data}/edge-blobs");
    let addr = std::env::var("CAMBIUM_ADDR").unwrap_or_else(|_| default_addr.into());
    tokio::runtime::Runtime::new().expect("runtime").block_on(async move {
        let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
        cambium_core::log::info(
            &format!("cmbd {tier} listening"),
            serde_json::json!({"addr": addr.to_string(), "upstream": upstream.to_string(), "cache": dir.to_string()}),
        );
        let (app, e) = app_tier_with(upstream, dir, tier);
        let drain =
            std::time::Duration::from_secs(std::env::var("CAMBIUM_SHUTDOWN_DRAIN_S").ok().and_then(|v| v.parse().ok()).unwrap_or(5));
        axum::serve(l, app)
            .with_graceful_shutdown(async move {
                crate::shutdown_signal().await;
                // fail /readyz first so the Service stops sending new requests here
                e.draining.store(true, std::sync::atomic::Ordering::Relaxed);
                cambium_core::log::info(
                    &format!("cmbd {tier}: shutdown requested; draining"),
                    serde_json::json!({"drain_s": drain.as_secs()}),
                );
                tokio::time::sleep(drain).await;
            })
            .await
            .unwrap();
        cambium_core::log::info(&format!("cmbd {tier} stopped cleanly"), serde_json::json!({}));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[test]
    fn eviction_removes_oldest_until_under_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("ab");
        std::fs::create_dir_all(&sub).unwrap();
        let t0 = std::time::SystemTime::now() - std::time::Duration::from_secs(1000);
        for i in 0..10u64 {
            let f = sub.join(format!("f{i}"));
            std::fs::write(&f, vec![0u8; 1000]).unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(t0 + std::time::Duration::from_secs(i)).unwrap();
        }
        assert_eq!(evict(&[d.path().to_path_buf()], 20_000), (10_000, 0), "under the cap: nothing removed");
        let (bytes, removed) = evict(&[d.path().to_path_buf()], 5_000);
        assert_eq!((bytes, removed), (4_000, 6), "down to 90% of the cap");
        for i in 0..6 {
            assert!(!sub.join(format!("f{i}")).exists(), "oldest removed first");
        }
        assert!(sub.join("f9").exists());
    }

    #[tokio::test]
    async fn health_is_local_and_readiness_follows_the_upstream() {
        let d = tempfile::tempdir().unwrap();
        // nothing listens on this port: the upstream is down
        let (app, e) = app_tier_with("http://127.0.0.1:9".into(), d.path().join("c").to_string_lossy().into(), "edge");
        let get = |p: &str| axum::http::Request::get(p).body(axum::body::Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(get("/healthz")).await.unwrap().status(), StatusCode::OK);
        assert_eq!(app.clone().oneshot(get("/readyz")).await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        e.draining.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = app.oneshot(get("/readyz")).await.unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
