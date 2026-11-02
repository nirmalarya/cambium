//! Regional read-through cache ("edge"): `cmbd edge` sits near a group of users
//! and forwards every request to the primary (CAMBIUM_UPSTREAM). Blob downloads are
//! served from a local content-addressed copy after the first fetch.
//!
//! Permissions stay with the primary: before a cached blob is served, the edge
//! repeats the caller's request upstream for a single byte (`Range: bytes=0-0`)
//! with the caller's own credentials, so the primary still decides who may read
//! it and only the bulk transfer is saved. Cached bytes are checked against the
//! blob id (a SHA-256 of the content) before they are stored.
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
    let bad = |m: String| (StatusCode::BAD_GATEWAY, m).into_response();
    let path = path_q.split('?').next().unwrap_or("").to_string();
    let range = h.get("range").and_then(|v| v.to_str().ok()).map(String::from);
    if method == "GET" {
        if let Some(id) = blob_id(&path) {
            if let Ok(Some(data)) = e.store.get(id) {
                // cached: let the primary decide whether this caller may read it
                let probe = match upstream_call(e, "GET", &path_q, &h, Some("bytes=0-0"), &[]) {
                    Ok(x) => x,
                    Err(m) => return bad(m),
                };
                if !matches!(probe.status(), 200 | 206 | 416) {
                    return to_response(probe, Some("denied"));
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
            return serve_local(data, range.as_deref(), "miss");
        }
    }
    match upstream_call(e, &method, &path_q, &h, range.as_deref(), &body) {
        Ok(x) => to_response(x, None),
        Err(m) => bad(m),
    }
}

pub fn app(upstream: String, cache_dir: String) -> Router {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(std::time::Duration::from_secs(300)).build();
    let e = Arc::new(Edge {
        upstream: upstream.trim_end_matches('/').to_string(),
        agent,
        store: FsBlobStore::new(cache_dir).expect("edge cache dir"),
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

pub fn run() {
    let upstream = std::env::var("CAMBIUM_UPSTREAM").unwrap_or_else(|_| panic!("cmbd edge needs CAMBIUM_UPSTREAM (the primary's base URL)"));
    let data = std::env::var("CAMBIUM_DATA").unwrap_or_else(|_| "./cambium-edge".into());
    let dir = format!("{data}/edge-blobs");
    let addr = std::env::var("CAMBIUM_ADDR").unwrap_or_else(|_| "127.0.0.1:7879".into());
    tokio::runtime::Runtime::new().expect("runtime").block_on(async move {
        let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
        cambium_core::log::info(
            "cmbd edge listening",
            serde_json::json!({"addr": addr.to_string(), "upstream": upstream.to_string(), "cache": dir.to_string()}),
        );
        axum::serve(l, app(upstream, dir)).await.unwrap();
    });
}
