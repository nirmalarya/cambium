// ureq::Error and axum Response are large by design; these paths are network-bound.
#![allow(clippy::result_large_err)]
//! cmbd: the Cambium server. All version-control logic runs here.
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum::{
    extract::Request,
    middleware::{self, Next},
};
use cambium_core::limits::{parse_range, RateLimiter};
use cambium_core::{
    blob::{BlobStore, FsBlobStore},
    engine::{split_entry, Manifest},
    Engine, Error, FileChange,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

mod admin;
mod auth;
mod browse;
mod edge;
mod hooks;
mod obs;
mod review;
mod settings;
use cambium_core::{Perm, Principal};

/// Shared server state: the engine plus the optional identity provider.
pub struct Ctx {
    pub eng: Arc<Engine>,
    pub oidc: Option<auth::Oidc>,
}
impl std::ops::Deref for Ctx {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.eng
    }
}
pub(crate) type St = Arc<Ctx>;
pub(crate) struct ApiErr(pub(crate) Error);
impl From<Error> for ApiErr {
    fn from(e: Error) -> Self {
        ApiErr(e)
    }
}
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let (code, kind) = match &self.0 {
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::RefMoved { .. } => (StatusCode::CONFLICT, "ref_moved"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "merge_conflict"),
            Error::Bad(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Error::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Error::Busy(_) => (StatusCode::CONFLICT, "busy"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        // storage and I/O details stay in the server log, not in the response
        let message = if code == StatusCode::INTERNAL_SERVER_ERROR {
            cambium_core::log::error("internal error", json!({"error": self.0.to_string()}));
            "internal error; see the server log".to_string()
        } else {
            self.0.to_string()
        };
        let mut body = json!({"error": kind, "message": message});
        if let Error::Conflict(p) = &self.0 {
            body["paths"] = json!(p);
        }
        if let Error::RefMoved { found, .. } = &self.0 {
            body["found"] = json!(found);
        }
        if matches!(self.0, Error::Busy(_)) {
            // transient: clients (cmb included) retry automatically
            return (code, [("retry-after", "1")], Json(body)).into_response();
        }
        (code, Json(body)).into_response()
    }
}
pub(crate) type R = Result<Json<Value>, ApiErr>;
/// Conflict paths the caller cannot read are replaced by a count, so a failed merge
/// still explains itself without naming hidden files.
pub(crate) fn hide_conflicts(e: Error, az: &cambium_core::Authz) -> Error {
    match e {
        Error::Conflict(paths) => {
            let n = paths.len();
            let mut shown: Vec<String> = paths.into_iter().filter(|p| az.can(p, Perm::Read)).collect();
            if shown.len() < n {
                shown.push(format!("({} more you cannot read)", n - shown.len()));
            }
            Error::Conflict(shown)
        }
        e => e,
    }
}

pub(crate) fn user(st: &St, h: &HeaderMap) -> Result<Principal, ApiErr> {
    let t =
        h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).ok_or(ApiErr(Error::Unauthorized))?;
    let p = auth::authenticate(&st.eng, st.oidc.as_ref(), t)?;
    obs::set_actor(&p.user);
    Ok(p)
}
/// Whether cmbd terminates TLS itself (CAMBIUM_TLS_CERT + CAMBIUM_TLS_KEY).
pub(crate) fn tls_enabled() -> bool {
    std::env::var("CAMBIUM_TLS_CERT").is_ok() && std::env::var("CAMBIUM_TLS_KEY").is_ok()
}
fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
pub(crate) fn need(ok: bool, what: &str) -> Result<(), ApiErr> {
    if ok {
        Ok(())
    } else {
        Err(ApiErr(Error::Forbidden(what.into())))
    }
}
/// Protected branches change only through an approved review.
fn guard_protected(st: &St, r: &str, name: &str) -> Result<(), ApiErr> {
    let n = st.required_approvals(r, name)?;
    if n > 0 {
        return Err(ApiErr(Error::Forbidden(format!(
            "{name} is protected ({n} approval(s) required): open a review with `cmb review create`"
        ))));
    }
    Ok(())
}
/// Every file needs write access. The error names only paths the caller can read;
/// the rest are counted, so a refusal never reveals hidden file names.
pub(crate) fn check_writes(az: &cambium_core::Authz, files: &[FileChange]) -> cambium_core::engine::Result<()> {
    let denied: Vec<&str> = files.iter().filter(|f| !az.can(&f.path, Perm::Write)).map(|f| f.path.as_str()).collect();
    if denied.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = denied.iter().copied().filter(|p| az.can(p, Perm::Read)).take(5).collect();
    let hidden = denied.iter().filter(|p| !az.can(p, Perm::Read)).count();
    let mut what = shown.join(", ");
    if hidden > 0 {
        what = if what.is_empty() {
            format!("{hidden} path(s) you cannot read")
        } else {
            format!("{what} and {hidden} path(s) you cannot read")
        };
    }
    Err(Error::Forbidden(format!("write on {what}")))
}
/// Hide files the caller may not read; drop changesets that only touched such files.
pub(crate) fn redact(az: &cambium_core::Authz, mut cs: cambium_core::Changeset) -> Option<cambium_core::Changeset> {
    if az.open() {
        return Some(cs);
    }
    let had = !cs.files.is_empty();
    cs.files.retain(|f| az.can(&f.path, Perm::Read));
    if had && cs.files.is_empty() {
        None
    } else {
        Some(cs)
    }
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
}
/// CAMBIUM_REPO_CREATORS="group:platform,user:alice" limits who may create repositories
/// (the creator becomes the repo's admin); unset or empty = any signed-in user.
pub(crate) fn may_create_repo(u: &Principal) -> bool {
    creator_allowed(&std::env::var("CAMBIUM_REPO_CREATORS").unwrap_or_default(), u)
}
fn creator_allowed(list: &str, u: &Principal) -> bool {
    let list: Vec<&str> = list.split(',').map(str::trim).filter(|x| !x.is_empty()).collect();
    list.is_empty()
        || list.iter().any(|e| {
            e.strip_prefix("user:") == Some(u.user.as_str()) || e.strip_prefix("group:").is_some_and(|g| u.groups.iter().any(|x| x == g))
        })
}
async fn create_repo(State(st): State<St>, h: HeaderMap, Json(b): Json<NewRepo>) -> R {
    let u = user(&st, &h)?;
    need(may_create_repo(&u), "permission to create repositories (CAMBIUM_REPO_CREATORS)")?;
    let root = st.create_repo(&b.name, &u.user)?;
    Ok(Json(json!({"repo": b.name, "main": root})))
}
async fn refs(State(st): State<St>, h: HeaderMap, Path(r): Path<String>) -> R {
    let u = user(&st, &h)?;
    need(st.authz(&r, &u)?.can_any(Perm::Read), "read on this repo")?;
    let v: Vec<Value> = st.list_refs(&r)?.into_iter().map(|(n, c, v)| json!({"name":n,"cs":c,"version":v})).collect();
    Ok(Json(json!(v)))
}
#[derive(Deserialize)]
struct Cas {
    expected: Option<String>,
    new: Option<String>,
}
/// Creating a ref needs read access; moving one needs write on every path that
/// differs between the old and new target; deleting `main` needs admin.
async fn cas(State(st): State<St>, h: HeaderMap, Path((r, name)): Path<(String, String)>, Json(b): Json<Cas>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    let new = match &b.new {
        Some(n) => Some(st.resolve(&r, n)?),
        None => None,
    };
    if b.expected.is_none() && !cambium_core::engine::valid_ref_name(&name) {
        return Err(ApiErr(Error::Bad(format!(
            "ref {name:?} not allowed: only main and release/<name> (work moves as changesets and reviews)"
        ))));
    }
    if b.expected.is_some() {
        guard_protected(&st, &r, &name)?;
    }
    if !az.open() {
        match (&b.expected, &new) {
            (None, target) => {
                // a new release branch: writers only, and only at history already on main
                // (otherwise anyone's unreviewed draft could reach release pipelines)
                need(az.can_any(Perm::Write), "write in this repo to create a release branch")?;
                if !az.can("", Perm::Admin) {
                    let main = st.get_ref(&r, "main")?;
                    let on_main = match (target, main) {
                        (Some(t), Some(m)) => st.is_ancestor(&r, t, &m)?,
                        _ => false,
                    };
                    need(on_main, "a release branch must start at a changeset already on main")?;
                }
            }
            (Some(old), None) => {
                // deleting a branch discards its work: admins, or whoever may write every
                // path where it differs from main
                if !az.can("", Perm::Admin) {
                    need(name != "main", "admin on / to delete main")?;
                    let mb = match st.get_ref(&r, "main")? {
                        Some(m) => st.manifest(&r, &m)?,
                        None => Default::default(),
                    };
                    let mo = st.manifest(&r, &st.resolve(&r, old)?)?;
                    let diff = Engine::changes_between(&mb, &mo);
                    need(az.can_any(Perm::Write), "permission to delete this ref")?;
                    check_writes(&az, &diff)?;
                }
            }
            (Some(old), Some(n)) => {
                let (mo, mn) = (st.manifest(&r, &st.resolve(&r, old)?)?, st.manifest(&r, n)?);
                check_writes(&az, &Engine::changes_between(&mo, &mn))?;
            }
        }
    }
    st.cas_ref(&r, &name, b.expected.as_deref(), new.as_deref())?;
    Ok(Json(json!({"ref": name, "cs": new})))
}
async fn resolve(State(st): State<St>, h: HeaderMap, Path((r, rev)): Path<(String, String)>) -> R {
    let u = user(&st, &h)?;
    need(st.authz(&r, &u)?.can_any(Perm::Read), "read on this repo")?;
    Ok(Json(json!({"cs": st.resolve(&r, &rev)?})))
}
async fn put_blob(State(st): State<St>, h: HeaderMap, body: Bytes) -> R {
    let u = user(&st, &h)?;
    Ok(Json(json!({"id": st.put_blob_as(&body, &u)?})))
}
/// Whole blob, or one byte range of it (`Range: bytes=N-`) so an interrupted fetch can resume.
async fn get_blob(State(st): State<St>, h: HeaderMap, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let u = user(&st, &h)?;
    // same answer for "missing" and "not yours", so ids of hidden files are not confirmed
    if !st.blob_readable(&id, &u)? {
        return Err(ApiErr(Error::NotFound(format!("blob {id}"))));
    }
    let data = st.blobs.get(&id).map_err(Error::from)?.ok_or(ApiErr(Error::NotFound(format!("blob {id}"))))?;
    let len = data.len() as u64;
    let range = h.get("range").and_then(|v| v.to_str().ok());
    let Some(r) = range else {
        return Ok(([("accept-ranges", "bytes".to_string()), ("content-length", len.to_string())], data).into_response());
    };
    match parse_range(r, len) {
        Ok((a, b)) => Ok((
            StatusCode::PARTIAL_CONTENT,
            [("accept-ranges", "bytes".to_string()), ("content-range", format!("bytes {a}-{b}/{len}"))],
            data[a as usize..=b as usize].to_vec(),
        )
            .into_response()),
        Err(()) => Ok((StatusCode::RANGE_NOT_SATISFIABLE, [("content-range", format!("bytes */{len}"))]).into_response()),
    }
}

