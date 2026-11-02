//! Code browser for the web UI: repository list, directory and file views, blame,
//! history, changesets, raw downloads and search. Server-rendered HTML, no scripts.
//! Every page applies the same per-directory permissions as the API: entries the
//! signed-in user cannot read are not shown, history hides changesets that only
//! touched hidden paths, and hidden files look the same as missing ones.
use crate::review::{api_to_ui, diff_html, esc, page, ui_err, ui_user};
use crate::{redact, ApiErr, St};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use cambium_core::{Authz, Perm, Principal};
use serde::Deserialize;

pub fn ui() -> Router<St> {
    Router::new()
        .route("/ui/:r/code", get(code))
        .route("/ui/:r/blame", get(blame))
        .route("/ui/:r/history", get(history))
        .route("/ui/:r/cs/:id", get(changeset))
        .route("/ui/:r/raw", get(raw))
        .route("/ui/:r/search", get(search))
}

#[derive(Deserialize, Default)]
struct Loc {
    #[serde(default = "main_rev")]
    rev: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    n: Option<usize>,
}
fn main_rev() -> String {
    "main".into()
}

/// Signed-in user with read access somewhere in `r`, or the page to show instead.
fn reader(st: &St, h: &HeaderMap, r: &str) -> Result<(Principal, Authz), Response> {
    let (u, _) = ui_user(st, h)?;
    let az = st.authz(r, &u).map_err(|e| api_to_ui(ApiErr(e)))?;
    if !az.can_any(Perm::Read) {
        return Err(ui_err("you do not have read access to this repository", StatusCode::FORBIDDEN));
    }
    Ok((u, az))
}
fn not_found() -> Response {
    ui_err("not found", StatusCode::NOT_FOUND)
}
fn clean_path(p: &str) -> Option<String> {
    let p = p.trim_matches('/').to_string();
    (p.is_empty() || cambium_core::engine::valid_path(&p)).then_some(p)
}
pub(crate) fn q(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}
fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}
fn date(ts: i64) -> String {
    cambium_core::audit::rfc3339(ts).chars().take(10).collect()
}

