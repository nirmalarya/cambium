//! Repository settings in the web UI (admins): permission rules, branch protection,
//! webhooks; plus creating repositories. Every form carries the session's CSRF value
//! and calls the same engine functions as the API, so the same admin rules apply.
use crate::review::{api_to_ui, check_csrf, csrf, esc, page, ui_err, ui_user};
use crate::{ApiErr, St};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use cambium_core::{Error, Perm, Principal};
use serde::Deserialize;

pub fn ui() -> Router<St> {
    Router::new()
        .route("/ui/repos", post(create_repo))
        .route("/ui/:r/settings", get(settings))
        .route("/ui/:r/settings/acl", post(set_rule))
        .route("/ui/:r/settings/protect", post(protect))
        .route("/ui/:r/settings/hooks", post(add_hook))
        .route("/ui/:r/settings/hooks/:id/delete", post(remove_hook))
}

fn is_admin(st: &St, r: &str, u: &Principal) -> bool {
    let Ok(az) = st.authz(r, u) else { return false };
    if az.open() {
        st.db.one("SELECT owner FROM repos WHERE name=?1", cambium_core::p![r]).ok().flatten().and_then(|x| x.os(0)).as_deref()
            == Some(u.user.as_str())
    } else {
        az.can("", Perm::Admin)
    }
}
fn back(r: &str) -> Response {
    Redirect::to(&format!("/ui/{}/settings", crate::browse::q(r))).into_response()
}
fn done(r: &str, res: Result<(), Error>) -> Response {
    match res {
        Ok(()) => back(r),
        Err(e) => api_to_ui(ApiErr(e)),
    }
}
/// Signed in and the form carries this session's CSRF value.
fn form_user(st: &St, h: &HeaderMap, got: &str) -> Result<Principal, Response> {
    let (u, tok) = ui_user(st, h)?;
    check_csrf(&tok, got)?;
    Ok(u)
}

#[derive(Deserialize)]
struct NewRepo {
    csrf: String,
    name: String,
}
async fn create_repo(State(st): State<St>, h: HeaderMap, Form(f): Form<NewRepo>) -> Response {
    let u = match form_user(&st, &h, &f.csrf) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if !crate::may_create_repo(&u) {
        return ui_err("you may not create repositories here", StatusCode::FORBIDDEN);
    }
    match st.create_repo(f.name.trim(), &u.user) {
        Ok(_) => Redirect::to(&format!("/ui/{}/code", crate::browse::q(f.name.trim()))).into_response(),
        Err(e) => api_to_ui(ApiErr(e)),
    }
}

