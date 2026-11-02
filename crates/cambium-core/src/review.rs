//! Code review: review requests between branches, comments, approvals, and
//! protected branches that only change through an approved review.
use crate::engine::{Changeset, Engine, Error, Perm, Principal, Result};
use crate::p;
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct Review {
    pub id: i64,
    pub repo: String,
    pub source: String,
    pub target: String,
    pub title: String,
    pub author: String,
    pub state: String,
    pub created_at: i64,
    /// approvals that still count (given on the current source head, not by the author)
    pub approvals: Vec<String>,
    pub required: i64,
    /// what the change touches: docs, comments (comment/format-only code), or logic
    pub kind: crate::semantic::ChangeKind,
    /// code-owner sets (from OWNERS files on the target branch) still lacking an approval
    pub owners_pending: Vec<String>,
}
#[derive(Serialize, Clone, Debug)]
pub struct Comment {
    pub id: i64,
    pub author: String,
    pub path: Option<String>,
    pub body: String,
    pub created_at: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

impl Engine {
    /// Approvals needed to change `name` (0 = not protected).
    pub fn required_approvals(&self, repo: &str, name: &str) -> Result<i64> {
        Ok(self.db.one("SELECT required_approvals FROM protected_refs WHERE repo=?1 AND name=?2", p![repo, name])?.map_or(0, |r| r.i(0)))
    }
    /// Approvals needed for a change of `kind` to `name`: protected refs may ask fewer
    /// for docs-only or comment/format-only changes.
    pub fn required_for(&self, repo: &str, name: &str, kind: crate::semantic::ChangeKind) -> Result<i64> {
        let Some(r) =
            self.db.one("SELECT required_approvals,trivial_approvals FROM protected_refs WHERE repo=?1 AND name=?2", p![repo, name])?
        else {
            return Ok(0);
        };
        Ok(match (kind, r.oi(1)) {
            (crate::semantic::ChangeKind::Logic, _) | (_, None) => r.i(0),
            (_, Some(t)) => t.min(r.i(0)),
        })
    }
    pub fn protected_refs(&self, repo: &str) -> Result<Vec<(String, i64)>> {
        Ok(self
            .db
            .query("SELECT name,required_approvals FROM protected_refs WHERE repo=?1 ORDER BY name", p![repo])?
            .into_iter()
            .map(|r| (r.s(0), r.i(1)))
            .collect())
    }
    /// Approvals for trivial (docs/comment-only) changes, per protected ref.
    pub fn trivial_rules(&self, repo: &str) -> Result<Vec<(String, Option<i64>)>> {
        Ok(self
            .db
            .query("SELECT name,trivial_approvals FROM protected_refs WHERE repo=?1 ORDER BY name", p![repo])?
            .into_iter()
            .map(|r| (r.s(0), r.oi(1)))
            .collect())
    }
    /// Set (Some) or clear (None) the approvals needed for docs/comment-only changes
    /// to a protected ref. Same permission as protecting it.
    pub fn set_trivial_approvals(&self, repo: &str, who: &Principal, name: &str, n: Option<i64>) -> Result<()> {
        let az = self.authz(repo, who)?;
        let owner = self.db.one("SELECT owner FROM repos WHERE name=?1", p![repo])?.and_then(|r| r.os(0));
        let ok = if az.open() { owner.as_deref() == Some(who.user.as_str()) } else { az.can("", Perm::Admin) };
        if !ok {
            return Err(Error::Forbidden("admin on / (or the repo owner) to change review rules".into()));
        }
        if n.is_some_and(|x| !(0..=20).contains(&x)) {
            return Err(Error::Bad("approvals must be 0..20".into()));
        }
        if self.required_approvals(repo, name)? == 0 {
            return Err(Error::Bad(format!("{name} is not protected")));
        }
        self.db.exec("UPDATE protected_refs SET trivial_approvals=?1 WHERE repo=?2 AND name=?3", p![n, repo, name])?;
        Ok(())
    }
    /// Kind of the change a review proposes (merge base of target and source vs source).
    pub fn review_kind(&self, repo: &str, source: &str, target: &str) -> Result<crate::semantic::ChangeKind> {
        use crate::semantic::{classify, classify_file};
        let Some(t) = self.get_ref(repo, target)? else { return Ok(crate::semantic::ChangeKind::Logic) };
        let base = self.merge_base(repo, &t, source)?.unwrap_or(t);
        let (mb, ms) = (self.manifest(repo, &base)?, self.manifest(repo, source)?);
        let text = |e: Option<&String>| -> Result<Option<String>> {
            let Some(e) = e else { return Ok(None) };
            let (b, mode) = crate::engine::split_entry(e);
            if mode == crate::engine::MODE_LINK || self.blob_size(b)? > 1 << 20 {
                return Ok(None);
            }
            Ok(self.blobs.get(b)?.and_then(|d| String::from_utf8(d).ok()))
        };
        let mut kinds = vec![];
        for p in mb.keys().chain(ms.keys()).collect::<std::collections::BTreeSet<_>>() {
            let (a, b) = (mb.get(p), ms.get(p));
            if a == b {
                continue;
            }
            // a mode change (e.g. +x) is never trivial
            let mode_changed = a.zip(b).is_some_and(|(x, y)| crate::engine::split_entry(x).1 != crate::engine::split_entry(y).1);
            let k =
                if mode_changed { crate::semantic::ChangeKind::Logic } else { classify_file(p, text(a)?.as_deref(), text(b)?.as_deref()) };
            kinds.push(k);
        }
        Ok(classify(kinds))
    }
    /// Protect a branch (n >= 1) or lift protection (n = 0). Repo admins only
    /// (the owner while the repo has no permission rules).
    pub fn set_protected(&self, repo: &str, who: &Principal, name: &str, n: i64) -> Result<()> {
        let az = self.authz(repo, who)?;
        let owner = self.db.one("SELECT owner FROM repos WHERE name=?1", p![repo])?.and_then(|r| r.os(0));
        let ok = if az.open() { owner.as_deref() == Some(who.user.as_str()) } else { az.can("", Perm::Admin) };
        if !ok {
            return Err(Error::Forbidden("admin on / (or the repo owner) to protect branches".into()));
        }
        if !(0..=20).contains(&n) {
            return Err(Error::Bad("approvals must be 0..20".into()));
        }
        if n == 0 {
            self.db.exec("DELETE FROM protected_refs WHERE repo=?1 AND name=?2", p![repo, name])?;
        } else {
            self.db.exec("INSERT INTO protected_refs(repo,name,required_approvals) VALUES(?1,?2,?3) ON CONFLICT(repo,name) DO UPDATE SET required_approvals=excluded.required_approvals", p![repo, name, n])?;
        }
        Ok(())
    }