/// Repo navigation bar with a branch selector.
pub(crate) fn nav(st: &St, r: &str, rev: &str, active: &str) -> String {
    let refs: String = st
        .list_refs(r)
        .unwrap_or_default()
        .iter()
        .map(|(n, _, _)| format!("<option{}>{}</option>", if n == rev { " selected" } else { "" }, esc(n)))
        .collect();
    let link = |k: &str, label: &str, href: String| {
        if k == active {
            format!("<b>{label}</b>")
        } else {
            format!("<a href=\"{href}\">{label}</a>")
        }
    };
    format!(
        r#"<div class=repohead><div class=trail><a href="/ui">Repositories</a></div><h1>{rn}</h1>
<nav class=tabs aria-label="Repository">{c} {h} {v} {s}<span class=tools>
<form class=inline method=get action="/ui/{rq}/code"><label class=muted for=rev-pick>Branch</label> <select id=rev-pick name=rev>{refs}</select> <button>Switch</button></form>
<form class=inline method=get action="/ui/{rq}/search" role=search><input type=hidden name=rev value="{revq}"><input name=q placeholder="Search this branch" aria-label="Search code" size=22> <button>Search</button></form></span></nav></div>"#,
        rn = esc(r),
        rq = q(r),
        revq = esc(rev),
        c = link("code", "Code", format!("/ui/{}/code?rev={}", q(r), q(rev))),
        h = link("history", "History", format!("/ui/{}/history?rev={}", q(r), q(rev))),
        v = link("reviews", "Reviews", format!("/ui/{}", q(r))),
        s = link("settings", "Settings", format!("/ui/{}/settings", q(r))),
    )
}
/// "repo / a / b / c" with links to each directory.
fn crumbs(r: &str, rev: &str, path: &str) -> String {
    let mut out = format!("<a href=\"/ui/{}/code?rev={}\">{}</a>", q(r), q(rev), esc(r));
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        out.push_str(&format!(" / <a href=\"/ui/{}/code?rev={}&path={}\">{}</a>", q(r), q(rev), q(&acc), esc(seg)));
    }
    out
}
const ICON_DIR: &str = r#"<svg class="ico dir" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M1.5 3.5A1.5 1.5 0 0 1 3 2h3.3l1.5 1.5H13A1.5 1.5 0 0 1 14.5 5v7.5A1.5 1.5 0 0 1 13 14H3a1.5 1.5 0 0 1-1.5-1.5z"/></svg>"#;
const ICON_FILE: &str = r#"<svg class="ico file" viewBox="0 0 16 16" aria-hidden="true"><path fill="none" stroke="currentColor" stroke-width="1.3" d="M4 1.75h5.2L12.5 5v9.25H4z M9 1.75V5.2h3.5"/></svg>"#;
fn human(n: i64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// Directory listing or file view at `rev:path`.
async fn code(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(l): Query<Loc>) -> Response {
    let (u, az) = match reader(&st, &h, &r) {
        Ok(x) => x,
        Err(p) => return p,
    };
    let Some(path) = clean_path(&l.path) else { return not_found() };
    let Ok(cs) = st.resolve(&r, &l.rev) else { return not_found() };
    let Ok(root) = st.root_tree(&r, &cs) else { return not_found() };
    let ent = if path.is_empty() { None } else { st.lookup(&root, &path).ok().flatten() };
    let is_dir = path.is_empty() || ent.as_ref().is_some_and(|e| e.kind == "tree");
    if is_dir {
        if !az.can_see_dir(&path) {
            return not_found();
        }
        let tid = ent.map_or(root, |e| e.id);
        let Ok(t) = st.get_tree(&tid) else { return not_found() };
        let mut entries: Vec<_> = t
            .entries
            .into_iter()
            .filter(|e| {
                let p = join(&path, &e.name);
                if e.kind == "tree" {
                    az.can_see_dir(&p)
                } else {
                    az.can(&p, Perm::Read)
                }
            })
            .collect();
        entries.sort_by_key(|e| (e.kind != "tree", e.name.clone()));
        let mut rows = String::new();
        let mut readme = None;
        for (i, e) in entries.iter().enumerate() {
            let p = join(&path, &e.name);
            // last change per entry (path history index); bounded for huge directories
            // last change this user may see (changesets touching only hidden files are skipped)
            let last = if i < 200 {
                st.log(&r, &cs, Some(&p), if az.open() { 1 } else { 20 }).ok().and_then(|v| v.into_iter().find_map(|c| redact(&az, c)))
            } else {
                None
            };
            let last = last.map_or(String::new(), |c| {
                format!(
                    "<a href=\"/ui/{}/cs/{}\" class=muted>{}</a> <span class=muted>on {}</span>",
                    q(&r),
                    q(&c.id),
                    esc(c.message.lines().next().unwrap_or("")),
                    date(c.created_at)
                )
            });
            let (icon, size) = if e.kind == "tree" { (ICON_DIR, String::new()) } else { (ICON_FILE, human(e.size)) };
            rows.push_str(&format!(
                "<tr><td><a class=entry href=\"/ui/{}/code?rev={}&path={}\">{icon}<span>{}{}</span></a></td><td class=muted>{size}</td><td>{last}</td></tr>",
                q(&r),
                q(&l.rev),
                q(&p),
                esc(&e.name),
                if e.kind == "tree" { "/" } else { "" }
            ));
            if e.kind != "tree"
                && readme.is_none()
                && ["README.md", "README", "README.txt", "readme.md"].contains(&e.name.as_str())
                && e.size <= 256 * 1024
            {
                readme = st.read_text(&e.id).ok().map(|t| (e.name.clone(), t));
            }
        }
        if rows.is_empty() {
            rows = "<tr><td class=empty>Nothing here yet. Add files with <code>cmb add</code> and land them with <code>cmb commit</code>.</td></tr>".into();
        }
        let readme = readme.map_or(String::new(), |(n, t)| {
            format!(
                "<div class=card><h3>{}</h3><pre style=\"white-space:pre-wrap;font:14px/1.6 var(--sans)\">{}</pre></div>",
                esc(&n),
                esc(&t)
            )
        });
        let body = format!(
            "{}<p class=path>{} <span class=at>at <code>{}</code></span></p><div class=card style=\"padding:.4rem .8rem\"><table class=files>{rows}</table></div>{readme}",
            nav(&st, &r, &l.rev, "code"),
            crumbs(&r, &l.rev, &path),
            short(&cs)
        );
        return page(&format!("{r}/{path}"), Some(&u.user), &body).into_response();
    }
    // a file
    let Some(e) = ent.filter(|_| az.can(&path, Perm::Read)) else { return not_found() };
    let actions = format!(
        "<a href=\"/ui/{rq}/blame?rev={rv}&path={pq}\">Blame</a> <a href=\"/ui/{rq}/history?rev={rv}&path={pq}\">History</a> <a href=\"/ui/{rq}/raw?rev={rv}&path={pq}\">Download</a>",
        rq = q(&r),
        rv = q(&l.rev),
        pq = q(&path)
    );
    let content = if e.kind == "link" {
        format!("<p>Symbolic link to <code>{}</code></p>", esc(&st.read_text(&e.id).unwrap_or_default()))
    } else if e.size > 1 << 20 {
        format!("<p class=muted>File too large to display ({}); use Raw.</p>", human(e.size))
    } else {
        match st.blobs.get(&e.id) {
            Ok(Some(d)) if !d.contains(&0) && std::str::from_utf8(&d).is_ok() => lines_table(std::str::from_utf8(&d).unwrap(), None),
            Ok(Some(d)) => format!("<p class=muted>Binary file ({}); use Raw to download.</p>", human(d.len() as i64)),
            _ => return not_found(),
        }
    };
    let body = format!(
        "{}<p class=path>{} <span class=at>{}, {}, at <code>{}</code></span></p><div class=actions>{actions}</div><div class=\"card scroll\" style=\"padding:.6rem 0\">{content}</div>",
        nav(&st, &r, &l.rev, "code"),
        crumbs(&r, &l.rev, &path),
        esc(&e.kind),
        human(e.size),
        short(&cs)
    );
    page(&format!("{r}/{path}"), Some(&u.user), &body).into_response()
}
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}
/// Numbered lines; `gutter` adds a per-line HTML column (blame).
fn lines_table(text: &str, gutter: Option<&[String]>) -> String {
    let mut out = String::from("<table class=code>");
    for (i, line) in text.lines().enumerate() {
        let g = gutter.map_or(String::new(), |g| format!("<td class=ln>{}</td>", g.get(i).cloned().unwrap_or_default()));
        out.push_str(&format!(
            "<tr id=L{n}>{g}<td class=ln><a href=\"#L{n}\">{n}</a></td><td class=code>{}</td></tr>",
            esc(line),
            n = i + 1
        ));
    }
    out.push_str("</table>");
    out
}

