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
    let (_, az) = reader(&st, &h, &r)?;
    let all = st.reviews(&r, q.state.as_deref())?;
    Ok(Json(json!(all.into_iter().filter(|rv| review_visible(&st, &az, &r, rv)).collect::<Vec<_>>())))
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
/// A review is visible only to people who can read every path it changes, so its title,
/// change kind, code owners and comments never reveal hidden work.
pub(crate) fn review_visible(st: &St, az: &cambium_core::Authz, r: &str, rv: &cambium_core::review::Review) -> bool {
    az.open() || changed_paths(st, r, rv).is_ok_and(|ps| ps.iter().all(|p| az.can(p, Perm::Read)))
}
/// The review, or "not found" when the caller may not see it.
fn visible_review(st: &St, az: &cambium_core::Authz, r: &str, id: i64) -> Result<cambium_core::review::Review, ApiErr> {
    let rv = st.review(r, id)?;
    if review_visible(st, az, r, &rv) {
        Ok(rv)
    } else {
        Err(ApiErr(Error::NotFound(format!("review {id}"))))
    }
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
    let rv = visible_review(&st, &az, &r, id)?;
    let diff = review_diff(&st, &az, &r, &rv)?;
    Ok(Json(json!({"review": rv, "comments": visible_comments(&st, &az, &r, id)?, "diff": diff})))
}
async fn comment(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Json(b): Json<NewComment>) -> R {
    let (u, az) = reader(&st, &h, &r)?;
    visible_review(&st, &az, &r, id)?;
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
/// Logo for the dark header and the browser tab icon (docs/brand/logo).
const LOGO_ON_DARK: &str = include_str!("../../../docs/brand/logo/cambium-on-dark.svg");
const FAVICON: &str = include_str!("../../../docs/brand/logo/favicon.svg");
async fn favicon() -> Response {
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "public, max-age=86400")], FAVICON).into_response()
}
/// The UI's one stylesheet. The page reads as a trunk cross-section: the header is the
/// bark, a thin bright line under it is the cambium (the brand colour, used only where
/// something new happens: focus, primary actions, added lines), the page is sapwood.
const CSS: &str = r#"
:root{--bark:#1d5c45;--bark-deep:#154736;--cambium:#3ccf86;--sap:#f4f7f5;--surface:#fff;--ink:#16201b;--soft:#4f6157;
--rule:#dbe3de;--heart:#8a6a3a;--danger:#b42318;--add-bg:#e8f7ee;--del-bg:#fdecea;--serif:Charter,"Bitstream Charter","Sitka Text",Cambria,Georgia,serif;
--sans:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"Helvetica Neue",sans-serif;--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
@media (prefers-color-scheme:dark){:root{--sap:#0e1a15;--surface:#14241d;--ink:#e3ece6;--soft:#9cb0a5;--rule:#253830;--heart:#c9a36a;
--add-bg:#12301f;--del-bg:#3a1a17;--danger:#f2877c}}
*{box-sizing:border-box}
body{margin:0;font:15px/1.55 var(--sans);color:var(--ink);background:var(--sap)}
a{color:var(--bark)} @media (prefers-color-scheme:dark){a{color:var(--cambium)}}
a:hover{text-decoration-thickness:2px}
:focus-visible{outline:3px solid var(--cambium);outline-offset:2px}
header.site{background:var(--bark);color:#fff;border-bottom:3px solid var(--cambium);padding:.55rem 1.25rem;display:flex;justify-content:space-between;align-items:center;gap:1rem}
header.site a.brand{color:#fff;text-decoration:none;display:flex;align-items:center;gap:.55rem;font:600 1.15rem/1 var(--serif)}
header.site .who{display:flex;align-items:center;gap:.75rem;font-size:14px;color:#e3efe8}
header.site .who button{background:transparent;border:1px solid rgba(255,255,255,.45);color:#fff;padding:.25rem .7rem}
header.site .who button:hover{background:rgba(255,255,255,.1)}
main{max-width:1120px;margin:0 auto;padding:1.75rem 1.25rem 4rem}
h1,h2,h3{font-family:var(--serif);font-weight:600;line-height:1.25;margin:0 0 .75rem}
h1{font-size:1.75rem} h2{font-size:1.45rem} h3{font-size:1.15rem}
p{max-width:75ch}
.muted{color:var(--soft)} .err{color:var(--danger)}
code,pre,.mono{font-family:var(--mono);font-size:13px}
.card{background:var(--surface);border:1px solid var(--rule);border-radius:10px;padding:1.25rem 1.4rem;margin-bottom:1.5rem}
table{width:100%;border-collapse:collapse}
th{font-weight:600;font-size:13px;color:var(--soft);text-align:left;padding:.45rem .6rem;border-bottom:1px solid var(--rule)}
td{padding:.5rem .6rem;border-bottom:1px solid var(--rule);vertical-align:top}
tr:last-child td{border-bottom:0}
table.files td:first-child{white-space:nowrap}
table.files a.entry{display:inline-flex;align-items:center;gap:.5rem;text-decoration:none;color:var(--ink);font-weight:500}
table.files a.entry:hover span{text-decoration:underline}
svg.ico{width:16px;height:16px;flex:none} svg.ico.dir{color:var(--heart)} svg.ico.file{color:var(--soft)}
.repohead{margin-bottom:1.25rem}
.repohead .trail{font-size:14px;color:var(--soft);margin-bottom:.2rem} .repohead .trail a{color:var(--soft)}
.repohead h1{margin:0 0 .7rem}
nav.tabs{display:flex;flex-wrap:wrap;align-items:flex-end;gap:.25rem 1.5rem;border-bottom:1px solid var(--rule)}
nav.tabs a,nav.tabs b{padding:.45rem 0;text-decoration:none;color:var(--soft);font-weight:500;border-bottom:3px solid transparent;margin-bottom:-1px}
nav.tabs b{color:var(--ink);border-bottom-color:var(--cambium)}
nav.tabs a:hover{color:var(--ink)}
nav.tabs .tools{margin-left:auto;display:flex;gap:.5rem;padding-bottom:.4rem;flex-wrap:wrap}
nav.tabs .tools button{background:transparent;color:var(--ink);border:1px solid var(--rule);font-weight:500}
nav.tabs .tools button:hover{background:var(--surface);border-color:var(--soft)}
.path{font-size:15px;margin:0 0 1rem} .path .at{color:var(--soft);font-size:13px;margin-left:.4rem}
.meta{color:var(--soft);font-size:14px;margin:.25rem 0 1rem}
.actions{display:flex;gap:1rem;margin:.25rem 0 1rem;font-size:14px}
table.code{font:13px/1.5 var(--mono);width:auto;min-width:100%}
table.code td{border:0;padding:0 .75rem}
td.ln{color:var(--soft);text-align:right;user-select:none;width:1%;white-space:nowrap} td.ln a{color:inherit;text-decoration:none}
td.code{white-space:pre}
table.code tr:target{background:var(--add-bg)} table.code tr:target td.ln{color:var(--ink);font-weight:600}
.scroll{overflow-x:auto}
pre.diff{background:var(--surface);border:1px solid var(--rule);border-radius:8px;padding:.75rem 0;overflow:auto;font:13px/1.5 var(--mono)}
pre.diff span{display:block;padding:0 1rem}
.add{background:var(--add-bg)} .del{background:var(--del-bg)} .hunk{color:var(--soft)}
.badge{display:inline-block;padding:.05rem .55rem;border-radius:999px;background:var(--sap);border:1px solid var(--rule);font-size:12px;color:var(--soft)}
.ok{background:var(--add-bg);color:var(--ink);border-color:transparent} .warn{background:var(--del-bg);color:var(--ink);border-color:transparent}
input,textarea,select{font:inherit;color:var(--ink);background:var(--surface);padding:.4rem .55rem;border:1px solid var(--rule);border-radius:6px}
textarea{width:100%;min-height:5rem}
button{font:inherit;font-weight:600;background:var(--bark);color:#fff;border:0;border-radius:6px;padding:.42rem .95rem;cursor:pointer}
button:hover{background:var(--bark-deep)} button[disabled]{opacity:.5;cursor:not-allowed}
form.inline{display:inline}
.empty{padding:1.5rem 0;color:var(--soft)}
@media (max-width:640px){main{padding:1.25rem .9rem 3rem} nav.tabs .tools{margin-left:0} td.ln{display:none}}
@media (prefers-reduced-motion:reduce){*{transition:none!important}}
"#;

pub(crate) fn page(title: &str, who: Option<&str>, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t} – Cambium</title><style>{css}</style><link rel=icon type="image/svg+xml" href="/favicon.svg"></head>
<body><header class=site><a href="/ui" class=brand>{logo}Cambium</a><span class=who>{w}</span></header><main>{body}</main></body></html>"#,
        t = esc(title),
        css = CSS,
        logo = LOGO_ON_DARK
            .replacen("<svg ", "<svg width=\"24\" height=\"24\" aria-hidden=\"true\" ", 1)
            .replace("<title>Cambium</title>", ""),
        w = who
            .map(|u| format!(
                "<span>Signed in as {}</span><form class=inline method=post action=/ui/logout><button>Sign out</button></form>",
                esc(u)
            ))
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
pub(crate) fn csrf(token: &str) -> String {
    hash_bytes(format!("cambium-csrf\n{token}").as_bytes())[..32].to_string()
}
/// Signed-in principal for UI pages, or a redirect to the sign-in form.
pub(crate) fn ui_user(st: &St, h: &HeaderMap) -> Result<(Principal, String), Response> {
    let t = cookie_token(h).ok_or_else(|| Redirect::to("/ui/login").into_response())?;
    let u = st.session_principal(&t).map_err(|_| Redirect::to("/ui/login").into_response())?;
    crate::obs::set_actor(&u.user);
    Ok((u, t))
}
pub(crate) fn ui_err(e: impl std::fmt::Display, code: StatusCode) -> Response {
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
pub(crate) fn api_to_ui(e: ApiErr) -> Response {
    let code = match &e.0 {
        Error::Forbidden(_) => StatusCode::FORBIDDEN,
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Unauthorized => StatusCode::UNAUTHORIZED,
        Error::Bad(_) | Error::Conflict(_) | Error::RefMoved { .. } => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ui_err(&e.0, code)
}
pub(crate) fn check_csrf(token: &str, got: &str) -> Result<(), Response> {
    if cambium_core::blob::ct_eq(&csrf(token), got) {
        Ok(())
    } else {
        Err(ui_err("form expired or forged (CSRF check failed); reload the page", StatusCode::FORBIDDEN))
    }
}
pub(crate) fn diff_html(d: &str) -> String {
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
/// Sign-in has no session yet to derive a CSRF value from, so refuse cross-site posts
/// (login CSRF) using the browser's own headers.
fn same_origin(h: &HeaderMap) -> bool {
    let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok());
    // modern browsers: Sec-Fetch-Site says where the form was submitted from (and is not
    // affected by our no-referrer policy, which makes them send `Origin: null`)
    if let Some(site) = hv("sec-fetch-site") {
        return site == "same-origin" || site == "none";
    }
    // older browsers: compare a real Origin with Host; `null` carries no information
    match (hv("origin"), hv("host")) {
        (Some("null"), _) | (None, _) => true,
        (Some(o), Some(host)) => o.split_once("://").is_some_and(|(_, rest)| rest == host),
        (Some(_), None) => false,
    }
}
async fn login(State(st): State<St>, h: HeaderMap, Form(f): Form<Login>) -> Response {
    if !same_origin(&h) {
        return ui_err("sign-in must come from this site", StatusCode::FORBIDDEN);
    }
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
    let (u, tok) = match ui_user(&st, &h) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let mut rows = String::new();
    for r in st.list_repos().unwrap_or_default() {
        let Ok(az) = st.authz(&r, &u) else { continue };
        if !az.can_any(Perm::Read) {
            continue;
        }
        let open = st.reviews(&r, Some("open")).map(|v| v.into_iter().filter(|rv| review_visible(&st, &az, &r, rv)).count()).unwrap_or(0);
        rows.push_str(&format!(
            "<tr><td><a href=\"/ui/{0}/code\">{0}</a></td><td><a href=\"/ui/{0}/history\">history</a></td><td><a href=\"/ui/{0}\">{open} open review(s)</a></td></tr>",
            esc(&r)
        ));
    }
    if rows.is_empty() {
        rows = "<tr><td colspan=3 class=muted>No repositories yet. Create one below, or from a terminal with <code>cmb init &lt;name&gt;</code>; \
                an existing git repository can be imported with <code>cmbd import-git</code>.</td></tr>"
            .into();
    }
    page(
        "Repositories",
        Some(&u.user),
        &format!(
            "<div class=card><h2>Repositories</h2><table><tr><th>Repo</th><th></th><th>Reviews</th></tr>{rows}</table></div>\
             <div class=card><h3>New repository</h3><form method=post action=/ui/repos><input type=hidden name=csrf value=\"{}\">\
             <input name=name placeholder=\"name (letters, digits, - and _)\" required pattern=\"[A-Za-z0-9_-]+\" size=30> <button>Create</button></form></div>",
            csrf(&tok)
        ),
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
    for rv in st.reviews(&r, None).unwrap_or_default().into_iter().filter(|rv| review_visible(&st, &az, &r, rv)) {
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
    page(&r, Some(&u.user), &format!(r#"{nav}<div class=card><h2>{rn} — reviews</h2><p>Protected: {prot}</p>
<table><tr><th>#</th><th>Title</th><th>Branches</th><th>Author</th><th>State</th><th>Approvals</th></tr>{rows}</table></div>
<div class=card><h3>New review</h3><form method=post action="/ui/{rn}/reviews"><input type=hidden name=csrf value="{c}">
<input name=title placeholder=Title required size=40> <select name=source>{branches}</select> → <select name=target>{branches}</select> <button>Open review</button></form></div>"#,
        rn = esc(&r), c = csrf(&tok), nav = crate::browse::nav(&st, &r, "main", "reviews"))).into_response()
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
    let rv = match visible_review(&st, &az, &r, id) {
        Ok(x) => x,
        Err(e) => return api_to_ui(e),
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
<p class=meta><span class="badge {sb}">{state}</span> <span class=badge>{kind}</span> Changeset <code>{s}</code> into <code>{tg}</code>, by {a}. Approvals: <b>{n} of {req}</b> {who}</p>{owners}{actions}</div>
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
    if let Err(e) = visible_review(st, &az, r, id) {
        return api_to_ui(e);
    }
    match f(st, &u, &az) {
        Ok(()) => Redirect::to(&format!("/ui/{r}/reviews/{id}")).into_response(),
        Err(e) => api_to_ui(ApiErr(e)),
    }
}

pub fn ui() -> Router<St> {
    Router::new()
        .route("/ui", get(home))
        .route("/favicon.svg", get(favicon))
        .route("/ui/login", get(login_form).post(login))
        .route("/ui/logout", post(logout))
        .route("/ui/:r", get(repo_page))
        .route("/ui/:r/reviews", post(ui_create))
        .route("/ui/:r/reviews/:id", get(review_page))
        .route("/ui/:r/reviews/:id/comments", post(ui_comment))
        .route("/ui/:r/reviews/:id/approve", post(ui_approve))
        .route("/ui/:r/reviews/:id/merge", post(ui_merge))
}