/// Request limiting. Authenticated callers get one bucket per user (however many
/// tokens they hold); requests without a valid credential share one bucket per
/// client IP, so made-up tokens cannot buy fresh budget and floods of bad
/// credentials are refused here, before they reach auth or the audit log.
/// Over the limit -> 429 with Retry-After in whole seconds.
async fn limit(State((st, l)): State<(St, Arc<RateLimiter>)>, req: Request, next: Next) -> Response {
    if matches!(req.uri().path(), "/v1/health" | "/healthz" | "/readyz" | "/metrics") || !l.enabled() {
        return next.run(req).await;
    }
    let h = req.headers();
    let who = match h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        Some(t) => auth::authenticate(&st.eng, st.oidc.as_ref(), t).ok(),
        None => review::cookie_token(h).and_then(|t| st.session_principal(&t).ok()),
    };
    let key = match who {
        Some(p) => format!("user:{}", p.user),
        None => obs::client_ip(&req).map(cambium_core::limits::ip_key).unwrap_or_else(|| "ip:unknown".into()),
    };
    match l.check(&key) {
        Ok(()) => next.run(req).await,
        Err(wait) => {
            obs::metrics().rate_limited.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", secs.to_string())],
                Json(json!({"error": "rate_limited", "message": format!("too many requests; retry in {secs}s"), "retry_after": secs})),
            )
                .into_response()
        }
    }
}
#[derive(Deserialize)]
struct Commit {
    r#ref: String,
    expected: String,
    message: String,
    files: Vec<FileChange>,
}
/// The caller may only reference blobs they can read or uploaded themselves. A hidden
/// blob gets the same answer as a missing one, so its id is not confirmed.
fn need_blob(st: &St, id: &str, u: &Principal) -> Result<(), ApiErr> {
    if st.blob_usable(id, u)? {
        Ok(())
    } else {
        Err(ApiErr(Error::Bad(format!("missing blob {id}"))))
    }
}
async fn commit(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Commit>) -> R {
    let u = user(&st, &h)?;
    check_writes(&st.authz(&r, &u)?, &b.files)?;
    for f in &b.files {
        if let Some(id) = &f.blob {
            need_blob(&st, id, &u)?;
        }
    }
    guard_protected(&st, &r, &b.r#ref)?;
    Ok(Json(json!(st.commit(&r, &b.r#ref, &b.expected, &u.user, &b.message, b.files)?)))
}
#[derive(Deserialize)]
struct Tree {
    rev: String,
    #[serde(default)]
    prefixes: Vec<String>,
    #[serde(default)]
    sizes: bool,
}
async fn tree(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Tree>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    let cs = st.resolve(&r, &b.rev)?;
    let entries = az.filter(st.tree(&r, &cs, &b.prefixes)?);
    if !b.sizes {
        return Ok(Json(json!({"cs": cs, "entries": entries})));
    }
    // sizes let a lazy (virtual) checkout list files without downloading them
    let mut sizes = serde_json::Map::new();
    for (p, e) in &entries {
        sizes.insert(p.clone(), json!(st.blob_size(split_entry(e).0)?));
    }
    Ok(Json(json!({"cs": cs, "entries": entries, "sizes": sizes})))
}
#[derive(Deserialize)]
struct Merge {
    target: String,
    source: String,
}
async fn merge(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Merge>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    guard_protected(&st, &r, &b.target)?;
    Ok(Json(json!(st.merge_checked(&r, &b.target, &b.source, &u.user, &|f| check_writes(&az, f)).map_err(|e| hide_conflicts(e, &az))?)))
}
#[derive(Deserialize)]
struct Rebase {
    cs: String,
    onto: String,
}
/// Rebase changeset `cs` (and the drafts under it) onto revision `onto` and return the
/// new tip. Moves no ref: used to bring drafts up to date before review or landing.
async fn rebase(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Rebase>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    let (cs, onto) = (st.resolve(&r, &b.cs)?, st.resolve(&r, &b.onto)?);
    Ok(Json(json!({"cs": st.rebase_onto(&r, &cs, &onto, &|f| check_writes(&az, f)).map_err(|e| hide_conflicts(e, &az))?})))
}
/// Tree fetch, step 1: the changeset a revision names and its root tree.
async fn root(State(st): State<St>, h: HeaderMap, Path((r, rev)): Path<(String, String)>) -> R {
    let u = user(&st, &h)?;
    need(st.authz(&r, &u)?.can_any(Perm::Read), "read on this repo")?;
    let cs = st.resolve(&r, &rev)?;
    Ok(Json(json!({"cs": cs, "root": st.root_tree(&r, &cs)?})))
}
#[derive(Deserialize)]
struct TreeQ {
    #[serde(default)]
    path: String,
}
/// Tree fetch, step 2: one directory. Served only where it occurs (`path`) and
/// if the caller may see that directory; entries the caller cannot read are left out
/// (then `complete` is false and the content no longer hashes to `id`). Immutable, so
/// any cache may keep it.
async fn get_tree(
    State(st): State<St>,
    h: HeaderMap,
    Path((r, id)): Path<(String, String)>,
    Query(q): Query<TreeQ>,
) -> Result<Response, ApiErr> {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    let Some(v) = serve_tree(&st, &az, &r, &id, &q.path)? else { return Err(ApiErr(Error::NotFound(format!("tree {id}")))) };
    let complete = v["complete"] == true;
    Ok(([("cache-control", if complete { "private, max-age=31536000, immutable" } else { "private, no-store" })], Json(v)).into_response())
}
/// One tree for this caller, or None ("absent" and "not yours" look the same).
fn serve_tree(st: &St, az: &cambium_core::Authz, r: &str, id: &str, path: &str) -> Result<Option<Value>, ApiErr> {
    let dir = path.trim_matches('/').to_string();
    if !(dir.is_empty() || cambium_core::engine::valid_path(&dir)) || !az.can_see_dir(&dir) || !st.tree_at(r, id, &dir)? {
        return Ok(None);
    }
    let t = st.get_tree(id)?;
    let n = t.entries.len();
    let entries: Vec<_> = t
        .entries
        .into_iter()
        .filter(|e| {
            let p = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
            if e.kind == "tree" {
                az.can_see_dir(&p)
            } else {
                az.can(&p, Perm::Read)
            }
        })
        .collect();
    let complete = entries.len() == n;
    Ok(Some(json!({"id": id, "path": dir, "complete": complete, "entries": entries})))
}
#[derive(Deserialize)]
struct TreeItem {
    id: String,
    #[serde(default)]
    path: String,
}
#[derive(Deserialize)]
struct TreeBatch {
    items: Vec<TreeItem>,
}
/// Many trees in one request (one directory level of a walk), each checked exactly like
/// a single tree fetch. Missing or hidden ones come back as {"id","path","error":"not_found"}.
async fn get_trees(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<TreeBatch>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    if b.items.len() > 2000 {
        return Err(ApiErr(Error::Bad("at most 2000 trees per request".into())));
    }
    let mut out = Vec::with_capacity(b.items.len());
    for it in &b.items {
        out.push(
            serve_tree(&st, &az, &r, &it.id, &it.path)?.unwrap_or_else(|| json!({"id": it.id, "path": it.path, "error": "not_found"})),
        );
    }
    Ok(Json(json!({"trees": out})))
}
#[derive(Deserialize)]
struct BisectQ {
    good: String,
    bad: String,
}
/// One stateless bisection step on first-parent history (for bisect pipelines).
async fn bisect(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(q): Query<BisectQ>) -> R {
    let u = user(&st, &h)?;
    need(st.authz(&r, &u)?.can_any(Perm::Read), "read on this repo")?;
    Ok(Json(json!(st.bisect(&r, &q.good, &q.bad)?)))
}
#[derive(Deserialize)]
struct Revert {
    cs: String,
    #[serde(default = "main_ref")]
    onto: String,
}
/// Undo changeset `cs` on top of the current head of `onto` and land it (for revert
/// pipelines). Protected refs only change through a review.
async fn revert(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Revert>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    guard_protected(&st, &r, &b.onto)?;
    let tip = st.revert(&r, &b.cs, &b.onto, &u.user, &|f| check_writes(&az, f)).map_err(|e| hide_conflicts(e, &az))?;
    Ok(Json(json!({"ref": b.onto, "cs": tip})))
}
#[derive(Deserialize)]
struct NewChangeset {
    parent: String,
    message: String,
    files: Vec<FileChange>,
}
/// Create a changeset on `parent` without touching any ref (a draft). Land it with
/// `/land` or open a review for it.
async fn create_changeset(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<NewChangeset>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    check_writes(&az, &b.files)?;
    for f in &b.files {
        if let Some(id) = &f.blob {
            need_blob(&st, id, &u)?;
        }
    }
    let parent = st.resolve(&r, &b.parent)?;
    Ok(Json(json!(st.create_changeset(&r, vec![parent], &u.user, &b.message, b.files)?)))
}
#[derive(Deserialize)]
struct Land {
    cs: String,
    #[serde(default = "main_ref")]
    onto: String,
}
fn main_ref() -> String {
    "main".into()
}
/// Land changeset `cs` on ref `onto`: server-side rebase onto the current head plus a
/// compare-and-swap, retried if someone else landed first. Protected refs only change
/// through an approved review.
async fn land(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Land>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    guard_protected(&st, &r, &b.onto)?;
    let tip = st.land(&r, &b.cs, &b.onto, &|f| check_writes(&az, f)).map_err(|e| hide_conflicts(e, &az))?;
    Ok(Json(json!({"ref": b.onto, "cs": tip})))
}
#[derive(Deserialize)]
struct LogQ {
    rev: String,
    path: Option<String>,
    limit: Option<usize>,
}
async fn log(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(q): Query<LogQ>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    if let Some(p) = q.path.as_deref().filter(|p| !p.is_empty()) {
        // same answer for hidden and missing paths
        need(az.can(p, Perm::Read) || az.can_see_dir(p), "read on this path")?;
    }
    let cs = st.resolve(&r, &q.rev)?;
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    // over-fetch when redacting so the caller still gets up to `limit` entries
    let raw = st.log(&r, &cs, q.path.as_deref(), if az.open() { limit } else { limit.saturating_mul(10).max(limit) })?;
    let v: Vec<_> = raw.into_iter().filter_map(|c| redact(&az, c)).take(limit).collect();
    Ok(Json(json!(v)))
}
#[derive(Deserialize)]
struct BlameQ {
    rev: String,
    path: String,
}
async fn blame(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(q): Query<BlameQ>) -> R {
    let u = user(&st, &h)?;
    st.authz(&r, &u)?.require(&q.path, Perm::Read)?;
    let cs = st.resolve(&r, &q.rev)?;
    let v: Vec<Value> = st.blame(&r, &cs, &q.path)?.into_iter().map(|(c, l)| json!({"cs": c, "line": l})).collect();
    Ok(Json(json!(v)))
}
#[derive(Deserialize)]
struct SearchQ {
    q: String,
    #[serde(default = "main_rev")]
    rev: String,
    #[serde(default = "hundred")]
    limit: usize,
}
fn main_rev() -> String {
    "main".into()
}
fn hundred() -> usize {
    100
}
/// Code search over one revision; only files the caller may read are searched.
async fn search(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(q): Query<SearchQ>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    let cs = st.resolve(&r, &q.rev)?;
    let m = az.filter(st.manifest(&r, &cs)?);
    let st2 = st.clone();
    let hits = tokio::task::spawn_blocking(move || st2.search(&m, &q.q, q.limit.clamp(1, 1000)))
        .await
        .map_err(|e| ApiErr(Error::Bad(e.to_string())))??;
    Ok(Json(json!(hits)))
}
#[derive(Deserialize)]
struct Diff {
    from: String,
    to: Option<String>,
    #[serde(default)]
    prefixes: Vec<String>,
    /// Working-copy overlay: path -> entry (null = deleted). Used instead of `to`.
    overlay: Option<std::collections::BTreeMap<String, Option<String>>>,
}
async fn diff(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Diff>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    let a = st.resolve(&r, &b.from)?;
    let ma = az.filter(st.tree(&r, &a, &b.prefixes)?);
    let mb: Manifest = match (b.to, b.overlay) {
        (Some(to), _) => {
            let t = st.resolve(&r, &to)?;
            az.filter(st.tree(&r, &t, &b.prefixes)?)
        }
        (None, Some(ov)) => {
            let mut mb = ma.clone();
            for (p, v) in ov {
                if !az.can(&p, Perm::Read) {
                    continue;
                }
                match v {
                    Some(x) => {
                        need_blob(&st, split_entry(&x).0, &u)?;
                        mb.insert(p, x);
                    }
                    None => {
                        mb.remove(&p);
                    }
                }
            }
            mb
        }
        _ => return Err(ApiErr(Error::Bad("need `to` or `overlay`".into()))),
    };
    Ok(Json(json!({"diff": st.diff_manifests(&ma, &mb)?})))
}
async fn changeset(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, String)>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    let cs = redact(&az, st.get_changeset(&r, &id)?).ok_or_else(|| ApiErr(Error::NotFound(format!("changeset {id}"))))?;
    Ok(Json(json!(cs)))
}
#[derive(Deserialize)]
struct MbQ {
    a: String,
    b: String,
}
async fn merge_base(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(q): Query<MbQ>) -> R {
    let u = user(&st, &h)?;
    need(st.authz(&r, &u)?.can_any(Perm::Read), "read on this repo")?;
    let (a, b) = (st.resolve(&r, &q.a)?, st.resolve(&r, &q.b)?);
    Ok(Json(json!({"base": st.merge_base(&r, &a, &b)?})))
}
async fn get_acl(State(st): State<St>, h: HeaderMap, Path(r): Path<String>) -> R {
    let u = user(&st, &h)?;
    let az = st.authz(&r, &u)?;
    need(az.open() || az.can("", Perm::Admin), "admin on /")?;
    Ok(Json(json!(st.acl(&r)?)))
}
#[derive(Deserialize)]
struct SetAcl {
    prefix: String,
    principal: String,
    perm: Option<String>,
}
async fn set_acl(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<SetAcl>) -> R {
    let u = user(&st, &h)?;
    let perm = match b.perm.as_deref() {
        None => None,
        Some(p) => Some(Perm::parse(p).ok_or_else(|| ApiErr(Error::Bad("perm must be none, read, write or admin".into())))?),
    };
    st.set_acl(&r, &u, &b.prefix, &b.principal, perm)?;
    Ok(Json(json!(st.acl(&r)?)))
}
async fn whoami(State(st): State<St>, h: HeaderMap) -> R {
    let u = user(&st, &h)?;
    Ok(Json(json!({"user": u.user, "groups": u.groups})))
}

/// Replays the stored response when a POST carries an `Idempotency-Key` that
/// this caller already used (24 h window), so a client can retry a commit or
/// merge after a network error without applying it twice. The key is bound to
/// the request body: reusing it with a different body is rejected (422).
async fn idempotent(State(st): State<St>, req: Request, next: Next) -> Response {
    let key = req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()).map(String::from);
    let (Some(key), true) = (key, req.method() == axum::http::Method::POST) else { return next.run(req).await };
    if key.is_empty() || key.len() > 128 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_request", "message": "Idempotency-Key must be 1..128 characters"})))
            .into_response();
    }
    let cred =
        req.headers().get("authorization").or_else(|| req.headers().get("cookie")).and_then(|v| v.to_str().ok()).unwrap_or("anonymous");
    let scope = cambium_core::blob::hash_bytes(cred.as_bytes());
    // scoped to the exact path, so a key reused on another repo/ref/review never replays
    let k = format!("{}:{}", req.uri().path(), key);
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, max_body()).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    };
    let req_hash = cambium_core::blob::hash_bytes(&bytes);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let _ = st.db.exec("DELETE FROM idempotency WHERE created_at<?1", cambium_core::p![now - 86_400]);
    let placeholder = json!({"req": req_hash}).to_string();
    match st.db.exec(
        "INSERT INTO idempotency(user_name,key,response,created_at) VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
        cambium_core::p![&scope, &k, &placeholder, now],
    ) {
        Ok(0) => {
            let stored: Value = st
                .db
                .one("SELECT response FROM idempotency WHERE user_name=?1 AND key=?2", cambium_core::p![&scope, &k])
                .ok()
                .flatten()
                .and_then(|r| serde_json::from_str(&r.s(0)).ok())
                .unwrap_or(Value::Null);
            if stored["req"] != json!(req_hash) {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": "idempotency_key_reused", "message": "this Idempotency-Key was used with a different request"})),
                )
                    .into_response();
            }
            let Some(status) = stored["status"].as_u64() else {
                return (
                    StatusCode::CONFLICT,
                    [("retry-after", "1")],
                    Json(json!({"error": "in_progress", "message": "a request with this Idempotency-Key is still running"})),
                )
                    .into_response();
            };
            return (
                StatusCode::from_u16(status as u16).unwrap_or(StatusCode::OK),
                [("content-type", "application/json"), ("idempotent-replayed", "true")],
                stored["body"].as_str().unwrap_or("").to_string(),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(e) => return ApiErr(e).into_response(),
    }
    let resp = next.run(Request::from_parts(parts, axum::body::Body::from(bytes))).await;
    if !resp.status().is_success() {
        let _ = st.db.exec("DELETE FROM idempotency WHERE user_name=?1 AND key=?2", cambium_core::p![&scope, &k]);
        return resp;
    }
    let (rp, rb) = resp.into_parts();
    let body = axum::body::to_bytes(rb, 64 * 1024 * 1024).await.unwrap_or_default();
    let rec = json!({"req": req_hash, "status": rp.status.as_u16(), "body": String::from_utf8_lossy(&body)}).to_string();
    let _ = st.db.exec("UPDATE idempotency SET response=?1 WHERE user_name=?2 AND key=?3", cambium_core::p![rec, &scope, &k]);
    Response::from_parts(rp, axum::body::Body::from(body))
}
fn max_body() -> usize {
    env_u64("CAMBIUM_MAX_BODY_MB", 64) as usize * 1024 * 1024
}

pub fn app(st: St, lim: Arc<RateLimiter>) -> Router {
    let timeout = std::time::Duration::from_secs(env_u64("CAMBIUM_REQUEST_TIMEOUT_S", 120));
    Router::new()
        .route("/v1/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/healthz", get(obs::healthz))
        .route("/readyz", get(obs::readyz))
        .route("/metrics", get(obs::metrics_endpoint))
        .route("/v1/repos", post(create_repo))
        .route("/v1/repos/:r/refs", get(refs))
        .route("/v1/repos/:r/refs/*name", post(cas))
        .route("/v1/repos/:r/resolve/*rev", get(resolve))
        .route("/v1/repos/:r/commit", post(commit))
        .route("/v1/repos/:r/tree", post(tree))
        .route("/v1/repos/:r/merge", post(merge))
        .route("/v1/repos/:r/rebase", post(rebase))
        .route("/v1/repos/:r/changesets", post(create_changeset))
        .route("/v1/repos/:r/root/*rev", get(root))
        .route("/v1/repos/:r/trees/:id", get(get_tree))
        .route("/v1/repos/:r/trees", post(get_trees))
        .route("/v1/repos/:r/land", post(land))
        .route("/v1/repos/:r/bisect", get(bisect))
        .route("/v1/repos/:r/revert", post(revert))
        .route("/v1/repos/:r/log", get(log))
        .route("/v1/repos/:r/blame", get(blame))
        .route("/v1/repos/:r/search", get(search))
        .route("/v1/repos/:r/diff", post(diff))
        .route("/v1/repos/:r/merge-base", get(merge_base))
        .route("/v1/repos/:r/changesets/:id", get(changeset))
        .route("/v1/repos/:r/acl", get(get_acl).post(set_acl))
        .route("/v1/whoami", get(whoami))
        .route("/v1/blobs", post(put_blob))
        .route("/v1/blobs/:id", get(get_blob))
        .merge(review::api())
        .merge(review::ui())
        .merge(browse::ui())
        .merge(hooks::api())
        .merge(settings::ui())
        .layer(middleware::from_fn_with_state(st.clone(), idempotent))
        .layer(middleware::from_fn_with_state((st.clone(), lim), limit))
        .layer(DefaultBodyLimit::max(max_body()))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, timeout))
        .layer(middleware::from_fn_with_state(st.clone(), obs::observe))
        .with_state(st)
}

