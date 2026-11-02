//! Webhook API and delivery loop. Each cmbd node runs the loop; per-hook leases decide
//! which node delivers (see cambium_core::webhooks). Deliveries are POSTs of JSON with
//!   X-Cambium-Event: ref
//!   X-Cambium-Delivery: <hook id>-<wal seq>     (stable across retries)
//!   X-Cambium-Signature: sha256=<HMAC-SHA256(secret, body)>
//! Loopback, link-local and cloud-metadata addresses are refused unless
//! CAMBIUM_WEBHOOK_ALLOW_LOCAL=1 (private networks are fine: CI is usually internal).
use crate::{user, ApiErr, St, R};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    routing::{delete, get},
    Json, Router,
};
use cambium_core::{Engine, Error};
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Arc;

pub fn api() -> Router<St> {
    Router::new().route("/v1/repos/:r/hooks", get(list).post(add)).route("/v1/repos/:r/hooks/:id", delete(remove))
}

#[derive(Deserialize)]
struct NewHook {
    url: String,
    secret: String,
}
async fn add(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<NewHook>) -> R {
    let u = user(&st, &h)?;
    target_allowed(&b.url).map_err(|e| ApiErr(Error::Bad(e)))?;
    let id = st.add_hook(&r, &u, &b.url, &b.secret)?;
    Ok(Json(json!({"id": id})))
}
async fn list(State(st): State<St>, h: HeaderMap, Path(r): Path<String>) -> R {
    let u = user(&st, &h)?;
    Ok(Json(json!(st.list_hooks(&r, &u)?)))
}
async fn remove(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>) -> R {
    let u = user(&st, &h)?;
    st.remove_hook(&r, &u, id)?;
    Ok(Json(json!({"removed": id})))
}

fn local_ok() -> bool {
    std::env::var("CAMBIUM_WEBHOOK_ALLOW_LOCAL").is_ok_and(|v| v == "1")
}
fn forbidden_v4(v: std::net::Ipv4Addr) -> bool {
    v.is_loopback()
        || v.is_link_local()
        || v.octets()[0] == 0
        || v.is_multicast()
        || v.is_broadcast()
        || v == std::net::Ipv4Addr::new(100, 100, 100, 200) // Alibaba Cloud metadata
}
fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => forbidden_v4(v),
        IpAddr::V6(v) => {
            let s = v.segments();
            // IPv4 hidden inside IPv6: mapped (::ffff:a.b.c.d), compatible (::a.b.c.d), NAT64 (64:ff9b::/96)
            let embedded = v.to_ipv4_mapped().or_else(|| {
                ((s[..6] == [0, 0, 0, 0, 0, 0] && !v.is_loopback() && !v.is_unspecified()) || s[..6] == [0x64, 0xff9b, 0, 0, 0, 0])
                    .then(|| std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8))
            });
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xffc0) == 0xfe80
                || v == std::net::Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) // AWS metadata (IPv6)
                || embedded.is_some_and(forbidden_v4)
        }
    }
}
/// Resolver for deliveries: the addresses checked are exactly the ones connected to,
/// so DNS rebinding between a check and the request cannot reach blocked addresses.
struct SafeResolver;
impl ureq::Resolver for SafeResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        let addrs: Vec<_> = netloc.to_socket_addrs()?.collect();
        if !local_ok() && addrs.iter().any(|a| forbidden_ip(a.ip())) {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "blocked address"));
        }
        Ok(addrs)
    }
}
/// Error text safe to store and show: a category, never connection details (which would
/// make webhooks a port scanner).
fn category(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) if t.to_string().contains("blocked address") => "blocked address".into(),
        ureq::Error::Transport(_) => "unreachable".into(),
    }
}
/// Refuse targets that resolve to loopback/link-local (e.g. 169.254.169.254, the
/// cloud metadata service) unless explicitly allowed.
pub(crate) fn target_allowed(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).ok_or("webhook url must be http(s)://...")?;
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    if hostport.contains('@') {
        return Err("webhook url must not contain credentials".into());
    }
    if local_ok() {
        return Ok(());
    }
    let default = if url.starts_with("https://") { 443 } else { 80 };
    let with_port = match (hostport.starts_with('['), hostport.contains("]:"), hostport.contains(':')) {
        (true, true, _) | (false, _, true) => hostport.to_string(), // [v6]:port or host:port
        _ => format!("{hostport}:{default}"),                       // [v6] or host
    };
    let addrs: Vec<_> = with_port.to_socket_addrs().map_err(|e| format!("cannot resolve {hostport}: {e}"))?.collect();
    if addrs.is_empty() || addrs.iter().any(|a| forbidden_ip(a.ip())) {
        return Err(format!("{hostport} resolves to a loopback/link-local address; set CAMBIUM_WEBHOOK_ALLOW_LOCAL=1 to allow"));
    }
    Ok(())
}