    /// Open a review of changeset `source` (any revision; stored as its changeset id)
    /// to land on ref `target`.
    pub fn create_review(&self, repo: &str, source: &str, target: &str, title: &str, author: &str) -> Result<i64> {
        let source = self.resolve(repo, source)?;
        let t = self.get_ref(repo, target)?.ok_or_else(|| Error::NotFound(format!("ref {target}")))?;
        if self.is_ancestor(repo, &source, &t)? {
            return Err(Error::Bad(format!("{} is already in {target}", &source[..12])));
        }
        let source = source.as_str();
        let title = title.trim();
        if title.is_empty() || title.len() > 200 {
            return Err(Error::Bad("title must be 1..200 characters".into()));
        }
        Ok(self
            .db
            .one(
                "INSERT INTO reviews(repo,source,target,title,author,state,created_at) VALUES(?1,?2,?3,?4,?5,'open',?6) RETURNING id",
                p![repo, source, target, title, author, now()],
            )?
            .unwrap()
            .i(0))
    }
    pub fn review(&self, repo: &str, id: i64) -> Result<Review> {
        let r = self
            .db
            .one("SELECT id,repo,source,target,title,author,state,created_at FROM reviews WHERE repo=?1 AND id=?2", p![repo, id])?
            .ok_or_else(|| Error::NotFound(format!("review {id}")))?;
        let head = r.s(2); // the changeset under review
        let approvals = self
            .db
            .query(
                "SELECT user_name FROM review_approvals WHERE review_id=?1 AND head=?2 AND user_name<>?3 ORDER BY user_name",
                p![id, &head, r.s(5)],
            )?
            .into_iter()
            .map(|x| x.s(0))
            .collect();
        Ok(Review {
            id: r.i(0),
            repo: r.s(1),
            source: r.s(2),
            target: r.s(3).clone(),
            title: r.s(4),
            author: r.s(5),
            state: r.s(6),
            created_at: r.i(7),
            approvals,
            kind: crate::semantic::ChangeKind::Logic,
            required: 0,
            owners_pending: vec![],
        })
        .and_then(|mut rv| {
            rv.kind = if rv.state == "open" { self.review_kind(repo, &rv.source, &rv.target)? } else { crate::semantic::ChangeKind::Logic };
            rv.required = self.required_for(repo, &rv.target, rv.kind)?;
            rv.owners_pending = self.owners_pending(&rv)?;
            Ok(rv)
        })
    }
    pub fn reviews(&self, repo: &str, state: Option<&str>) -> Result<Vec<Review>> {
        let ids = match state {
            Some(s) => self.db.query("SELECT id FROM reviews WHERE repo=?1 AND state=?2 ORDER BY id DESC", p![repo, s])?,
            None => self.db.query("SELECT id FROM reviews WHERE repo=?1 ORDER BY id DESC", p![repo])?,
        };
        ids.into_iter().map(|r| self.review(repo, r.i(0))).collect()
    }
    pub fn comments(&self, repo: &str, id: i64) -> Result<Vec<Comment>> {
        self.review(repo, id)?;
        Ok(self
            .db
            .query("SELECT id,author,path,body,created_at FROM review_comments WHERE review_id=?1 ORDER BY id", p![id])?
            .into_iter()
            .map(|r| Comment { id: r.i(0), author: r.s(1), path: r.os(2), body: r.s(3), created_at: r.i(4) })
            .collect())
    }
    pub fn comment(&self, repo: &str, id: i64, author: &str, path: Option<&str>, body: &str) -> Result<i64> {
        self.review(repo, id)?;
        let body = body.trim();
        if body.is_empty() || body.len() > 20_000 {
            return Err(Error::Bad("comment must be 1..20000 characters".into()));
        }
        Ok(self
            .db
            .one(
                "INSERT INTO review_comments(review_id,author,path,body,created_at) VALUES(?1,?2,?3,?4,?5) RETURNING id",
                p![id, author, path.filter(|x| !x.is_empty()), body, now()],
            )?
            .unwrap()
            .i(0))
    }
    /// Approve the review at the source's current head. Authors cannot approve
    /// their own review; a later push to the source makes earlier approvals stale.
    pub fn approve(&self, repo: &str, id: i64, user: &str) -> Result<()> {
        self.approve_as(repo, id, &Principal { user: user.into(), groups: vec![] })
    }
    /// Approve as a full principal; the approver's groups at this moment are
    /// recorded, so group-based code-owner rules can be checked later.
    pub fn approve_as(&self, repo: &str, id: i64, who: &Principal) -> Result<()> {
        let user = who.user.as_str();
        let rv = self.review(repo, id)?;
        if rv.state != "open" {
            return Err(Error::Bad(format!("review is {}", rv.state)));
        }
        if rv.author == user {
            return Err(Error::Forbidden("authors cannot approve their own review".into()));
        }
        let head = rv.source.clone();
        self.db.exec("INSERT INTO review_approvals(review_id,user_name,head,created_at,groups) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(review_id,user_name) DO UPDATE SET head=excluded.head, created_at=excluded.created_at, groups=excluded.groups",
            p![id, user, head, now(), who.groups.join(",")])?;
        Ok(())
    }
    /// Point the review at a new changeset (a revised version). Approvals given on the
    /// previous changeset no longer count.
    pub fn update_review(&self, repo: &str, id: i64, who: &str, cs: &str) -> Result<()> {
        let rv = self.review(repo, id)?;
        if rv.author != who {
            return Err(Error::Forbidden("only the author can update a review".into()));
        }
        if rv.state != "open" {
            return Err(Error::Bad(format!("review is {}", rv.state)));
        }
        let cs = self.resolve(repo, cs)?;
        self.db.exec("UPDATE reviews SET source=?1 WHERE id=?2", p![cs, id])?;
        Ok(())
    }
    pub fn close_review(&self, repo: &str, id: i64, who: &str) -> Result<()> {
        let rv = self.review(repo, id)?;
        if rv.author != who {
            return Err(Error::Forbidden("only the author can close a review".into()));
        }
        self.db.exec("UPDATE reviews SET state='closed' WHERE id=?1 AND state='open'", p![id])?;
        Ok(())
    }
    /// Merge an open review once it has enough approvals. `check` sees the files
    /// the merge would change (permission checks) before anything is written.
    pub fn merge_review(&self, repo: &str, id: i64, who: &str, check: &dyn Fn(&[crate::FileChange]) -> Result<()>) -> Result<Changeset> {
        let rv = self.review(repo, id)?;
        if rv.state != "open" {
            return Err(Error::Bad(format!("review is {}", rv.state)));
        }
        if (rv.approvals.len() as i64) < rv.required {
            return Err(Error::Forbidden(format!(
                "{} needs {} approval(s) on the current head, has {}",
                rv.target,
                rv.required,
                rv.approvals.len()
            )));
        }
        if !rv.owners_pending.is_empty() {
            return Err(Error::Forbidden(format!("code-owner approval missing: {}", rv.owners_pending.join("; "))));
        }
        // land the exact changeset the approvals were counted on
        let head = rv.source.clone();
        let n = self
            .db
            .one("SELECT count(*) FROM review_approvals WHERE review_id=?1 AND head=?2 AND user_name<>?3", p![id, &head, &rv.author])?
            .unwrap()
            .i(0);
        if n < rv.required {
            return Err(Error::Forbidden("source moved while merging; approvals are stale".into()));
        }
        let _ = who;
        let tip = self.land(repo, &head, &rv.target, check)?;
        self.db.exec("UPDATE reviews SET state='merged' WHERE id=?1", p![id])?;
        self.get_changeset(repo, &tip)
    }
}

/// One OWNERS file: who may approve changes under its directory.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Owners {
    pub users: Vec<String>,
    pub groups: Vec<String>,
}