/// Blob backend from env: CAMBIUM_BLOB=s3 (CAMBIUM_S3_*), else the local filesystem under CAMBIUM_DATA.
pub(crate) fn blob_store(data: &str) -> Arc<dyn BlobStore> {
    match std::env::var("CAMBIUM_BLOB").as_deref() {
        Ok("s3") => {
            let c = cambium_core::s3::S3Config::from_env().unwrap_or_else(|e| panic!("{e}"));
            cambium_core::log::info("blob store", json!({"kind": "s3", "endpoint": c.endpoint, "bucket": c.bucket}));
            let st = cambium_core::s3::S3BlobStore::new(c);
            // a fresh RustFS/MinIO has no bucket yet; without create permission this just warns
            if let Err(e) = st.ensure_bucket() {
                cambium_core::log::error("could not create or verify the S3 bucket", json!({"error": e.to_string()}));
            }
            Arc::new(st)
        }
        _ => {
            // CAMBIUM_BLOB_DIR lets several nodes share one directory (e.g. an NFS mount)
            let dir = std::env::var("CAMBIUM_BLOB_DIR").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| format!("{data}/blobs"));
            Arc::new(FsBlobStore::new(dir).expect("blob dir"))
        }
    }
}

/// Metadata store from env: CAMBIUM_DB=postgres://... for multi-node, else SQLite in CAMBIUM_DATA.
pub(crate) fn open_engine(data: &str, blobs: Arc<dyn BlobStore>) -> Engine {
    match std::env::var("CAMBIUM_DB").ok().filter(|v| !v.is_empty()) {
        Some(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => {
            cambium_core::log::info("metadata store", json!({"kind": "postgres"}));
            Engine::open_pg(&url, blobs).unwrap_or_else(|e| panic!("postgres: {e}"))
        }
        Some(other) => panic!("CAMBIUM_DB must be a postgres:// URL (got a different scheme: {})", other.split(':').next().unwrap_or("")),
        None => Engine::open(&format!("{data}/cambium.db"), blobs).expect("db"),
    }
}

const USAGE: &str = "usage: cmbd [server]                      run the API server (CAMBIUM_ADDR, CAMBIUM_DATA, CAMBIUM_DB, ...)
       cmbd edge                          regional read-through cache (CAMBIUM_UPSTREAM)
       cmbd token <name> [--groups g] [--ttl 90d] [--label l] | token list | token rotate <name> [--grace 1h] | token revoke <name>
       cmbd audit export [--out f] | audit verify
       cmbd migrate [--status]
       cmbd backup <dir> | cmbd restore <dir>
       cmbd import-git <git-dir> <repo> [owner]
See docs/deploy.md and docs/runbook.md.";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            return;
        }
        Some("-V" | "--version") => {
            println!("cmbd {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some(a) if a.starts_with('-') => {
            eprintln!("cmbd: unknown option {a}\n{USAGE}");
            std::process::exit(2);
        }
        _ => {}
    }
    match std::env::args().nth(1).as_deref() {
        Some("edge") => return edge::run("edge"),
        Some("daemon") => return edge::run("daemon"),
        _ => {}
    }
    let data = std::env::var("CAMBIUM_DATA").unwrap_or_else(|_| "./cambium-data".into());
    std::fs::create_dir_all(&data).expect("data dir");
    // stores open before the async runtime starts (the Postgres client is synchronous)
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        std::env::set_var("CAMBIUM_MIGRATING", "1");
    }
    if std::env::args().nth(1).as_deref() == Some("restore") {
        return admin::restore_cmd(&data);
    }
    let eng = Arc::new(open_engine(&data, blob_store(&data)));
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = admin::dispatch(&eng, &data, &args) {
        std::process::exit(code);
    }
    if let Some(a) = args.get(1).filter(|a| a.as_str() != "server") {
        eprintln!("cmbd: unknown command {a}\n{USAGE}");
        std::process::exit(2);
    }
    let oidc = auth::OidcConfig::from_env().map(|c| {
        let iss = c.issuer.clone();
        let o = auth::Oidc::new(c).unwrap_or_else(|e| panic!("OIDC setup: {e}"));
        cambium_core::log::info("OIDC sign-in enabled; static tokens still accepted", json!({"issuer": iss}));
        o
    });
    let ctx = Arc::new(Ctx { eng, oidc });
    hooks::start(ctx.eng.clone());
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(serve(ctx, data));
    // don't wait for idle blocking-pool threads or pool workers on the way out
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    std::process::exit(0);
}

