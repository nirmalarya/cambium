//! Review API (JSON) and the server-rendered review UI (HTML, no JavaScript).
//! The UI signs in with an API token once; the browser then holds only a random
//! server-side session id (HttpOnly, SameSite=Strict, Secure behind TLS) that
//! expires after CAMBIUM_SESSION_TTL (default 8h). Every form carries a per-session CSRF value.
use crate::{check_writes, need, user, ApiErr, St, R};
use axum::{
    extract::{Form, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use cambium_core::{blob::hash_bytes, engine::Manifest, Error, Perm, Principal};
use serde::Deserialize;
use serde_json::{json, Value};

// ---------------- JSON API ----------------
#[derive(Deserialize)]
pub struct NewReview {
    source: String,
    #[serde(default = "main")]
    target: String,
    title: String,
}
fn main() -> String {
    "main".into()
}
#[derive(Deserialize)]
pub struct StateQ {
    state: Option<String>,
}
#[derive(Deserialize)]
pub struct NewComment {
    body: String,
    path: Option<String>,
}
#[derive(Deserialize)]
pub struct Protect {
    r#ref: String,
    approvals: i64,
    /// approvals for docs/comment-only changes; a negative value clears the rule
    #[serde(default)]
    trivial_approvals: Option<i64>,
}
/// [name, approvals, approvals for docs/comment-only changes (or null)]
fn protection_list(st: &St, r: &str) -> Result<Value, ApiErr> {
    let trivial: std::collections::HashMap<String, Option<i64>> = st.trivial_rules(r)?.into_iter().collect();
    Ok(json!(st.protected_refs(r)?.into_iter().map(|(n, a)| json!([n, a, trivial.get(&n).copied().flatten()])).collect::<Vec<_>>()))
}

fn reader(st: &St, h: &HeaderMap, r: &str) -> Result<(Principal, cambium_core::Authz), ApiErr> {
    let u = user(st, h)?;
    let az = st.authz(r, &u)?;
    need(az.can_any(Perm::Read), "read on this repo")?;
    Ok((u, az))
}
async fn list(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(q): Query<StateQ>) -> R {
    reader(&st, &h, &r)?;
    Ok(Json(json!(st.reviews(&r, q.state.as_deref())?)))
}
async fn create(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<NewReview>) -> R {
    let (u, _) = reader(&st, &h, &r)?;
    let id = st.create_review(&r, &b.source, &b.target, &b.title, &u.user)?;
    Ok(Json(json!(st.review(&r, id)?)))
}
fn review_diff(st: &St, az: &cambium_core::Authz, r: &str, rv: &cambium_core::review::Review) -> Result<String, ApiErr> {
    let s = rv.source.clone(); // the changeset under review
    let t = st.get_ref(r, &rv.target)?.ok_or_else(|| Error::NotFound(format!("ref {}", rv.target)))?;
    let base = st.merge_base(r, &t, &s)?.unwrap_or(t);
    let (mb, ms): (Manifest, Manifest) = (az.filter(st.manifest(r, &base)?), az.filter(st.manifest(r, &s)?));
    Ok(st.diff_manifests(&mb, &ms)?)
}
/// Paths changed by the review (merge base vs source), unfiltered.
fn changed_paths(st: &St, r: &str, rv: &cambium_core::review::Review) -> Result<Vec<String>, ApiErr> {
    let s = rv.source.clone(); // the changeset under review
    let t = st.get_ref(r, &rv.target)?.ok_or_else(|| Error::NotFound(format!("ref {}", rv.target)))?;
    let base = st.merge_base(r, &t, &s)?.unwrap_or(t);
    let (mb, ms) = (st.manifest(r, &base)?, st.manifest(r, &s)?);
    let mut out: Vec<String> = ms.iter().filter(|(k, v)| mb.get(*k) != Some(*v)).map(|(k, _)| k.clone()).collect();
    out.extend(mb.keys().filter(|k| !ms.contains_key(*k)).cloned());
    Ok(out)
}
/// An approval only counts from someone who can see everything it approves.
pub(crate) fn approve_checked(st: &St, az: &cambium_core::Authz, r: &str, id: i64, u: &Principal) -> Result<(), ApiErr> {
    let rv = st.review(r, id)?;
    if changed_paths(st, r, &rv)?.iter().any(|p| !az.can(p, Perm::Read)) {
        return Err(ApiErr(Error::Forbidden("read on every path this review changes".into())));
    }
    st.approve_as(r, id, u)?;
    Ok(())
}
/// Comments on paths the caller cannot read are left out.
fn visible_comments(st: &St, az: &cambium_core::Authz, r: &str, id: i64) -> Result<Vec<cambium_core::review::Comment>, ApiErr> {
    Ok(st.comments(r, id)?.into_iter().filter(|c| c.path.as_deref().is_none_or(|p| p.is_empty() || az.can(p, Perm::Read))).collect())
}
async fn show(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>) -> R {
    let (_, az) = reader(&st, &h, &r)?;
    let rv = st.review(&r, id)?;
    let diff = review_diff(&st, &az, &r, &rv)?;
    Ok(Json(json!({"review": rv, "comments": visible_comments(&st, &az, &r, id)?, "diff": diff})))
}
async fn comment(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Json(b): Json<NewComment>) -> R {
    let (u, az) = reader(&st, &h, &r)?;
    if let Some(p) = b.path.as_deref().filter(|p| !p.is_empty()) {
        need(az.can(p, Perm::Read), &format!("read on {p}"))?;
    }
    Ok(Json(json!({"id": st.comment(&r, id, &u.user, b.path.as_deref(), &b.body)?})))
}
async fn approve(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>) -> R {
    let (u, az) = reader(&st, &h, &r)?;
    approve_checked(&st, &az, &r, id, &u)?;
    Ok(Json(json!(st.review(&r, id)?)))
}
async fn merge(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>) -> R {
    let (u, az) = reader(&st, &h, &r)?;
    Ok(Json(json!(st.merge_review(&r, id, &u.user, &|f| check_writes(&az, f)).map_err(|e| crate::hide_conflicts(e, &az))?)))
}
#[derive(Deserialize)]
pub struct UpdateReview {
    cs: String,
}
async fn update(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Json(b): Json<UpdateReview>) -> R {
    let (u, _) = reader(&st, &h, &r)?;
    st.update_review(&r, id, &u.user, &b.cs)?;
    Ok(Json(json!(st.review(&r, id)?)))
}
async fn close(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>) -> R {
    let (u, _) = reader(&st, &h, &r)?;
    st.close_review(&r, id, &u.user)?;
    Ok(Json(json!(st.review(&r, id)?)))
}
async fn protect(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<Protect>) -> R {
    let u = user(&st, &h)?;
    st.set_protected(&r, &u, &b.r#ref, b.approvals)?;
    if let (Some(t), true) = (b.trivial_approvals, b.approvals > 0) {
        st.set_trivial_approvals(&r, &u, &b.r#ref, (t >= 0).then_some(t))?;
    }
    Ok(Json(protection_list(&st, &r)?))
}
async fn protected(State(st): State<St>, h: HeaderMap, Path(r): Path<String>) -> R {
    reader(&st, &h, &r)?;
    Ok(Json(protection_list(&st, &r)?))
}

pub fn api() -> Router<St> {
    Router::new()
        .route("/v1/repos/:r/reviews", get(list).post(create))
        .route("/v1/repos/:r/reviews/:id", get(show))
        .route("/v1/repos/:r/reviews/:id/comments", post(comment))
        .route("/v1/repos/:r/reviews/:id/approve", post(approve))
        .route("/v1/repos/:r/reviews/:id/merge", post(merge))
        .route("/v1/repos/:r/reviews/:id/close", post(close))
        .route("/v1/repos/:r/reviews/:id/update", post(update))
        .route("/v1/repos/:r/protected", get(protected).post(protect))
}

// ---------------- HTML UI ----------------
const COOKIE: &str = "cambium_session";

pub fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
}
fn page(title: &str, who: Option<&str>, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t} · Cambium</title><style>
body{{font:15px/1.5 system-ui,sans-serif;margin:0;color:#16201b;background:#f3f6f4}}
header{{background:#1d5c45;color:#fff;padding:.6rem 1.2rem;display:flex;justify-content:space-between;align-items:center}}
header a{{color:#fff;text-decoration:none;font-weight:600}} main{{max-width:980px;margin:1.2rem auto;padding:0 1rem}}
.card{{background:#fff;border:1px solid #d5ded9;border-radius:6px;padding:1rem;margin-bottom:1rem}}
table{{width:100%;border-collapse:collapse}} td,th{{text-align:left;padding:.35rem;border-bottom:1px solid #e3e9e5}}
pre.diff{{background:#fbfcfb;border:1px solid #d5ded9;padding:.6rem;overflow:auto;font:13px/1.4 ui-monospace,monospace}}
.add{{color:#1d6b3a}} .del{{color:#b42318}} .hunk{{color:#5b6b63}} .badge{{padding:.1rem .45rem;border-radius:4px;background:#e3e9e5;font-size:12px}}
.ok{{background:#d9efe0;color:#1d6b3a}} .warn{{background:#f8e1df;color:#b42318}}
input,textarea,select{{font:inherit;padding:.35rem;border:1px solid #b9c6bf;border-radius:4px}} textarea{{width:100%;min-height:5rem}}
button{{font:inherit;background:#1d5c45;color:#fff;border:0;border-radius:4px;padding:.4rem .9rem;cursor:pointer}}
.err{{color:#b42318}} form.inline{{display:inline}}
</style></head><body><header><a href="/ui">Cambium · Reviews</a><span>{w}</span></header><main>{body}</main></body></html>"#,
        t = esc(title),
        w = who
            .map(|u| format!("{} · <form class=inline method=post action=/ui/logout><button>Sign out</button></form>", esc(u)))
            .unwrap_or_default()
    ))
}
pub(crate) fn cookie_token(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().strip_prefix(&format!("{COOKIE}=")).map(String::from))
        .next()
}
fn csrf(token: &str) -> String {
    hash_bytes(format!("cambium-csrf\n{token}").as_bytes())[..32].to_string()
}
/// Signed-in principal for UI pages, or a redirect to the sign-in form.
fn ui_user(st: &St, h: &HeaderMap) -> Result<(Principal, String), Response> {
    let t = cookie_token(h).ok_or_else(|| Redirect::to("/ui/login").into_response())?;
    let u = st.session_principal(&t).map_err(|_| Redirect::to("/ui/login").into_response())?;
    crate::obs::set_actor(&u.user);
    Ok((u, t))
}
fn ui_err(e: impl std::fmt::Display, code: StatusCode) -> Response {
    (
        code,
        page(
            "Error",
            None,
            &format!("<div class=card><p class=err>{}</p><p><a href=\"javascript:history.back()\">Back</a></p></div>", esc(&e.to_string())),
        ),
    )
        .into_response()
}
fn api_to_ui(e: ApiErr) -> Response {
    let code = match &e.0 {
        Error::Forbidden(_) => StatusCode::FORBIDDEN,
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Unauthorized => StatusCode::UNAUTHORIZED,
        Error::Bad(_) | Error::Conflict(_) | Error::RefMoved { .. } => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ui_err(&e.0, code)
}
fn check_csrf(token: &str, got: &str) -> Result<(), Response> {
    if cambium_core::blob::ct_eq(&csrf(token), got) {
        Ok(())
    } else {
        Err(ui_err("form expired or forged (CSRF check failed); reload the page", StatusCode::FORBIDDEN))
    }
}
fn diff_html(d: &str) -> String {
    d.lines()
        .map(|l| {
            let c = if l.starts_with("+++") || l.starts_with("---") {
                "hunk"
            } else if l.starts_with('+') {
                "add"
            } else if l.starts_with('-') {
                "del"
            } else if l.starts_with("@@") || l.starts_with("mode ") {
                "hunk"
            } else {
                ""
            };
            format!("<span class=\"{c}\">{}</span>\n", esc(l))
        })
        .collect()
}

async fn login_form() -> Html<String> {
    page(
        "Sign in",
        None,
        r#"<div class=card><h2>Sign in</h2><p>Paste your access token (from your SSO helper or <code>cmbd token</code>).</p>
<form method=post action=/ui/login><input type=password name=token autocomplete=off size=60 required> <button>Sign in</button></form></div>"#,
    )
}
#[derive(Deserialize)]
struct Login {
    token: String,
}
async fn login(State(st): State<St>, Form(f): Form<Login>) -> Response {
    let t = f.token.trim().to_string();
    let who = match crate::auth::authenticate(&st.eng, st.oidc.as_ref(), &t) {
        Ok(p) if !t.is_empty() => p,
        _ => return ui_err("that token was not accepted", StatusCode::UNAUTHORIZED),
    };
    crate::obs::set_actor(&who.user);
    let ttl =
        std::env::var("CAMBIUM_SESSION_TTL").ok().and_then(|v| cambium_core::tokens::parse_ttl(&v).ok().flatten()).unwrap_or(8 * 3600);
    let sid = match st.create_session(&who, ttl, &t, crate::auth::jwt_exp(&t)) {
        Ok(s) => s,
        Err(e) => return api_to_ui(ApiErr(e)),
    };
    ([(header::SET_COOKIE, format!("{COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/ui; Max-Age={ttl}{}", secure()))], Redirect::to("/ui"))
        .into_response()
}
fn secure() -> &'static str {
    if crate::tls_enabled() || std::env::var("CAMBIUM_COOKIE_SECURE").is_ok_and(|v| v == "1") {
        "; Secure"
    } else {
        ""
    }
}
async fn logout(State(st): State<St>, h: HeaderMap) -> Response {
    if let Some(sid) = cookie_token(&h) {
        if let Ok(p) = st.session_principal(&sid) {
            crate::obs::set_actor(&p.user);
        }
        let _ = st.end_session(&sid);
    }
    ([(header::SET_COOKIE, format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/ui; Max-Age=0{}", secure()))], Redirect::to("/ui/login"))
        .into_response()
}
async fn home(State(st): State<St>, h: HeaderMap) -> Response {
    let (u, _) = match ui_user(&st, &h) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut rows = String::new();
    for r in st.list_repos().unwrap_or_default() {
        let Ok(az) = st.authz(&r, &u) else { continue };
        if !az.can_any(Perm::Read) {
            continue;
        }
        let open = st.reviews(&r, Some("open")).map(|v| v.len()).unwrap_or(0);
        rows.push_str(&format!("<tr><td><a href=\"/ui/{0}\">{0}</a></td><td>{open} open</td></tr>", esc(&r)));
    }
    page(
        "Repositories",
        Some(&u.user),
        &format!("<div class=card><h2>Repositories</h2><table><tr><th>Repo</th><th>Reviews</th></tr>{rows}</table></div>"),
    )
    .into_response()
}
async fn repo_page(State(st): State<St>, h: HeaderMap, Path(r): Path<String>) -> Response {
    let (u, tok) = match ui_user(&st, &h) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let az = match st.authz(&r, &u) {
        Ok(a) => a,
        Err(e) => return api_to_ui(ApiErr(e)),
    };
    if !az.can_any(Perm::Read) {
        return ui_err("you do not have read access to this repository", StatusCode::FORBIDDEN);
    }
    let mut rows = String::new();
    for rv in st.reviews(&r, None).unwrap_or_default() {
        let badge = match rv.state.as_str() {
            "merged" => "ok",
            "closed" => "",
            _ => "warn",
        };
        rows.push_str(&format!("<tr><td>#{id}</td><td><a href=\"/ui/{r}/reviews/{id}\">{t}</a></td><td>{s} → {tg}</td><td>{a}</td><td><span class=\"badge {badge}\">{st}</span></td><td>{n}/{req}</td></tr>",
            id = rv.id, r = esc(&r), t = esc(&rv.title), s = esc(&rv.source), tg = esc(&rv.target), a = esc(&rv.author), st = esc(&rv.state), n = rv.approvals.len(), req = rv.required));
    }
    let branches: String = st.list_refs(&r).unwrap_or_default().iter().map(|(n, _, _)| format!("<option>{}</option>", esc(n))).collect();
    let prot: String =
        st.protected_refs(&r).unwrap_or_default().iter().map(|(n, k)| format!("<span class=badge>{} needs {k}</span> ", esc(n))).collect();
    page(&r, Some(&u.user), &format!(r#"<div class=card><h2>{rn} — reviews</h2><p>Protected: {prot}</p>
<table><tr><th>#</th><th>Title</th><th>Branches</th><th>Author</th><th>State</th><th>Approvals</th></tr>{rows}</table></div>
<div class=card><h3>New review</h3><form method=post action="/ui/{rn}/reviews"><input type=hidden name=csrf value="{c}">
<input name=title placeholder=Title required size=40> <select name=source>{branches}</select> → <select name=target>{branches}</select> <button>Open review</button></form></div>"#,
        rn = esc(&r), c = csrf(&tok))).into_response()
}
#[derive(Deserialize)]
struct UiNew {
    csrf: String,
    title: String,
    source: String,
    target: String,
}
async fn ui_create(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Form(f): Form<UiNew>) -> Response {
    let (u, tok) = match ui_user(&st, &h) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(e) = check_csrf(&tok, &f.csrf) {
        return e;
    }
    match st.authz(&r, &u).map(|a| a.can_any(Perm::Read)) {
        Ok(true) => {}
        Ok(false) => return ui_err("no read access", StatusCode::FORBIDDEN),
        Err(e) => return api_to_ui(ApiErr(e)),
    }
    match st.create_review(&r, &f.source, &f.target, &f.title, &u.user) {
        Ok(id) => Redirect::to(&format!("/ui/{r}/reviews/{id}")).into_response(),
        Err(e) => api_to_ui(ApiErr(e)),
    }
}
async fn review_page(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>) -> Response {
    let (u, tok) = match ui_user(&st, &h) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let az = match st.authz(&r, &u) {
        Ok(a) => a,
        Err(e) => return api_to_ui(ApiErr(e)),
    };
    if !az.can_any(Perm::Read) {
        return ui_err("you do not have read access to this repository", StatusCode::FORBIDDEN);
    }
    let rv = match st.review(&r, id) {
        Ok(x) => x,
        Err(e) => return api_to_ui(ApiErr(e)),
    };
    let diff = match review_diff(&st, &az, &r, &rv) {
        Ok(d) => d,
        Err(e) => return api_to_ui(e),
    };
    let comments: String = visible_comments(&st, &az, &r, id)
        .unwrap_or_default()
        .iter()
        .map(|c| {
            format!(
                "<div class=card><b>{}</b>{} <span class=badge>{}</span><p>{}</p></div>",
                esc(&c.author),
                c.path.as_ref().map(|p| format!(" on <code>{}</code>", esc(p))).unwrap_or_default(),
                c.created_at,
                esc(&c.body).replace('\n', "<br>")
            )
        })
        .collect();
    let c = csrf(&tok);
    let ready = rv.approvals.len() as i64 >= rv.required && rv.owners_pending.is_empty();
    let actions = if rv.state == "open" {
        format!(
            r#"<form class=inline method=post action="/ui/{rn}/reviews/{id}/approve"><input type=hidden name=csrf value="{c}"><button>Approve</button></form>
 <form class=inline method=post action="/ui/{rn}/reviews/{id}/merge"><input type=hidden name=csrf value="{c}"><button {dis}>Merge into {tg}</button></form>"#,
            rn = esc(&r),
            tg = esc(&rv.target),
            dis = if ready { "" } else { "disabled title=\"needs more approvals or code-owner sign-off\"" }
        )
    } else {
        String::new()
    };
    page(&format!("#{id} {}", rv.title), Some(&u.user), &format!(r#"<div class=card><h2>#{id} {t}</h2>
<p><code>{s}</code> → <code>{tg}</code> by <b>{a}</b> · <span class="badge {sb}">{state}</span> · <span class=badge>{kind}</span> · approvals <b>{n}/{req}</b> {who}</p>{owners}{actions}</div>
<div class=card><h3>Changes</h3><pre class=diff>{d}</pre></div>
<h3>Comments</h3>{comments}
<div class=card><form method=post action="/ui/{rn}/reviews/{id}/comments"><input type=hidden name=csrf value="{c}">
<input name=path placeholder="file (optional)" size=40><textarea name=body required placeholder="Comment"></textarea><button>Comment</button></form></div>"#,
        t = esc(&rv.title), s = esc(&rv.source), tg = esc(&rv.target), a = esc(&rv.author), state = esc(&rv.state),
        sb = if rv.state == "merged" { "ok" } else { "" }, n = rv.approvals.len(), req = rv.required,
        kind = match rv.kind { cambium_core::semantic::ChangeKind::Docs => "docs only", cambium_core::semantic::ChangeKind::Comments => "comments/format only", cambium_core::semantic::ChangeKind::Logic => "logic" },
        who = if rv.approvals.is_empty() { String::new() } else { format!("({})", esc(&rv.approvals.join(", "))) },
        owners = if rv.owners_pending.is_empty() { String::new() } else {
            format!("<p class=owners>Code-owner approval needed: {}</p>", rv.owners_pending.iter().map(|o| format!("<code>{}</code>", esc(o))).collect::<Vec<_>>().join(", ")) },
        d = if diff.is_empty() { "(no changes)".into() } else { diff_html(&diff) }, rn = esc(&r))).into_response()
}
#[derive(Deserialize)]
struct UiAct {
    csrf: String,
}
#[derive(Deserialize)]
struct UiComment {
    csrf: String,
    body: String,
    #[serde(default)]
    path: String,
}
async fn ui_comment(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Form(f): Form<UiComment>) -> Response {
    ui_act(&st, &h, &r, id, &f.csrf, |st, u, az| {
        let p = f.path.trim();
        if !p.is_empty() && !az.can(p, Perm::Read) {
            return Err(Error::Forbidden(format!("read on {p}")));
        }
        st.comment(&r, id, &u.user, Some(p), &f.body).map(|_| ())
    })
}
async fn ui_approve(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Form(f): Form<UiAct>) -> Response {
    ui_act(&st, &h, &r, id, &f.csrf, |st, u, az| approve_checked(st, az, &r, id, u).map_err(|e| e.0))
}
async fn ui_merge(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Form(f): Form<UiAct>) -> Response {
    ui_act(&st, &h, &r, id, &f.csrf, |st, u, az| {
        st.merge_review(&r, id, &u.user, &|fs| check_writes(az, fs)).map(|_| ()).map_err(|e| crate::hide_conflicts(e, az))
    })
}
fn ui_act(
    st: &St,
    h: &HeaderMap,
    r: &str,
    id: i64,
    got: &str,
    f: impl FnOnce(&St, &Principal, &cambium_core::Authz) -> cambium_core::engine::Result<()>,
) -> Response {
    let (u, tok) = match ui_user(st, h) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(e) = check_csrf(&tok, got) {
        return e;
    }
    let az = match st.authz(r, &u) {
        Ok(a) => a,
        Err(e) => return api_to_ui(ApiErr(e)),
    };
    if !az.can_any(Perm::Read) {
        return ui_err("no read access", StatusCode::FORBIDDEN);
    }
    match f(st, &u, &az) {
        Ok(()) => Redirect::to(&format!("/ui/{r}/reviews/{id}")).into_response(),
        Err(e) => api_to_ui(ApiErr(e)),
    }
}

pub fn ui() -> Router<St> {
    Router::new()
        .route("/ui", get(home))
        .route("/ui/login", get(login_form).post(login))
        .route("/ui/logout", post(logout))
        .route("/ui/:r", get(repo_page))
        .route("/ui/:r/reviews", post(ui_create))
        .route("/ui/:r/reviews/:id", get(review_page))
        .route("/ui/:r/reviews/:id/comments", post(ui_comment))
        .route("/ui/:r/reviews/:id/approve", post(ui_approve))
        .route("/ui/:r/reviews/:id/merge", post(ui_merge))
}