/// `user:alice`, `group:payments-team`, or a bare name (= user). `#` starts a comment.
pub fn parse_owners(text: &str) -> Owners {
    let mut o = Owners::default();
    for line in text.lines() {
        let l = line.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        for tok in l.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
            if let Some(g) = tok.strip_prefix("group:") {
                o.groups.push(g.into());
            } else {
                o.users.push(tok.strip_prefix("user:").unwrap_or(tok).trim_start_matches('@').into());
            }
        }
    }
    o
}
/// Directory of the nearest OWNERS file governing `path` ("" = repo root).
pub fn owners_dir_for(path: &str, has: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut dir = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    loop {
        let f = if dir.is_empty() { "OWNERS".to_string() } else { format!("{dir}/OWNERS") };
        if has(&f) {
            return Some(dir);
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    }
}

impl Engine {
    /// Owner sets that still need an approval for this review. OWNERS files are
    /// read from the *target* branch, so a review cannot change its own rules.
    pub fn owners_pending(&self, rv: &Review) -> Result<Vec<String>> {
        if rv.state != "open" {
            return Ok(vec![]);
        }
        let Some(t) = self.get_ref(&rv.repo, &rv.target)? else { return Ok(vec![]) };
        let s = rv.source.clone();
        let mt = self.manifest(&rv.repo, &t)?;
        if !mt.keys().any(|k| k == "OWNERS" || k.ends_with("/OWNERS")) {
            return Ok(vec![]);
        }
        let base = self.merge_base(&rv.repo, &t, &s)?.unwrap_or(t.clone());
        let changed = Engine::changes_between(&self.manifest(&rv.repo, &base)?, &self.manifest(&rv.repo, &s)?);
        let has = |f: &str| mt.contains_key(f);
        let mut dirs: Vec<String> = changed.iter().filter_map(|c| owners_dir_for(&c.path, &has)).collect();
        dirs.sort();
        dirs.dedup();
        if dirs.is_empty() {
            return Ok(vec![]);
        }
        let approvers: Vec<(String, Vec<String>)> = self
            .db
            .query(
                "SELECT user_name,groups FROM review_approvals WHERE review_id=?1 AND head=?2 AND user_name<>?3",
                p![rv.id, &s, &rv.author],
            )?
            .into_iter()
            .map(|r| (r.s(0), r.s(1).split(',').filter(|g| !g.is_empty()).map(String::from).collect()))
            .collect();
        let mut pending = vec![];
        for d in dirs {
            let f = if d.is_empty() { "OWNERS".to_string() } else { format!("{d}/OWNERS") };
            let id = crate::engine::split_entry(&mt[&f]).0.to_string();
            let text = self.blobs.get(&id)?.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            let o = parse_owners(&text);
            let ok = approvers.iter().any(|(u, gs)| o.users.contains(u) || gs.iter().any(|g| o.groups.contains(g)));
            if !ok {
                let who: Vec<String> =
                    o.users.iter().map(|u| format!("user:{u}")).chain(o.groups.iter().map(|g| format!("group:{g}"))).collect();
                pending.push(format!("{f} ({})", if who.is_empty() { "no owners listed".into() } else { who.join(", ") }));
            }
        }
        Ok(pending)
    }
}
