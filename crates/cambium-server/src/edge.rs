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
            e.remember(&h, &object);
            // keep only complete trees whose content matches their id
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                let entries = serde_json::from_value::<Vec<cambium_core::tree::TreeEntry>>(v["entries"].clone());
                if v["complete"] == true && entries.is_ok_and(|en| cambium_core::tree::Tree { entries: en }.id() == id) {
                    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
                    if std::fs::write(&tmp, &body).is_ok() {
                        let _ = std::fs::rename(&tmp, &file);
                    }
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

fn app_tier(upstream: String, cache_dir: String, tier: &'static str) -> Router {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(std::time::Duration::from_secs(300)).build();
    let trees = std::path::PathBuf::from(format!("{cache_dir}-trees"));
    std::fs::create_dir_all(&trees).expect("tree cache dir");
    let ttl = std::env::var("CAMBIUM_EDGE_AUTHZ_TTL").ok().and_then(|v| v.parse().ok()).unwrap_or(60u64);
    let e = Arc::new(Edge {
        upstream: upstream.trim_end_matches('/').to_string(),
        agent,
        store: FsBlobStore::new(cache_dir).expect("edge cache dir"),
        trees,
        authz: Default::default(),
        authz_ttl: std::time::Duration::from_secs(ttl),
        tier,
    });
    Router::new().fallback(move |req: Request| {
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
    })
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
        axum::serve(l, app_tier(upstream, dir, tier)).await.unwrap();
    });
}