async fn blame(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(l): Query<Loc>) -> Response {
    let (u, az) = match reader(&st, &h, &r) {
        Ok(x) => x,
        Err(p) => return p,
    };
    let Some(path) = clean_path(&l.path).filter(|p| !p.is_empty() && az.can(p, Perm::Read)) else { return not_found() };
    let Ok(cs) = st.resolve(&r, &l.rev) else { return not_found() };
    let Ok(lines) = st.blame(&r, &cs, &path) else { return not_found() };
    let mut seen: std::collections::HashMap<String, (String, i64)> = Default::default();
    let mut gutter = vec![];
    let mut prev = String::new();
    for (c, _) in &lines {
        if c == &prev {
            gutter.push(String::new());
            continue;
        }
        prev = c.clone();
        let (author, ts) =
            seen.entry(c.clone()).or_insert_with(|| st.get_changeset(&r, c).map(|x| (x.author, x.created_at)).unwrap_or_default()).clone();
        gutter.push(format!(
            "<a href=\"/ui/{}/cs/{}\">{}</a> {} {}",
            q(&r),
            q(c),
            short(c),
            esc(author.split(" <").next().unwrap_or("")),
            date(ts)
        ));
    }
    let text: String = lines.iter().map(|(_, t)| format!("{t}\n")).collect();
    let body = format!(
        "{}<div class=card><p>Blame: {} <span class=muted>at {}</span></p>{}</div>",
        nav(&st, &r, &l.rev, "code"),
        crumbs(&r, &l.rev, &path),
        short(&cs),
        lines_table(&text, Some(&gutter))
    );
    page(&format!("blame {path}"), Some(&u.user), &body).into_response()
}