/// Start the delivery loop on a background thread (CAMBIUM_WEBHOOKS=0 disables it).
pub fn start(eng: Arc<Engine>) {
    if std::env::var("CAMBIUM_WEBHOOKS").is_ok_and(|v| v == "0") {
        return;
    }
    let node = format!("{}-{:x}", std::process::id(), rand::random::<u64>());
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(std::time::Duration::from_secs(10)).resolver(SafeResolver).build();
    let busy: Arc<std::sync::Mutex<std::collections::HashSet<i64>>> = Default::default();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        // lease 120 s > worst case per pass (5 events x 10 s timeout)
        let Ok(hooks) = eng.claim_hooks(&node, 120) else { continue };
        for (id, repo, url, secret, last) in hooks {
            // one thread per hook: a slow receiver only delays its own repository
            if !busy.lock().unwrap().insert(id) {
                continue;
            }
            let (eng, agent, busy) = (eng.clone(), agent.clone(), busy.clone());
            std::thread::spawn(move || {
                deliver(&eng, &agent, id, &repo, &url, &secret, last);
                busy.lock().unwrap().remove(&id);
            });
        }
    });
}

fn deliver(eng: &Engine, agent: &ureq::Agent, id: i64, repo: &str, url: &str, secret: &str, mut last: i64) {
    for _ in 0..5 {
        let Ok(Some((seq, payload))) = eng.next_ref_event(repo, last) else { break };
        let body = payload.to_string();
        let sig = cambium_core::webhooks::hmac_sha256_hex(secret.as_bytes(), body.as_bytes());
        let res = agent
            .post(url)
            .set("content-type", "application/json")
            .set("user-agent", "cambium-webhook")
            .set("x-cambium-event", "ref")
            .set("x-cambium-delivery", &format!("{id}-{seq}"))
            .set("x-cambium-signature", &format!("sha256={sig}"))
            .send_string(&body);
        match res {
            Ok(_) => {
                let _ = eng.hook_delivered(id, seq);
                last = seq;
            }
            Err(e) => {
                let why = category(&e);
                cambium_core::log::error("webhook delivery failed", json!({"hook": id, "seq": seq, "error": why}));
                let _ = eng.hook_failed(id, &why);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn metadata_and_loopback_targets_refused() {
        std::env::remove_var("CAMBIUM_WEBHOOK_ALLOW_LOCAL");
        for u in [
            "http://169.254.169.254/latest/meta-data",
            "http://127.0.0.1:9000/x",
            "http://localhost/hook",
            "http://[::1]:80/",
            "http://user:pw@10.0.0.1/",
        ] {
            assert!(super::target_allowed(u).is_err(), "{u}");
        }
        assert!(super::target_allowed("http://10.1.2.3:8080/ci").is_ok(), "private CI addresses are fine");
        assert!(super::target_allowed("ftp://x").is_err());
        for ip in ["100.100.100.200", "0.1.2.3", "fd00:ec2::254", "::127.0.0.1", "64:ff9b::a9fe:a9fe", "::ffff:169.254.169.254"] {
            assert!(super::forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["10.1.2.3", "192.168.1.5", "2001:db8::1", "fd12::1"] {
            assert!(!super::forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