async fn serve(ctx: Arc<Ctx>, data: String) {
    let addr: std::net::SocketAddr =
        std::env::var("CAMBIUM_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".into()).parse().expect("CAMBIUM_ADDR must be host:port");
    // CAMBIUM_RATE: requests per second per token (0 = off). CAMBIUM_BURST: how many may be saved up.
    let rate: f64 = std::env::var("CAMBIUM_RATE").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0);
    let burst: f64 = std::env::var("CAMBIUM_BURST").ok().and_then(|v| v.parse().ok()).unwrap_or(200.0);
    let app = app(ctx, Arc::new(RateLimiter::new(rate, burst)));
    let handle = axum_server::Handle::new();
    let grace = std::time::Duration::from_secs(env_u64("CAMBIUM_SHUTDOWN_GRACE_S", 25));
    let drain = std::time::Duration::from_secs(env_u64("CAMBIUM_SHUTDOWN_DRAIN_S", 5));
    let h2 = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        // 1) fail /readyz so the load balancer stops sending new work, 2) wait for it
        // to notice, 3) stop accepting and let in-flight requests finish (bounded).
        obs::metrics().shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
        cambium_core::log::info("shutdown requested; draining", json!({"drain_s": drain.as_secs(), "grace_s": grace.as_secs()}));
        tokio::time::sleep(drain).await;
        h2.graceful_shutdown(Some(grace));
        // hard deadline: never outlive drain + grace (+5s) even if a blocking DB call or a
        // keep-alive connection holds the runtime open
        let deadline = grace + std::time::Duration::from_secs(5);
        std::thread::spawn(move || {
            std::thread::sleep(deadline);
            cambium_core::log::error("shutdown deadline exceeded; exiting", json!({"deadline_s": deadline.as_secs()}));
            std::process::exit(1);
        });
    });
    let hdr = std::time::Duration::from_secs(env_u64("CAMBIUM_HEADER_TIMEOUT_S", 15));
    cambium_core::log::info(
        "cmbd listening",
        json!({"addr": addr.to_string(), "data": data, "tls": tls_enabled(), "rate_per_s": rate, "burst": burst,
        "max_body_mb": env_u64("CAMBIUM_MAX_BODY_MB", 64), "request_timeout_s": env_u64("CAMBIUM_REQUEST_TIMEOUT_S", 120), "version": env!("CARGO_PKG_VERSION")}),
    );
    let svc = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    if tls_enabled() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            std::env::var("CAMBIUM_TLS_CERT").unwrap(),
            std::env::var("CAMBIUM_TLS_KEY").unwrap(),
        )
        .await
        .unwrap_or_else(|e| panic!("TLS: {e}"));
        let mut srv = axum_server::bind_rustls(addr, cfg).handle(handle);
        srv.http_builder().http1().timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(hdr);
        srv.serve(svc).await.expect("server");
    } else {
        let mut srv = axum_server::bind(addr).handle(handle);
        srv.http_builder().http1().timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(hdr);
        srv.serve(svc).await.expect("server");
    }
    cambium_core::log::info("cmbd stopped cleanly", json!({}));
}
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request as Req;
    use cambium_core::blob::MemBlobStore;
    use tower::ServiceExt;

    fn setup(rate: f64, burst: f64) -> (Router, String, Arc<Engine>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let e = Arc::new(Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap());
        let t = e.create_token("alice").unwrap();
        (app(Arc::new(Ctx { eng: e.clone(), oidc: None }), Arc::new(RateLimiter::new(rate, burst))), t, e, d)
    }
    fn get(uri: &str, tok: &str, range: Option<&str>) -> Req<Body> {
        let mut b = Req::get(uri).header("authorization", format!("Bearer {tok}"));
        if let Some(r) = range {
            b = b.header("range", r);
        }
        b.body(Body::empty()).unwrap()
    }

    fn post(uri: &str, tok: &str, body: Value) -> Req<Body> {
        Req::post(uri)
            .header("authorization", format!("Bearer {tok}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Hidden blobs cannot be pulled out through the diff overlay or a commit.
    #[tokio::test]
    async fn hidden_blob_not_readable_via_diff_or_commit() {
        let (app, alice, e, _d) = setup(0.0, 1.0);
        let bob = e.create_token("bob").unwrap();
        let root = e.create_repo("r", "alice").unwrap();
        let secret = e.put_blob(b"salaries\n").unwrap();
        let open = e.put_blob(b"hello\n").unwrap();
        let fc =
            |p: &str, b: &str| cambium_core::FileChange { path: p.into(), blob: Some(b.into()), mode: cambium_core::engine::MODE_FILE };
        let c = e.commit("r", "main", &root, "alice", "init", vec![fc("pub/a", &open), fc("pay/s", &secret)]).unwrap();
        let a = Principal { user: "alice".into(), groups: vec![] };
        e.set_acl("r", &a, "", "user:alice", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &a, "pub", "user:bob", Some(Perm::Write)).unwrap();

        // diff overlay with the hidden blob under a path bob can read
        let r = app.clone().oneshot(post("/v1/repos/r/diff", &bob, json!({"from": "main", "overlay": {"pub/a": secret}}))).await.unwrap();
        assert_eq!(r.status(), 400);
        let body = to_bytes(r.into_body(), 10_000).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("salaries"));
        // commit the hidden blob under a writable path, then try to read it back
        let r = app
            .clone()
            .oneshot(post(
                "/v1/repos/r/commit",
                &bob,
                json!({"ref": "main", "expected": c.id, "message": "m", "files": [{"path": "pub/x", "blob": secret}]}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // alice (who can read it) may still reference it
        let r = app.clone().oneshot(post("/v1/repos/r/diff", &alice, json!({"from": "main", "overlay": {"pub/a": secret}}))).await.unwrap();
        assert_eq!(r.status(), 200);
        // bob may commit identical bytes if he uploads them himself
        let up = Req::post("/v1/blobs").header("authorization", format!("Bearer {bob}")).body(Body::from("salaries\n")).unwrap();
        assert_eq!(app.clone().oneshot(up).await.unwrap().status(), 200);
        let r = app
            .oneshot(post(
                "/v1/repos/r/commit",
                &bob,
                json!({"ref": "main", "expected": c.id, "message": "m", "files": [{"path": "pub/x", "blob": secret}]}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn blob_range_resume() {
        let (app, t, e, _d) = setup(0.0, 1.0);
        // an unreferenced blob is readable by whoever uploaded it
        let id = e.put_blob_as(b"0123456789", &Principal { user: "alice".into(), groups: vec![] }).unwrap();
        let r = app.clone().oneshot(get(&format!("/v1/blobs/{id}"), &t, None)).await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["accept-ranges"], "bytes");
        let r = app.clone().oneshot(get(&format!("/v1/blobs/{id}"), &t, Some("bytes=6-"))).await.unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()["content-range"], "bytes 6-9/10");
        assert_eq!(&to_bytes(r.into_body(), 100).await.unwrap()[..], b"6789");
        let r = app.oneshot(get(&format!("/v1/blobs/{id}"), &t, Some("bytes=10-"))).await.unwrap();
        assert_eq!(r.status(), 416);
    }

    #[tokio::test]
    async fn rate_limit_returns_429_with_retry_after_per_token() {
        let (app, t, e, _d) = setup(0.5, 2.0);
        let other = e.create_token("bob").unwrap();
        e.create_repo("r", "alice").unwrap();
        for _ in 0..2 {
            assert_eq!(app.clone().oneshot(get("/v1/repos/r/refs", &t, None)).await.unwrap().status(), 200);
        }
        let r = app.clone().oneshot(get("/v1/repos/r/refs", &t, None)).await.unwrap();
        assert_eq!(r.status(), 429);
        assert_eq!(r.headers()["retry-after"], "2");
        assert_eq!(app.clone().oneshot(get("/v1/repos/r/refs", &other, None)).await.unwrap().status(), 200, "other token unaffected");
        let h = Req::get("/v1/health").body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(h).await.unwrap().status(), 200, "health exempt");
    }

    /// Made-up tokens share one unauthenticated bucket instead of getting a fresh one each,
    /// and several tokens for one user share that user's bucket.
    #[tokio::test]
    async fn rate_limit_keyed_by_user_not_raw_token() {
        let (app, t, e, _d) = setup(0.001, 2.0);
        e.create_repo("r", "alice").unwrap();
        for i in 0..2 {
            let fake = format!("cmbt_fake{i}");
            assert_eq!(app.clone().oneshot(get("/v1/repos/r/refs", &fake, None)).await.unwrap().status(), 401);
        }
        let r = app.clone().oneshot(get("/v1/repos/r/refs", "cmbt_fake_new", None)).await.unwrap();
        assert_eq!(r.status(), 429, "a new fake token must not get a fresh bucket");
        // a real user is not blocked by someone else's bad requests
        assert_eq!(app.clone().oneshot(get("/v1/repos/r/refs", &t, None)).await.unwrap().status(), 200);
        // a second token for alice shares alice's bucket
        let t2 = e.create_token("alice").unwrap();
        assert_eq!(app.clone().oneshot(get("/v1/repos/r/refs", &t2, None)).await.unwrap().status(), 200);
        assert_eq!(app.oneshot(get("/v1/repos/r/refs", &t2, None)).await.unwrap().status(), 429);
    }

    /// Review comments, approvals and merge conflicts respect per-directory permissions.
    #[tokio::test]
    async fn reviews_and_conflicts_hide_unreadable_paths() {
        let (app, alice, e, _d) = setup(0.0, 1.0);
        let bob = e.create_token("bob").unwrap();
        let root = e.create_repo("r", "alice").unwrap();
        let fc = |p: &str, c: &str| cambium_core::FileChange {
            path: p.into(),
            blob: Some(e.put_blob(c.as_bytes()).unwrap()),
            mode: cambium_core::engine::MODE_FILE,
        };
        let base = e.commit("r", "main", &root, "alice", "base", vec![fc("pub/a", "1\n"), fc("pay/s", "1\n")]).unwrap().id;
        let a = Principal { user: "alice".into(), groups: vec![] };
        e.set_acl("r", &a, "", "user:alice", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &a, "pub", "user:bob", Some(Perm::Write)).unwrap();
        e.cas_ref("r", "feat", None, Some(&base)).unwrap();
        e.commit("r", "feat", &base, "alice", "both", vec![fc("pub/a", "2\n"), fc("pay/s", "2\n")]).unwrap();
        let body = |r: axum::response::Response| async {
            serde_json::from_slice::<Value>(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
        };
        let rv =
            body(app.clone().oneshot(post("/v1/repos/r/reviews", &alice, json!({"source": "feat", "title": "t"}))).await.unwrap()).await;
        let id = rv["id"].as_i64().unwrap();
        let c = |p: &str| json!({"body": "about salaries", "path": p});
        assert_eq!(
            app.clone().oneshot(post(&format!("/v1/repos/r/reviews/{id}/comments"), &alice, c("pay/s"))).await.unwrap().status(),
            200
        );
        // the review changes pay/, which bob cannot read: to him it does not exist
        assert_eq!(app.clone().oneshot(post(&format!("/v1/repos/r/reviews/{id}/comments"), &bob, c("pub/a"))).await.unwrap().status(), 404);
        assert_eq!(app.clone().oneshot(get(&format!("/v1/repos/r/reviews/{id}"), &bob, None)).await.unwrap().status(), 404);
        let listed = body(app.clone().oneshot(get("/v1/repos/r/reviews", &bob, None)).await.unwrap()).await;
        assert_eq!(listed.as_array().unwrap().len(), 0, "not listed either");
        assert_ne!(app.clone().oneshot(post(&format!("/v1/repos/r/reviews/{id}/approve"), &bob, json!({}))).await.unwrap().status(), 200);
        // conflicts: both sides edit pub/a and pay/s
        e.commit("r", "main", &base, "alice", "main side", vec![fc("pub/a", "3\n"), fc("pay/s", "3\n")]).unwrap();
        let r = app.clone().oneshot(post("/v1/repos/r/merge", &bob, json!({"target": "main", "source": "feat"}))).await.unwrap();
        assert_eq!(r.status(), 409);
        let v = body(r).await;
        let paths: Vec<String> = serde_json::from_value(v["paths"].clone()).unwrap();
        assert_eq!(paths, vec!["pub/a".to_string(), "(1 more you cannot read)".to_string()]);
        assert!(!v.to_string().contains("pay/"));
        // log limit is capped
        let r = app.oneshot(get("/v1/repos/r/log?rev=main&limit=1000000000", &alice, None)).await.unwrap();
        assert_eq!(r.status(), 200);
    }

    #[test]
    fn client_ip_ignores_forwarded_for_unless_trusted() {
        let mk = || {
            let mut r = Req::get("/").header("x-forwarded-for", "6.6.6.6, 10.0.0.9").body(Body::empty()).unwrap();
            r.extensions_mut().insert(axum::extract::ConnectInfo("192.0.2.1:5000".parse::<std::net::SocketAddr>().unwrap()));
            r
        };
        std::env::remove_var("CAMBIUM_TRUST_PROXY");
        assert_eq!(obs::client_ip(&mk()).unwrap().to_string(), "192.0.2.1");
        std::env::set_var("CAMBIUM_TRUST_PROXY", "1");
        assert_eq!(obs::client_ip(&mk()).unwrap().to_string(), "10.0.0.9", "rightmost entry, added by our proxy");
        std::env::remove_var("CAMBIUM_TRUST_PROXY");
    }

    /// An Idempotency-Key replays only on the exact same path, not on another repo.
    #[tokio::test]
    async fn idempotency_key_scoped_to_path() {
        let (app, t, e, _d) = setup(0.0, 1.0);
        e.create_repo("a", "alice").unwrap();
        e.create_repo("b", "alice").unwrap();
        let req = |repo: &str| {
            Req::post(format!("/v1/repos/{repo}/refs/release/1"))
                .header("authorization", format!("Bearer {t}"))
                .header("content-type", "application/json")
                .header("idempotency-key", "k1")
                .body(Body::from(json!({"expected": null, "new": e.get_ref(repo, "main").unwrap().unwrap()}).to_string()))
                .unwrap()
        };
        assert_eq!(app.clone().oneshot(req("a")).await.unwrap().status(), 200);
        app.clone().oneshot(req("b")).await.unwrap();
        assert!(e.get_ref("b", "release/1").unwrap().is_some(), "repo b got its own ref, not a replay of repo a");
    }

    /// Deleting a branch needs write on every path where it differs from main.
    #[tokio::test]
    async fn ref_delete_needs_write_on_branch_changes() {
        let (app, alice, e, _d) = setup(0.0, 1.0);
        let bob = e.create_token("bob").unwrap();
        let root = e.create_repo("r", "alice").unwrap();
        let a = Principal { user: "alice".into(), groups: vec![] };
        e.set_acl("r", &a, "", "user:alice", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &a, "pub", "user:bob", Some(Perm::Write)).unwrap();
        let fc = |p: &str| cambium_core::FileChange {
            path: p.into(),
            blob: Some(e.put_blob(p.as_bytes()).unwrap()),
            mode: cambium_core::engine::MODE_FILE,
        };
        e.cas_ref("r", "pay-work", None, Some(&root)).unwrap();
        let pw = e.commit("r", "pay-work", &root, "alice", "pay", vec![fc("pay/x")]).unwrap().id;
        e.cas_ref("r", "pub-work", None, Some(&root)).unwrap();
        let bw = e.commit("r", "pub-work", &root, "alice", "pub", vec![fc("pub/x")]).unwrap().id;
        let del =
            |name: &str, head: &str, tok: &str| post(&format!("/v1/repos/r/refs/{name}"), tok, json!({"expected": head, "new": null}));
        assert_eq!(app.clone().oneshot(del("pay-work", &pw, &bob)).await.unwrap().status(), 403);
        assert_eq!(app.clone().oneshot(del("pub-work", &bw, &bob)).await.unwrap().status(), 200);
        assert_eq!(app.oneshot(del("pay-work", &pw, &alice)).await.unwrap().status(), 200);
    }

    /// Tree fetch: whole and verifiable for readers of everything; filtered for others;
    /// hidden trees are not served at any path.
    #[tokio::test]
    async fn tree_fetch_respects_permissions() {
        let (app, alice, e, _d) = setup(0.0, 1.0);
        let bob = e.create_token("bob").unwrap();
        let root = e.create_repo("r", "alice").unwrap();
        let fc = |p: &str| cambium_core::FileChange {
            path: p.into(),
            blob: Some(e.put_blob(p.as_bytes()).unwrap()),
            mode: cambium_core::engine::MODE_FILE,
        };
        e.commit("r", "main", &root, "alice", "c", vec![fc("pub/a"), fc("pay/s")]).unwrap();
        let a = Principal { user: "alice".into(), groups: vec![] };
        e.set_acl("r", &a, "", "user:alice", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &a, "pub", "user:bob", Some(Perm::Read)).unwrap();
        let body = |r: axum::response::Response| async {
            serde_json::from_slice::<Value>(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
        };
        let rt = body(app.clone().oneshot(get("/v1/repos/r/root/main", &alice, None)).await.unwrap()).await;
        let rid = rt["root"].as_str().unwrap().to_string();
        let full = body(app.clone().oneshot(get(&format!("/v1/repos/r/trees/{rid}"), &alice, None)).await.unwrap()).await;
        assert_eq!(full["complete"], true);
        let entries: Vec<cambium_core::tree::TreeEntry> = serde_json::from_value(full["entries"].clone()).unwrap();
        assert_eq!(cambium_core::tree::Tree { entries: entries.clone() }.id(), rid, "client can verify the tree");
        let pay = entries.iter().find(|x| x.name == "pay").unwrap().id.clone();
        // bob: root is filtered to pub only
        let filtered = body(app.clone().oneshot(get(&format!("/v1/repos/r/trees/{rid}"), &bob, None)).await.unwrap()).await;
        assert_eq!(filtered["complete"], false);
        assert_eq!(filtered["entries"].as_array().unwrap().len(), 1);
        assert!(!filtered.to_string().contains("pay"));
        // the hidden tree is not served, at its own path or a readable one
        for path in ["pay", "pub"] {
            let r = app.clone().oneshot(get(&format!("/v1/repos/r/trees/{pay}?path={path}"), &bob, None)).await.unwrap();
            assert_eq!(r.status(), 404, "{path}");
        }
        assert_eq!(app.oneshot(get(&format!("/v1/repos/r/trees/{pay}?path=pay"), &alice, None)).await.unwrap().status(), 200);
    }

    /// Code browser pages apply per-directory permissions and escape file content.
    #[tokio::test]
    async fn code_browser_respects_permissions_and_escapes() {
        let (app, _alice, e, _d) = setup(0.0, 1.0);
        let root = e.create_repo("r", "alice").unwrap();
        let fc = |p: &str, c: &str| cambium_core::FileChange {
            path: p.into(),
            blob: Some(e.put_blob(c.as_bytes()).unwrap()),
            mode: cambium_core::engine::MODE_FILE,
        };
        let c1 = e
            .commit("r", "main", &root, "alice", "public things", vec![fc("pub/a.html", "<script>alert(1)</script>\nline two\n")])
            .unwrap()
            .id;
        e.commit("r", "main", &c1, "alice", "salaries update", vec![fc("pay/s.txt", "salaries 100\n")]).unwrap();
        let a = Principal { user: "alice".into(), groups: vec![] };
        e.set_acl("r", &a, "", "user:alice", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &a, "pub", "user:bob", Some(Perm::Read)).unwrap();
        let bob = Principal { user: "bob".into(), groups: vec![] };
        let sid = e.create_session(&bob, 3600, "x.y.z", None).unwrap();
        let page = |uri: &str| {
            let app = app.clone();
            let uri = uri.to_string();
            let sid = sid.clone();
            async move {
                let r = app
                    .oneshot(Req::get(&uri).header("cookie", format!("cambium_session={sid}")).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = r.status();
                let headers = r.headers().clone();
                (status, headers, String::from_utf8_lossy(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).to_string())
            }
        };
        let (s, _, b) = page("/ui/r/code").await;
        assert_eq!(s, 200);
        assert!(b.contains("pub/") && !b.contains("pay"), "hidden dir not listed");
        let (s, _, b) = page("/ui/r/code?path=pub/a.html").await;
        assert_eq!(s, 200);
        assert!(b.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !b.contains("<script>alert"), "content escaped");
        for uri in [
            "/ui/r/code?path=pay",
            "/ui/r/code?path=pay/s.txt",
            "/ui/r/raw?path=pay/s.txt",
            "/ui/r/blame?path=pay/s.txt",
            "/ui/r/history?path=pay",
        ] {
            assert_eq!(page(uri).await.0, 404, "{uri}");
        }
        let (s, h, b) = page("/ui/r/raw?path=pub/a.html").await;
        assert_eq!(s, 200);
        assert_eq!(h["content-type"], "application/octet-stream");
        assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment"));
        assert!(b.contains("<script>"), "raw bytes are served as a download");
        let (_, _, b) = page("/ui/r/history").await;
        assert!(b.contains("public things") && !b.contains("salaries"), "pay-only changeset hidden");
        let (s, _, b) = page("/ui/r/blame?path=pub/a.html").await;
        assert!(s == 200 && b.contains("line two"));
        let (_, _, b) = page("/ui/r/search?q=salaries").await;
        assert!(b.contains("0 result(s)"), "search does not leak hidden files");
        let (_, _, b) = page(&format!("/ui/r/cs/{c1}")).await;
        assert!(b.contains("public things") && b.contains("pub/a.html"));
        // signed out: sent to the sign-in page
        let r = app.oneshot(Req::get("/ui/r/code").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 303);
    }

    /// Settings pages: admins only, CSRF-checked, same rules as the API.
    #[tokio::test]
    async fn settings_pages_admin_only_and_csrf_checked() {
        let (app, _t, e, _d) = setup(0.0, 1.0);
        e.create_repo("r", "alice").unwrap();
        let session = |user: &str| {
            let tok = e.create_token(user).unwrap();
            let p = e.verify_principal(&tok).unwrap();
            let sid = e.create_session(&p, 3600, &tok, None).unwrap();
            (sid.clone(), crate::review::csrf(&sid))
        };
        let (alice_sid, alice_csrf) = session("alice");
        let (bob_sid, bob_csrf) = session("bob");
        let get_page = |sid: String, uri: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(Req::get(uri).header("cookie", format!("cambium_session={sid}")).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        let form = |sid: String, uri: String, body: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Req::post(uri)
                        .header("cookie", format!("cambium_session={sid}"))
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };
        assert_eq!(get_page(alice_sid.clone(), "/ui/r/settings").await, 200, "owner of an open repo");
        assert_eq!(get_page(bob_sid.clone(), "/ui/r/settings").await, 403);
        // wrong CSRF value: refused, nothing changes
        assert_eq!(
            form(alice_sid.clone(), "/ui/r/settings/acl".into(), "csrf=bad&prefix=&principal=user:alice&perm=admin".into()).await,
            403
        );
        assert!(e.acl("r").unwrap().is_empty());
        // bob cannot set rules even with his own valid CSRF value
        assert_ne!(
            form(bob_sid.clone(), "/ui/r/settings/acl".into(), format!("csrf={bob_csrf}&prefix=&principal=user:bob&perm=admin")).await,
            303
        );
        assert!(e.acl("r").unwrap().is_empty());
        // alice sets rules, protection and a webhook
        assert_eq!(
            form(alice_sid.clone(), "/ui/r/settings/acl".into(), format!("csrf={alice_csrf}&prefix=&principal=user:alice&perm=admin"))
                .await,
            303
        );
        assert_eq!(e.acl("r").unwrap().len(), 1);
        assert_eq!(
            form(alice_sid.clone(), "/ui/r/settings/protect".into(), format!("csrf={alice_csrf}&refname=main&approvals=2&trivial=1")).await,
            303
        );
        assert_eq!(e.required_approvals("r", "main").unwrap(), 2);
        assert_eq!(e.trivial_rules("r").unwrap(), vec![("main".to_string(), Some(1))]);
        std::env::remove_var("CAMBIUM_WEBHOOK_ALLOW_LOCAL");
        assert_eq!(
            form(
                alice_sid.clone(),
                "/ui/r/settings/hooks".into(),
                format!("csrf={alice_csrf}&url=http%3A%2F%2F169.254.169.254%2F&secret=0123456789abcdef")
            )
            .await,
            400,
            "metadata address refused"
        );
        assert_eq!(
            form(
                alice_sid.clone(),
                "/ui/r/settings/hooks".into(),
                format!("csrf={alice_csrf}&url=http%3A%2F%2F10.9.8.7%2Fci&secret=0123456789abcdef")
            )
            .await,
            303
        );
        let a = Principal { user: "alice".into(), groups: vec![] };
        let id = e.list_hooks("r", &a).unwrap()[0].id;
        assert_eq!(form(alice_sid.clone(), format!("/ui/r/settings/hooks/{id}/delete"), format!("csrf={alice_csrf}")).await, 303);
        assert!(e.list_hooks("r", &a).unwrap().is_empty());
        // create a repository from the home page form
        assert_eq!(form(bob_sid, "/ui/repos".into(), format!("csrf={bob_csrf}&name=bobs-repo")).await, 303);
        assert!(e.list_repos().unwrap().contains(&"bobs-repo".to_string()));
    }

    /// Regressions for the independent security review (2026-11-02).
    #[tokio::test]
    async fn security_review_regressions() {
        let (app, alice, e, _d) = setup(0.0, 1.0);
        let reader = e.create_token("rita").unwrap();
        let root = e.create_repo("r", "alice").unwrap();
        let fc = |p: &str, c: &str| cambium_core::FileChange {
            path: p.into(),
            blob: Some(e.put_blob(c.as_bytes()).unwrap()),
            mode: cambium_core::engine::MODE_FILE,
        };
        let c1 = e.commit("r", "main", &root, "alice", "public", vec![fc("proj/pub/a", "1\n")]).unwrap().id;
        let c2 = e.commit("r", "main", &c1, "alice", "SECRET: acquire Initech", vec![fc("proj/priv/acquisition.txt", "x\n")]).unwrap().id;
        let a = Principal { user: "alice".into(), groups: vec![] };
        e.set_acl("r", &a, "", "user:alice", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &a, "proj/pub", "user:rita", Some(Perm::Read)).unwrap();
        let text =
            |r: axum::response::Response| async { String::from_utf8_lossy(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).to_string() };
        // 3: refusals never name hidden paths
        let r = app.clone().oneshot(post("/v1/repos/r/refs/release/probe", &alice, json!({"expected": null, "new": c1}))).await.unwrap();
        assert_eq!(r.status(), 200);
        let r = app.clone().oneshot(post("/v1/repos/r/refs/release/probe", &reader, json!({"expected": c1, "new": c2}))).await.unwrap();
        assert_eq!(r.status(), 403);
        let t = text(r).await;
        assert!(!t.contains("acquisition") && t.contains("cannot read"), "{t}");
        // 5: a reader cannot create release branches; a writer only at history on main
        let r = app.clone().oneshot(post("/v1/repos/r/refs/release/x", &reader, json!({"expected": null, "new": c1}))).await.unwrap();
        assert_eq!(r.status(), 403);
        e.set_acl("r", &a, "proj/pub", "user:rita", Some(Perm::Write)).unwrap();
        let draft = e.create_changeset("r", vec![c2.clone()], "rita", "draft", vec![fc("proj/pub/b", "2\n")]).unwrap().id;
        let r = app.clone().oneshot(post("/v1/repos/r/refs/release/x", &reader, json!({"expected": null, "new": draft}))).await.unwrap();
        assert_eq!(r.status(), 403, "unreviewed draft cannot become a release");
        let r = app.clone().oneshot(post("/v1/repos/r/refs/release/x", &reader, json!({"expected": null, "new": c1}))).await.unwrap();
        assert_eq!(r.status(), 200, "history already on main is fine");
        // low: log of a hidden path answers like a missing one
        let r = app.clone().oneshot(get("/v1/repos/r/log?rev=main&path=proj/priv", &reader, None)).await.unwrap();
        assert_eq!(r.status(), 403);
        // 2: directory listing never shows a hidden changeset's message
        let sid = e.create_session(&Principal { user: "rita".into(), groups: vec![] }, 3600, &reader, None).unwrap();
        let r = app
            .clone()
            .oneshot(Req::get("/ui/r/code?path=proj").header("cookie", format!("cambium_session={sid}")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let t = text(r).await;
        assert!(t.contains("pub/") && !t.contains("SECRET") && !t.contains("priv"), "listing leaked: {t}");
        // low: an unreferenced blob is readable only by its uploader
        let up = e.put_blob_as(b"not committed yet", &a).unwrap();
        assert_eq!(app.clone().oneshot(get(&format!("/v1/blobs/{up}"), &alice, None)).await.unwrap().status(), 200);
        assert_eq!(app.clone().oneshot(get(&format!("/v1/blobs/{up}"), &reader, None)).await.unwrap().status(), 404);
        // low: cross-site sign-in refused
        let login = |site: &'static str| {
            Req::post("/ui/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("sec-fetch-site", site)
                .body(Body::from(format!("token={alice}")))
                .unwrap()
        };
        assert_eq!(app.clone().oneshot(login("cross-site")).await.unwrap().status(), 403);
        assert_eq!(app.clone().oneshot(login("same-origin")).await.unwrap().status(), 303);
        // low: CAMBIUM_REPO_CREATORS limits who may create repositories
        let who = |u: &str, g: &[&str]| Principal { user: u.into(), groups: g.iter().map(|x| x.to_string()).collect() };
        assert!(creator_allowed("", &who("anyone", &[])), "unset = anyone");
        assert!(creator_allowed("group:platform, user:alice", &who("alice", &[])));
        assert!(creator_allowed("group:platform, user:alice", &who("bob", &["platform"])));
        assert!(!creator_allowed("group:platform, user:alice", &who("rita", &["eng"])));
    }

    // ---------- OIDC + per-directory permissions over HTTP ----------
    /// A throwaway RSA key made fresh for this test run (never written to the repo).
    struct TestIdp {
        pem: String,
        _dir: tempfile::TempDir,
        jwks: String,
    }
    fn idp() -> &'static TestIdp {
        static IDP: std::sync::OnceLock<TestIdp> = std::sync::OnceLock::new();
        IDP.get_or_init(|| {
            use base64::Engine as _;
            // test-only signing key from the system openssl (keeps RSA private-key code
            // out of the dependency tree entirely)
            let dir = tempfile::tempdir().unwrap();
            let kp = dir.path().join("k.pem");
            let st = std::process::Command::new("openssl")
                .args(["genrsa", "-traditional", "-out"])
                .arg(&kp)
                .arg("2048")
                .stderr(std::process::Stdio::null())
                .status()
                .expect("openssl is needed for the OIDC tests");
            assert!(st.success());
            let m = std::process::Command::new("openssl").args(["rsa", "-noout", "-modulus", "-in"]).arg(&kp).output().unwrap();
            let hexmod = String::from_utf8(m.stdout).unwrap().trim().trim_start_matches("Modulus=").to_string();
            let b64 = |b: Vec<u8>| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
            let jwk = json!({"keys": [{"kty": "RSA", "kid": "test-key-1", "use": "sig", "alg": "RS256",
                "n": b64(hex::decode(hexmod).unwrap()), "e": b64(vec![1, 0, 1])}]});
            let jwks = dir.path().join("jwks.json").to_string_lossy().into_owned();
            std::fs::write(&jwks, jwk.to_string()).unwrap();
            let pem = std::fs::read_to_string(&kp).unwrap();
            TestIdp { pem, _dir: dir, jwks }
        })
    }
    fn oidc() -> auth::Oidc {
        auth::Oidc::new(auth::OidcConfig {
            issuer: "https://idp.example.test".into(),
            audience: "cambium-test".into(),
            jwks_file: Some(idp().jwks.clone()),
            jwks_url: None,
            user_claim: None,
            groups_claim: "groups".into(),
        })
        .unwrap()
    }
    fn jwt(claims: serde_json::Value, kid: &str) -> String {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(idp().pem.as_bytes()).unwrap();
        let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        h.kid = Some(kid.into());
        jsonwebtoken::encode(&h, &claims, &key).unwrap()
    }
    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }
    fn good(user: &str, groups: &[&str]) -> serde_json::Value {
        json!({"iss": "https://idp.example.test", "aud": "cambium-test", "exp": now() + 600, "iat": now(), "sub": "id-123", "preferred_username": user, "groups": groups})
    }

    #[test]
    fn oidc_accepts_valid_and_rejects_bad_tokens() {
        let o = oidc();
        let p = o.verify(&jwt(good("dana", &["eng", "pay"]), "test-key-1")).unwrap();
        assert_eq!(p.user, "dana");
        assert_eq!(p.groups, vec!["eng", "pay"]);
        let mut wrong_aud = good("x", &[]);
        wrong_aud["aud"] = json!("other-app");
        assert!(o.verify(&jwt(wrong_aud, "test-key-1")).is_err());
        let mut wrong_iss = good("x", &[]);
        wrong_iss["iss"] = json!("https://evil.example.test");
        assert!(o.verify(&jwt(wrong_iss, "test-key-1")).is_err());
        let mut expired = good("x", &[]);
        expired["exp"] = json!(now() - 3600);
        assert!(o.verify(&jwt(expired, "test-key-1")).is_err());
        assert!(o.verify(&jwt(good("x", &[]), "unknown-kid")).is_err());
        // HS256 token "signed" with the public key text must not be accepted
        let forged = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &good("x", &[]),
            &jsonwebtoken::EncodingKey::from_secret(b"anything"),
        )
        .unwrap();
        assert!(o.verify(&forged).is_err());
    }

    #[test]
    fn oidc_identity_cannot_take_over_static_accounts_or_use_unverified_email() {
        let o = oidc();
        let d = tempfile::tempdir().unwrap();
        let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
        e.create_token("ci-bot").unwrap();
        let t = jwt(good("ci-bot", &["admins"]), "test-key-1");
        assert!(auth::authenticate(&e, Some(&o), &t).is_err(), "OIDC name equal to a static account is refused");
        assert_eq!(auth::authenticate(&e, Some(&o), &jwt(good("dana", &[]), "test-key-1")).unwrap().user, "dana");
        // without preferred_username, an unverified email is skipped in favour of sub
        let mut c = good("x", &[]);
        c.as_object_mut().unwrap().remove("preferred_username");
        c["email"] = json!("boss@example.test");
        c["sub"] = json!("sub-123");
        assert_eq!(o.verify(&jwt(c.clone(), "test-key-1")).unwrap().user, "sub-123");
        c["email_verified"] = json!(true);
        assert_eq!(o.verify(&jwt(c, "test-key-1")).unwrap().user, "boss@example.test");
    }

    #[tokio::test]
    async fn acl_enforced_for_oidc_and_static_identities() {
        let d = tempfile::tempdir().unwrap();
        let e = Arc::new(Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap());
        let admin = e.create_token("owner").unwrap();
        let root = e.create_repo("r", "owner").unwrap();
        let fc = |p: &str, s: &str| FileChange {
            path: p.into(),
            blob: Some(e.blobs.put(s.as_bytes()).unwrap()),
            mode: cambium_core::engine::MODE_FILE,
        };
        let c = e.commit("r", "main", &root, "owner", "seed", vec![fc("pub/a", "a\n"), fc("pay/b", "b\n")]).unwrap();
        let o = Principal::new("owner");
        e.set_acl("r", &o, "", "user:owner", Some(Perm::Admin)).unwrap();
        e.set_acl("r", &o, "", "group:eng", Some(Perm::Read)).unwrap();
        e.set_acl("r", &o, "pay", "group:eng", Some(Perm::None)).unwrap();
        e.set_acl("r", &o, "pay", "group:pay", Some(Perm::Write)).unwrap();
        let app = app(Arc::new(Ctx { eng: e.clone(), oidc: Some(oidc()) }), Arc::new(RateLimiter::new(0.0, 1.0)));
        let post = |tok: &str, uri: &str, body: serde_json::Value| {
            Req::post(uri)
                .header("authorization", format!("Bearer {tok}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let dev = jwt(good("dev", &["eng"]), "test-key-1");
        let payer = jwt(good("payer", &["eng", "pay"]), "test-key-1");
        // tree is filtered
        let r = app.clone().oneshot(post(&dev, "/v1/repos/r/tree", json!({"rev": "main"}))).await.unwrap();
        let v: Value = serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert!(v["entries"]["pub/a"].is_string() && v["entries"]["pay/b"].is_null());
        // hidden blob looks missing
        let pay_blob = e.manifest("r", &c.id).unwrap()["pay/b"].clone();
        assert_eq!(app.clone().oneshot(get(&format!("/v1/blobs/{pay_blob}"), &dev, None)).await.unwrap().status(), 404);
        assert_eq!(app.clone().oneshot(get(&format!("/v1/blobs/{pay_blob}"), &payer, None)).await.unwrap().status(), 200);
        // writes: reader gets 403, pay group may write under pay/
        let blob = e.put_blob_as(b"new\n", &Principal { user: "payer".into(), groups: vec![] }).unwrap();
        let body = |p: &str, head: &str| json!({"ref": "main", "expected": head, "message": "m", "files": [{"path": p, "blob": blob}]});
        assert_eq!(app.clone().oneshot(post(&dev, "/v1/repos/r/commit", body("pub/x", &c.id))).await.unwrap().status(), 403);
        assert_eq!(app.clone().oneshot(post(&payer, "/v1/repos/r/commit", body("pub/x", &c.id))).await.unwrap().status(), 403);
        assert_eq!(app.clone().oneshot(post(&payer, "/v1/repos/r/commit", body("pay/x", &c.id))).await.unwrap().status(), 200);
        // only admins manage rules; static token and OIDC both work
        assert_eq!(
            app.clone()
                .oneshot(post(&dev, "/v1/repos/r/acl", json!({"prefix": "", "principal": "user:dev", "perm": "admin"})))
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            app.clone()
                .oneshot(post(&admin, "/v1/repos/r/acl", json!({"prefix": "pub", "principal": "group:eng", "perm": "write"})))
                .await
                .unwrap()
                .status(),
            200
        );
        let head = e.get_ref("r", "main").unwrap().unwrap();
        // `blob` is now also at pay/x, hidden from dev: dev must upload the bytes to use it
        assert_eq!(app.clone().oneshot(post(&dev, "/v1/repos/r/commit", body("pub/x", &head))).await.unwrap().status(), 400);
        let up = Req::post("/v1/blobs").header("authorization", format!("Bearer {dev}")).body(Body::from("new\n")).unwrap();
        assert_eq!(app.clone().oneshot(up).await.unwrap().status(), 200);
        assert_eq!(app.clone().oneshot(post(&dev, "/v1/repos/r/commit", body("pub/x", &head))).await.unwrap().status(), 200);
        // log hides changesets that only touched pay/
        let r = app.clone().oneshot(get("/v1/repos/r/log?rev=main", &dev, None)).await.unwrap();
        let v: Value = serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert!(v.as_array().unwrap().iter().all(|c| c["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| !f["path"].as_str().unwrap().starts_with("pay/"))));
        // a bad JWT is a 401, not a crash
        assert_eq!(app.oneshot(get("/v1/whoami", "a.b.c", None)).await.unwrap().status(), 401);
    }
}