async fn history(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(l): Query<Loc>) -> Response {
    let (u, az) = match reader(&st, &h, &r) {
        Ok(x) => x,
        Err(p) => return p,
    };
    let Some(path) = clean_path(&l.path) else { return not_found() };
    if !path.is_empty() && !az.can_see_dir(&path) && !az.can(&path, Perm::Read) {
        return not_found();
    }
    let Ok(cs) = st.resolve(&r, &l.rev) else { return not_found() };
    let n = l.n.unwrap_or(100).clamp(1, 500);
    let p = (!path.is_empty()).then_some(path.as_str());
    let raw = st.log(&r, &cs, p, if az.open() { n } else { n * 10 }).unwrap_or_default();
    let list: Vec<_> = raw.into_iter().filter_map(|c| redact(&az, c)).take(n).collect();
    let mut rows = String::new();
    for c in &list {
        rows.push_str(&format!(
            "<tr><td><a href=\"/ui/{}/cs/{}\"><code>{}</code></a></td><td>{}</td><td class=muted>{}</td><td class=muted>{}</td></tr>",
            q(&r),
            q(&c.id),
            short(&c.id),
            esc(c.message.lines().next().unwrap_or("")),
            esc(c.author.split(" <").next().unwrap_or("")),
            date(c.created_at)
        ));
    }
    let older = match list.last().and_then(|c| c.parents.first()) {
        Some(p1) if list.len() == n => format!("<p><a href=\"/ui/{}/history?rev={}&path={}\">Older →</a></p>", q(&r), q(p1), q(&path)),
        _ => String::new(),
    };
    let title = if path.is_empty() { String::new() } else { format!(" of {}", crumbs(&r, &l.rev, &path)) };
    let body = format!(
        "{}<div class=card><h3>History{title}</h3><table><tr><th>Changeset</th><th>Message</th><th>Author</th><th>Date</th></tr>{rows}</table>{older}</div>",
        nav(&st, &r, &l.rev, "history")
    );
    page(&format!("history {r}"), Some(&u.user), &body).into_response()
}