async fn settings(State(st): State<St>, h: HeaderMap, Path(r): Path<String>) -> Response {
    let (u, tok) = match ui_user(&st, &h) {
        Ok(x) => x,
        Err(p) => return p,
    };
    if !is_admin(&st, &r, &u) {
        return ui_err("repository settings are for repo admins", StatusCode::FORBIDDEN);
    }
    let c = csrf(&tok);
    let rq = crate::browse::q(&r);
    let rules: String = st
        .acl(&r)
        .unwrap_or_default()
        .iter()
        .map(|a| {
            format!(
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td><form class=inline method=post action=\"/ui/{rq}/settings/acl\"><input type=hidden name=csrf value=\"{c}\"><input type=hidden name=prefix value=\"{0}\"><input type=hidden name=principal value=\"{1}\"><input type=hidden name=perm value=remove><button>Remove</button></form></td></tr>",
                esc(if a.prefix.is_empty() { "/" } else { &a.prefix }),
                esc(&a.principal),
                a.perm.as_str()
            )
        })
        .collect();
    let trivial: std::collections::HashMap<String, Option<i64>> = st.trivial_rules(&r).unwrap_or_default().into_iter().collect();
    let prot: String = st
        .protected_refs(&r)
        .unwrap_or_default()
        .iter()
        .map(|(n, k)| {
            let t = trivial.get(n).copied().flatten().map_or("same".into(), |t| t.to_string());
            format!("<tr><td>{}</td><td>{k}</td><td>{t}</td></tr>", esc(n))
        })
        .collect();
    let hooks: String = st
        .list_hooks(&r, &u)
        .unwrap_or_default()
        .iter()
        .map(|w| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form class=inline method=post action=\"/ui/{rq}/settings/hooks/{}/delete\"><input type=hidden name=csrf value=\"{c}\"><button>Delete</button></form></td></tr>",
                w.id,
                esc(&w.url),
                w.failures,
                esc(w.last_error.as_deref().unwrap_or("")),
                w.id
            )
        })
        .collect();
    let refs: String = st.list_refs(&r).unwrap_or_default().iter().map(|(n, _, _)| format!("<option>{}</option>", esc(n))).collect();
    let body = format!(
        r#"{nav}
<div class=card><h3>Permissions</h3><p class=muted>Longest matching prefix wins. With no rules the repository is open (everyone may write).</p>
<table><tr><th>Prefix</th><th>Who</th><th>Permission</th><th></th></tr>{rules}</table>
<form method=post action="/ui/{rq}/settings/acl"><input type=hidden name=csrf value="{c}">
<input name=prefix placeholder="path prefix (empty = whole repo)" size=28> <input name=principal placeholder="user:name, group:name or *" required size=24>
<select name=perm><option>read</option><option>write</option><option>admin</option><option>none</option></select> <button>Set rule</button></form></div>
<div class=card><h3>Branch protection</h3><p class=muted>Protected branches only change through approved reviews.</p>
<table><tr><th>Branch</th><th>Approvals</th><th>For docs/comment-only changes</th></tr>{prot}</table>
<form method=post action="/ui/{rq}/settings/protect"><input type=hidden name=csrf value="{c}">
<select name=refname>{refs}</select> approvals <input name=approvals type=number min=0 max=20 value=1 style="width:4em"> docs/comment-only <input name=trivial placeholder="same" size=5> <button>Save</button>
<span class=muted>(0 approvals lifts protection)</span></form></div>
<div class=card><h3>Webhooks</h3><p class=muted>Every move of main and release branches is POSTed, signed with the secret (see docs/deploy.md).</p>
<table><tr><th>#</th><th>URL</th><th>Failures</th><th>Last error</th><th></th></tr>{hooks}</table>
<form method=post action="/ui/{rq}/settings/hooks"><input type=hidden name=csrf value="{c}">
<input name=url placeholder="https://ci.example/hook" required size=36> <input name=secret type=password placeholder="secret (16+ characters)" required minlength=16 size=24> <button>Add webhook</button></form></div>"#,
        nav = crate::browse::nav(&st, &r, "main", "settings"),
    );
    page(&format!("settings {r}"), Some(&u.user), &body).into_response()
}

#[derive(Deserialize)]
struct Rule {
    csrf: String,
    #[serde(default)]
    prefix: String,
    principal: String,
    perm: String,
}
async fn set_rule(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Form(f): Form<Rule>) -> Response {
    let u = match form_user(&st, &h, &f.csrf) {
        Ok(u) => u,
        Err(p) => return p,
    };
    let perm = if f.perm == "remove" {
        None
    } else {
        match Perm::parse(&f.perm) {
            Some(p) => Some(p),
            None => return ui_err("unknown permission", StatusCode::BAD_REQUEST),
        }
    };
    let prefix = f.prefix.trim().trim_matches('/');
    done(&r, st.set_acl(&r, &u, if prefix == "/" { "" } else { prefix }, f.principal.trim(), perm))
}

#[derive(Deserialize)]
struct Protect {
    csrf: String,
    refname: String,
    approvals: i64,
    #[serde(default)]
    trivial: String,
}
async fn protect(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Form(f): Form<Protect>) -> Response {
    let u = match form_user(&st, &h, &f.csrf) {
        Ok(u) => u,
        Err(p) => return p,
    };
    let res = st.set_protected(&r, &u, &f.refname, f.approvals).and_then(|_| {
        if f.approvals == 0 {
            return Ok(());
        }
        let t = f.trivial.trim();
        let t = if t.is_empty() || t == "same" {
            None
        } else {
            Some(t.parse::<i64>().map_err(|_| Error::Bad("docs/comment-only approvals must be a number".into()))?)
        };
        st.set_trivial_approvals(&r, &u, &f.refname, t)
    });
    done(&r, res)
}

#[derive(Deserialize)]
struct Hook {
    csrf: String,
    url: String,
    secret: String,
}
async fn add_hook(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Form(f): Form<Hook>) -> Response {
    let u = match form_user(&st, &h, &f.csrf) {
        Ok(u) => u,
        Err(p) => return p,
    };
    if let Err(e) = crate::hooks::target_allowed(f.url.trim()) {
        return ui_err(e, StatusCode::BAD_REQUEST);
    }
    done(&r, st.add_hook(&r, &u, f.url.trim(), &f.secret).map(|_| ()))
}

#[derive(Deserialize)]
struct Csrf {
    csrf: String,
}
async fn remove_hook(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, i64)>, Form(f): Form<Csrf>) -> Response {
    let u = match form_user(&st, &h, &f.csrf) {
        Ok(u) => u,
        Err(p) => return p,
    };
    done(&r, st.remove_hook(&r, &u, id))
}