async fn changeset(State(st): State<St>, h: HeaderMap, Path((r, id)): Path<(String, String)>) -> Response {
    let (u, az) = match reader(&st, &h, &r) {
        Ok(x) => x,
        Err(p) => return p,
    };
    let Ok(c) = st.get_changeset(&r, &id) else { return not_found() };
    let Some(c) = redact(&az, c) else { return not_found() };
    let changed: std::collections::BTreeSet<&str> = c.files.iter().map(|f| f.path.as_str()).collect();
    let only = |m: cambium_core::engine::Manifest| -> cambium_core::engine::Manifest {
        az.filter(m).into_iter().filter(|(p, _)| changed.contains(p.as_str())).collect()
    };
    let before = match c.parents.first() {
        Some(p1) => st.manifest(&r, p1).map(only).unwrap_or_default(),
        None => Default::default(),
    };
    let after = st.manifest(&r, &c.id).map(only).unwrap_or_default();
    let diff = st.diff_manifests(&before, &after).unwrap_or_default();
    let parents: String =
        c.parents.iter().map(|p| format!("<a href=\"/ui/{}/cs/{}\"><code>{}</code></a> ", q(&r), q(p), short(p))).collect();
    let files: String = c
        .files
        .iter()
        .map(|f| {
            let what = if f.blob.is_some() { "M/A" } else { "D" };
            format!(
                "<li><span class=badge>{what}</span> <a href=\"/ui/{}/code?rev={}&path={}\">{}</a></li>",
                q(&r),
                q(&c.id),
                q(&f.path),
                esc(&f.path)
            )
        })
        .collect();
    let body = format!(
        "{}<div class=card><h2>{}</h2><pre style=\"white-space:pre-wrap;font:14px/1.6 var(--sans)\">{}</pre><p class=meta>By {} on {}. Changeset <code>{}</code>, parents {parents}. <a href=\"/ui/{}/code?rev={}\">Browse files at this changeset</a></p><ul>{files}</ul></div><div class=card><pre class=diff>{}</pre></div>",
        nav(&st, &r, "main", "history"),
        esc(c.message.lines().next().unwrap_or("")),
        esc(c.message.split_once('\n').map_or("", |x| x.1).trim()),
        esc(&c.author),
        cambium_core::audit::rfc3339(c.created_at),
        c.id,
        q(&r),
        q(&c.id),
        if diff.is_empty() { "(no visible changes)".into() } else { diff_html(&diff) }
    );
    page(&format!("changeset {}", short(&c.id)), Some(&u.user), &body).into_response()
}

/// File bytes as a download: never rendered by the browser (no HTML/script injection).
async fn raw(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(l): Query<Loc>) -> Response {
    let (_, az) = match reader(&st, &h, &r) {
        Ok(x) => x,
        Err(p) => return p,
    };
    let Some(path) = clean_path(&l.path).filter(|p| !p.is_empty() && az.can(p, Perm::Read)) else { return not_found() };
    let Ok(cs) = st.resolve(&r, &l.rev) else { return not_found() };
    let Some(e) = st.root_tree(&r, &cs).ok().and_then(|root| st.lookup(&root, &path).ok().flatten()).filter(|e| e.kind != "tree") else {
        return not_found();
    };
    let Ok(Some(data)) = st.blobs.get(&e.id) else { return not_found() };
    let name: String =
        path.rsplit('/').next().unwrap_or("file").chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        data,
    )
        .into_response()
}

async fn search(State(st): State<St>, h: HeaderMap, Path(r): Path<String>, Query(l): Query<Loc>) -> Response {
    let (u, az) = match reader(&st, &h, &r) {
        Ok(x) => x,
        Err(p) => return p,
    };
    let query = l.q.trim().to_string();
    let mut rows = String::new();
    let mut count = 0;
    if !query.is_empty() {
        let Ok(cs) = st.resolve(&r, &l.rev) else { return not_found() };
        let m = az.filter(st.manifest(&r, &cs).unwrap_or_default());
        let st2 = st.clone();
        let q2 = query.clone();
        let hits = tokio::task::spawn_blocking(move || st2.search(&m, &q2, 200)).await.ok().and_then(|x| x.ok()).unwrap_or_default();
        count = hits.len();
        for hit in hits {
            let anchor = if hit.line > 0 { format!("#L{}", hit.line) } else { String::new() };
            rows.push_str(&format!(
                "<tr><td><a href=\"/ui/{}/code?rev={}&path={}{anchor}\">{}{}</a></td><td class=code>{}</td></tr>",
                q(&r),
                q(&l.rev),
                q(&hit.path),
                esc(&hit.path),
                if hit.line > 0 { format!(":{}", hit.line) } else { String::new() },
                esc(&hit.text)
            ));
        }
    }
    let body = format!(
        "{}<div class=card><form method=get><input type=hidden name=rev value=\"{}\"><input name=q value=\"{}\" size=40> <button>Search</button></form><p class=muted>{count} result(s)</p><table>{rows}</table></div>",
        nav(&st, &r, &l.rev, "search"),
        esc(&l.rev),
        esc(&query)
    );
    page(&format!("search {r}"), Some(&u.user), &body).into_response()
}
