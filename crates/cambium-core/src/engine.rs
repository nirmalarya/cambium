use crate::blob::{hash_bytes, BlobStore};
use crate::db::Db;
use crate::p;
use crate::text;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// path -> entry. An entry is a blob id, with ":x" for executables and ":l" for symlinks.
pub type Manifest = BTreeMap<String, String>;

pub const MODE_FILE: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
/// Symbolic link; the blob holds the link target.
pub const MODE_LINK: u32 = 0o120000;
fn default_mode() -> u32 {
    MODE_FILE
}

/// Manifest entry for a blob and mode.
pub fn entry(blob: &str, mode: u32) -> String {
    match mode {
        MODE_EXEC => format!("{blob}:x"),
        MODE_LINK => format!("{blob}:l"),
        _ => blob.to_string(),
    }
}
/// Split an entry back into (blob id, mode).
pub fn split_entry(e: &str) -> (&str, u32) {
    if let Some(b) = e.strip_suffix(":x") {
        return (b, MODE_EXEC);
    }
    if let Some(b) = e.strip_suffix(":l") {
        return (b, MODE_LINK);
    }
    (e, MODE_FILE)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("ref moved: expected {expected:?}, found {found:?}")]
    RefMoved { expected: Option<String>, found: Option<String> },
    #[error("merge conflict in: {0:?}")]
    Conflict(Vec<String>),
    #[error("bad request: {0}")]
    Bad(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("storage: {0}")]
    Db(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}
impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Self {
        Error::Db(e.as_db_error().map_or_else(|| e.to_string(), |d| format!("{}: {}", d.code().code(), d.message())))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileChange {
    pub path: String,
    /// None means delete.
    pub blob: Option<String>,
    /// 0o100644 (regular) or 0o100755 (executable). Ignored for deletes.
    #[serde(default = "default_mode")]
    pub mode: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Changeset {
    pub id: String,
    pub seq: i64,
    pub parents: Vec<String>,
    pub author: String,
    pub message: String,
    pub created_at: i64,
    pub files: Vec<FileChange>,
}

/// Who is making a request: a user plus the groups their identity carries.
#[derive(Clone, Debug, Default)]
pub struct Principal {
    pub user: String,
    pub groups: Vec<String>,
}
impl Principal {
    pub fn new(user: &str) -> Self {
        Self { user: user.into(), groups: vec![] }
    }
    fn names(&self) -> Vec<String> {
        let mut v = vec!["*".to_string(), format!("user:{}", self.user)];
        v.extend(self.groups.iter().map(|g| format!("group:{g}")));
        v
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Perm {
    None,
    Read,
    Write,
    Admin,
}
impl Perm {
    pub fn parse(s: &str) -> Option<Perm> {
        Some(match s {
            "none" => Perm::None,
            "read" => Perm::Read,
            "write" => Perm::Write,
            "admin" => Perm::Admin,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Perm::None => "none",
            Perm::Read => "read",
            Perm::Write => "write",
            Perm::Admin => "admin",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AclRule {
    pub prefix: String,
    pub principal: String,
    pub perm: Perm,
}

/// Permissions of one principal in one repo. Open repos (no rules) grant write everywhere.
pub struct Authz {
    rules: Vec<AclRule>,
    names: Vec<String>,
}
fn under(prefix: &str, path: &str) -> bool {
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}
impl Authz {
    pub fn open(&self) -> bool {
        self.rules.is_empty()
    }
    /// Longest matching prefix that names this principal wins; ties take the highest grant.
    pub fn perm(&self, path: &str) -> Perm {
        if self.open() {
            return Perm::Write;
        }
        let mine: Vec<&AclRule> = self.rules.iter().filter(|r| self.names.contains(&r.principal) && under(&r.prefix, path)).collect();
        let Some(len) = mine.iter().map(|r| r.prefix.len()).max() else { return Perm::None };
        mine.iter().filter(|r| r.prefix.len() == len).map(|r| r.perm).max().unwrap_or(Perm::None)
    }
    pub fn can(&self, path: &str, p: Perm) -> bool {
        self.perm(path) >= p
    }
    /// May the principal see directory `dir` (to list it): read on it, or read on
    /// something beneath it.
    pub fn can_see_dir(&self, dir: &str) -> bool {
        self.can(dir, Perm::Read)
            || self.rules.iter().any(|r| self.names.contains(&r.principal) && under(dir, &r.prefix) && self.can(&r.prefix, Perm::Read))
    }
    /// True if the principal has at least `p` somewhere in the repo.
    pub fn can_any(&self, p: Perm) -> bool {
        self.open() || self.can("", p) || self.rules.iter().any(|r| self.names.contains(&r.principal) && self.can(&r.prefix, p))
    }
    pub fn require(&self, path: &str, p: Perm) -> Result<()> {
        if self.can(path, p) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!("{} on {}", p.as_str(), if path.is_empty() { "/" } else { path })))
        }
    }
    pub fn filter(&self, m: Manifest) -> Manifest {
        m.into_iter().filter(|(p, _)| self.can(p, Perm::Read)).collect()
    }
}

pub const SCHEMA: &str = include_str!("../../../docs/schema.sql");

enum Step {
    Go,
    Prune,
    Stop,
}

pub struct Engine {
    pub db: Db,
    pub blobs: Arc<dyn BlobStore>,
    /// recently built manifests (read from trees); bounded, cleared when full
    manifests: std::sync::Mutex<std::collections::HashMap<String, Arc<Manifest>>>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Ref names allowed through the API: `main` and `release/<name>` (the branch model:
/// no feature branches; work moves as changesets and reviews).
pub fn valid_ref_name(name: &str) -> bool {
    name == "main"
        || name.strip_prefix("release/").is_some_and(|r| {
            !r.is_empty() && r.len() <= 100 && r.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !r.starts_with('.')
        })
}

pub fn valid_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && s != ".cambium")
}

/// If `path` is a file in `m` but one of its parent directories is also a file, or other files
/// live under `path/`, return the conflicting path. Such trees cannot be laid out on disk, and a
/// symlink at a parent position would let a client write outside its working copy.
pub fn file_dir_conflict(m: &Manifest, path: &str) -> Option<String> {
    let mut i = 0;
    while let Some(j) = path[i..].find('/') {
        let parent = &path[..i + j];
        if m.contains_key(parent) {
            return Some(parent.to_string());
        }
        i += j + 1;
    }
    let under = format!("{path}/");
    m.range(under.clone()..).next().filter(|(k, _)| k.starts_with(&under)).map(|(k, _)| k.clone())
}

/// Every directory prefix of `path` plus the path itself ("a/b/c" -> "", "a", "a/b", "a/b/c").
pub fn path_keys(path: &str) -> Vec<String> {
    let mut v = vec![String::new()];
    let mut cur = String::new();
    for seg in path.split('/') {
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(seg);
        v.push(cur.clone());
    }
    v
}

impl Engine {
    /// SQLite file at `db_path` (single node).
    pub fn open(db_path: &str, blobs: Arc<dyn BlobStore>) -> Result<Self> {
        let c = rusqlite::Connection::open(db_path)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        // databases from older versions: add columns before the schema's new indexes run
        let has = |t: &str, col: &str| c.prepare(&format!("SELECT {col} FROM {t} LIMIT 0")).is_ok();
        let existed = has("changesets", "id");
        // indexes are backfilled only for databases that predate them, not on every start
        let had_history = has("path_history", "cs_id");
        let mut need_gen = false;
        if existed {
            for (t, col, ddl) in [
                ("tokens", "groups", "ALTER TABLE tokens ADD COLUMN groups TEXT NOT NULL DEFAULT ''"),
                ("repos", "owner", "ALTER TABLE repos ADD COLUMN owner TEXT"),
                ("changeset_files", "mode", "ALTER TABLE changeset_files ADD COLUMN mode INTEGER NOT NULL DEFAULT 33188"),
                ("changesets", "gen", "ALTER TABLE changesets ADD COLUMN gen INTEGER NOT NULL DEFAULT 1"),
            ] {
                if !has(t, col) {
                    c.execute_batch(ddl)?;
                    if col == "gen" {
                        need_gen = true;
                    }
                }
            }
        }
        c.execute_batch(SCHEMA)?;
        let mut c = c;
        crate::migrate::run(&mut c)?;
        let e = Self { db: Db::Sqlite(std::sync::Mutex::new(c)), blobs, manifests: Default::default() };
        if need_gen || (existed && !had_history) {
            e.backfill_indexes()?;
        }
        Ok(e)
    }
    /// Postgres at `url` (`postgres://...`): several cmbd nodes may share it.
    pub fn open_pg(url: &str, blobs: Arc<dyn BlobStore>) -> Result<Self> {
        let pool = std::env::var("CAMBIUM_PG_POOL").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
        Ok(Self { db: Db::open_pg(url, pool)?, blobs, manifests: Default::default() })
    }
    /// Fill generation numbers and the path history index for changesets written
    /// by versions that predate them. Idempotent.
    pub fn backfill_indexes(&self) -> Result<()> {
        let rows = self.db.query("SELECT id,repo,parent1,parent2 FROM changesets ORDER BY seq", p![])?;
        let mut gens: std::collections::HashMap<String, i64> = Default::default();
        for r in &rows {
            let g = 1 + [r.os(2), r.os(3)].iter().flatten().map(|x| gens.get(x).copied().unwrap_or(0)).max().unwrap_or(0);
            gens.insert(r.s(0), g);
        }
        let indexed: HashSet<String> =
            self.db.query("SELECT DISTINCT cs_id FROM path_history", p![])?.into_iter().map(|r| r.s(0)).collect();
        let todo: Vec<(String, String, i64)> = rows.iter().map(|r| (r.s(0), r.s(1), gens[&r.s(0)])).collect();
        self.db.tx(move |t| {
            for (id, repo, g) in todo {
                t.exec("UPDATE changesets SET gen=?2 WHERE id=?1", &p![&id, g])?;
                if indexed.contains(&id) {
                    continue;
                }
                let files = t.query("SELECT path FROM changeset_files WHERE cs_id=?1", &p![&id])?;
                let mut keys: Vec<String> = files.iter().flat_map(|f| path_keys(&f.s(0))).collect();
                keys.sort();
                keys.dedup();
                for k in keys {
                    t.exec(
                        "INSERT INTO path_history(repo,dir,gen,cs_id) VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
                        &p![&repo, k, g, &id],
                    )?;
                }
            }
            Ok(())
        })
    }

    // ---------- auth ----------
    pub fn verify_token(&self, tok: &str) -> Result<String> {
        Ok(self.verify_principal(tok)?.user)
    }

    // ---------- repos & WAL ----------
    pub fn create_repo(&self, repo: &str, user: &str) -> Result<String> {
        if repo.is_empty() || !repo.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::Bad("repo name must be [A-Za-z0-9_-]+".into()));
        }
        let n = self.db.exec("INSERT INTO repos(name,owner,created_at) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING", p![repo, user, now()])?;
        if n == 0 {
            return Err(Error::Bad(format!("repo {repo} exists")));
        }
        let root = self.create_changeset(repo, vec![], user, "repository created", vec![])?;
        self.cas_ref(repo, "main", None, Some(&root.id))?;
        Ok(root.id)
    }
    pub fn list_repos(&self) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT name FROM repos ORDER BY name", p![])?.iter().map(|r| r.s(0)).collect())
    }

    // ---------- per-directory permissions ----------
    pub fn acl(&self, repo: &str) -> Result<Vec<AclRule>> {
        self.check_repo(repo)?;
        let v = self.db.query("SELECT prefix,principal,perm FROM acl WHERE repo=?1 ORDER BY prefix,principal", p![repo])?;
        Ok(v.into_iter().map(|r| AclRule { prefix: r.s(0), principal: r.s(1), perm: Perm::parse(&r.s(2)).unwrap_or(Perm::None) }).collect())
    }
    pub fn authz(&self, repo: &str, who: &Principal) -> Result<Authz> {
        Ok(Authz { rules: self.acl(repo)?, names: who.names() })
    }
    /// Add, change (perm Some) or remove (None) one rule. The repo owner may write the
    /// first rule; after that only principals with admin on the repo root may.
    pub fn set_acl(&self, repo: &str, who: &Principal, prefix: &str, principal: &str, perm: Option<Perm>) -> Result<()> {
        let prefix = prefix.trim_end_matches('/');
        if !prefix.is_empty() && !valid_path(prefix) {
            return Err(Error::Bad(format!("invalid prefix {prefix}")));
        }
        let ok_principal = principal == "*"
            || ["user:", "group:"].iter().any(|k| principal.strip_prefix(k).is_some_and(|n| !n.is_empty() && !n.contains(',')));
        if !ok_principal {
            return Err(Error::Bad("principal must be *, user:<name> or group:<name>".into()));
        }
        let az = self.authz(repo, who)?;
        let owner = self.db.one("SELECT owner FROM repos WHERE name=?1", p![repo])?.and_then(|r| r.os(0));
        let allowed = if az.open() { owner.as_deref() == Some(who.user.as_str()) } else { az.can("", Perm::Admin) };
        if !allowed {
            return Err(Error::Forbidden("admin on / (or repo owner for the first rule)".into()));
        }
        match perm {
            Some(pm) => self.db.exec("INSERT INTO acl(repo,prefix,principal,perm) VALUES(?1,?2,?3,?4) ON CONFLICT(repo,prefix,principal) DO UPDATE SET perm=excluded.perm", p![repo, prefix, principal, pm.as_str()])?,
            None => self.db.exec("DELETE FROM acl WHERE repo=?1 AND prefix=?2 AND principal=?3", p![repo, prefix, principal])?,
        };
        Ok(())
    }
    /// A blob may be read if it is unreferenced (just uploaded) or some path that
    /// references it is readable by `who`.
    pub fn blob_readable(&self, id: &str, who: &Principal) -> Result<bool> {
        let refs = self
            .db
            .query("SELECT DISTINCT c.repo, f.path FROM changeset_files f JOIN changesets c ON c.id=f.cs_id WHERE f.blob=?1", p![id])?;
        if refs.is_empty() {
            return Ok(true);
        }
        let mut cache: BTreeMap<String, Authz> = BTreeMap::new();
        for r in refs {
            let (repo, path) = (r.s(0), r.s(1));
            if !cache.contains_key(&repo) {
                let a = self.authz(&repo, who)?;
                cache.insert(repo.clone(), a);
            }
            if cache[&repo].can(&path, Perm::Read) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn check_repo(&self, repo: &str) -> Result<()> {
        self.db.one("SELECT 1 FROM repos WHERE name=?1", p![repo])?.map(|_| ()).ok_or_else(|| Error::NotFound(format!("repo {repo}")))
    }

    /// Append a changeset to the write-ahead log. Never touches refs.
    pub fn create_changeset(
        &self,
        repo: &str,
        parents: Vec<String>,
        author: &str,
        message: &str,
        files: Vec<FileChange>,
    ) -> Result<Changeset> {
        self.create_changeset_at(repo, parents, author, message, files, now())
    }
    /// As `create_changeset` with an explicit timestamp (history import).
    pub fn create_changeset_at(
        &self,
        repo: &str,
        parents: Vec<String>,
        author: &str,
        message: &str,
        mut files: Vec<FileChange>,
        ts: i64,
    ) -> Result<Changeset> {
        self.check_repo(repo)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup_by(|a, b| a.path == b.path);
        for f in &files {
            if !valid_path(&f.path) {
                return Err(Error::Bad(format!("invalid path {}", f.path)));
            }
            if ![MODE_FILE, MODE_EXEC, MODE_LINK].contains(&f.mode) {
                return Err(Error::Bad(format!("unsupported mode {:o} for {}", f.mode, f.path)));
            }
            if let Some(b) = &f.blob {
                if !self.blobs.has(b)? {
                    return Err(Error::Bad(format!("missing blob {b}")));
                }
            }
        }
        if parents.len() > 2 {
            return Err(Error::Bad("a changeset has at most two parents".into()));
        }
        if files.iter().any(|f| f.blob.is_some()) {
            let mut m = match parents.first() {
                Some(p1) => self.manifest(repo, p1)?,
                None => Manifest::new(),
            };
            for f in &files {
                match &f.blob {
                    Some(b) => m.insert(f.path.clone(), b.clone()),
                    None => m.remove(&f.path),
                };
            }
            for f in files.iter().filter(|f| f.blob.is_some()) {
                if let Some(c) = file_dir_conflict(&m, &f.path) {
                    return Err(Error::Bad(format!("{} conflicts with {c}: a path cannot be both a file and a directory", f.path)));
                }
            }
        }
        let mut gen = 1;
        for pid in &parents {
            gen = gen.max(self.gen(repo, pid)? + 1);
        }
        let payload = serde_json::json!({"parents":parents,"author":author,"message":message,"files":files,"ts":ts});
        let (repo_s, author_s, msg_s, files_c, parents_c) =
            (repo.to_string(), author.to_string(), message.to_string(), files.clone(), parents.clone());
        let (id, seq) = self.db.tx(move |t| {
            let seq = t
                .one(
                    "INSERT INTO wal(repo,kind,payload,created_at) VALUES(?1,'changeset',?2,?3) RETURNING seq",
                    &p![&repo_s, payload.to_string(), ts],
                )?
                .unwrap()
                .i(0);
            let id = hash_bytes(format!("{repo_s}\n{seq}\n{payload}").as_bytes());
            t.exec(
                "INSERT INTO changesets(id,repo,seq,parent1,parent2,author,message,created_at,gen) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                &p![&id, &repo_s, seq, parents_c.first(), parents_c.get(1), &author_s, &msg_s, ts, gen],
            )?;
            let mut keys = vec![];
            for f in &files_c {
                t.exec("INSERT INTO changeset_files(cs_id,path,blob,mode) VALUES(?1,?2,?3,?4)", &p![&id, &f.path, f.blob.clone(), f.mode])?;
                keys.extend(path_keys(&f.path));
            }
            keys.sort();
            keys.dedup();
            for k in keys {
                t.exec("INSERT INTO path_history(repo,dir,gen,cs_id) VALUES(?1,?2,?3,?4)", &p![&repo_s, k, gen, &id])?;
            }
            Ok((id, seq))
        })?;
        // derive the root tree now, so fetches never wait for it (if this fails it is
        // computed on first use instead)
        if let Err(e) = self.root_tree(repo, &id) {
            crate::log::error("root tree not built yet", serde_json::json!({"cs": id, "error": e.to_string()}));
        }
        if let Err(e) = self.derive_eager(repo, &id) {
            crate::log::error("derived data not built yet", serde_json::json!({"cs": id, "error": e.to_string()}));
        }
        Ok(Changeset { id, seq, parents, author: author.into(), message: message.into(), created_at: ts, files })
    }

    /// Generation number: 1 for roots, else 1 + max(parent generations).
    pub fn gen(&self, repo: &str, id: &str) -> Result<i64> {
        self.db
            .one("SELECT gen FROM changesets WHERE repo=?1 AND id=?2", p![repo, id])?
            .map(|r| r.i(0))
            .ok_or_else(|| Error::NotFound(format!("changeset {id}")))
    }

    pub fn get_changeset(&self, repo: &str, id: &str) -> Result<Changeset> {
        let rows = if id.len() == 64 {
            self.db
                .query("SELECT id,seq,parent1,parent2,author,message,created_at FROM changesets WHERE repo=?1 AND id=?2", p![repo, id])?
        } else if id.len() >= 6 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            self.db.query(
                "SELECT id,seq,parent1,parent2,author,message,created_at FROM changesets WHERE repo=?1 AND id LIKE ?2 LIMIT 2",
                p![repo, format!("{id}%")],
            )?
        } else {
            vec![]
        };
        if rows.len() > 1 {
            return Err(Error::Bad(format!("ambiguous changeset prefix {id}")));
        }
        let r = rows.into_iter().next().ok_or_else(|| Error::NotFound(format!("changeset {id}")))?;
        let files = self
            .db
            .query("SELECT path,blob,mode FROM changeset_files WHERE cs_id=?1 ORDER BY path", p![r.s(0)])?
            .into_iter()
            .map(|f| FileChange { path: f.s(0), blob: f.os(1), mode: f.i(2) as u32 })
            .collect();
        Ok(Changeset {
            id: r.s(0),
            seq: r.i(1),
            parents: r.os(2).into_iter().chain(r.os(3)).collect(),
            author: r.s(4),
            message: r.s(5),
            created_at: r.i(6),
            files,
        })
    }
    /// Parent ids only (cheap; used by graph walks).
    fn parents(&self, repo: &str, id: &str) -> Result<(Vec<String>, i64)> {
        let r = self
            .db
            .one("SELECT parent1,parent2,gen FROM changesets WHERE repo=?1 AND id=?2", p![repo, id])?
            .ok_or_else(|| Error::NotFound(format!("changeset {id}")))?;
        Ok((r.os(0).into_iter().chain(r.os(1)).collect(), r.i(2)))
    }

    // ---------- refs (compare-and-swap on one row) ----------
    pub fn get_ref(&self, repo: &str, name: &str) -> Result<Option<String>> {
        Ok(self.db.one("SELECT cs_id FROM refs WHERE repo=?1 AND name=?2", p![repo, name])?.map(|r| r.s(0)))
    }
    pub fn list_refs(&self, repo: &str) -> Result<Vec<(String, String, i64)>> {
        Ok(self
            .db
            .query("SELECT name,cs_id,version FROM refs WHERE repo=?1 ORDER BY name", p![repo])?
            .into_iter()
            .map(|r| (r.s(0), r.s(1), r.i(2)))
            .collect())
    }
    /// Move `name` from `expected` to `new`. None = absent. One conditional
    /// UPDATE/INSERT/DELETE on one row, so it is atomic on SQLite and across
    /// several server nodes sharing a Postgres database.
    pub fn cas_ref(&self, repo: &str, name: &str, expected: Option<&str>, new: Option<&str>) -> Result<()> {
        if name.is_empty() || name.contains("..") || name.contains(' ') {
            return Err(Error::Bad("invalid ref name".into()));
        }
        let (repo, name, expected, new) = (repo.to_string(), name.to_string(), expected.map(String::from), new.map(String::from));
        self.db.tx(move |t| {
            let n = match (&expected, &new) {
                (None, Some(n)) => {
                    t.exec("INSERT INTO refs(repo,name,cs_id,version) VALUES(?1,?2,?3,1) ON CONFLICT DO NOTHING", &p![&repo, &name, n])?
                }
                (Some(e), Some(n)) => {
                    t.exec("UPDATE refs SET cs_id=?3, version=version+1 WHERE repo=?1 AND name=?2 AND cs_id=?4", &p![&repo, &name, n, e])?
                }
                (Some(e), None) => t.exec("DELETE FROM refs WHERE repo=?1 AND name=?2 AND cs_id=?3", &p![&repo, &name, e])?,
                (None, None) => return Err(Error::Bad("no-op".into())),
            };
            if n != 1 {
                let found = t.one("SELECT cs_id FROM refs WHERE repo=?1 AND name=?2", &p![&repo, &name])?.map(|r| r.s(0));
                return Err(Error::RefMoved { expected, found });
            }
            t.exec(
                "INSERT INTO wal(repo,kind,payload,created_at) VALUES(?1,'ref',?2,?3)",
                &p![&repo, serde_json::json!({"ref":name,"from":expected,"to":new}).to_string(), now()],
            )?;
            Ok(())
        })
    }

    /// Resolve a ref name or (prefix of a) changeset id.
    pub fn resolve(&self, repo: &str, rev: &str) -> Result<String> {
        if let Some(id) = self.get_ref(repo, rev)? {
            return Ok(id);
        }
        Ok(self.get_changeset(repo, rev)?.id)
    }

    // ---------- derived data ----------
    fn derived_get(&self, kind: &str, cs: &str, key: &str) -> Result<Option<String>> {
        Ok(self.db.one("SELECT value FROM derived WHERE kind=?1 AND cs_id=?2 AND key=?3", p![kind, cs, key])?.map(|r| r.s(0)))
    }
    fn derived_put(&self, kind: &str, cs: &str, key: &str, v: &str) -> Result<()> {
        self.db.exec(
            "INSERT INTO derived(kind,cs_id,key,value) VALUES(?1,?2,?3,?4) ON CONFLICT(kind,cs_id,key) DO UPDATE SET value=excluded.value",
            p![kind, cs, key, v],
        )?;
        Ok(())
    }

    /// Full manifest at a changeset, derived from first parent + changed files.
    /// Every file in changeset `cs`, read from its root tree (unchanged directories are
    /// shared tree objects, so nothing per changeset grows with the repo size).
    pub fn manifest(&self, repo: &str, cs: &str) -> Result<Manifest> {
        let key = format!("{repo}\n{cs}");
        if let Some(m) = self.manifests.lock().unwrap().get(&key) {
            return Ok((**m).clone());
        }
        let root = self.root_tree(repo, cs)?;
        let mut m = Manifest::new();
        self.manifest_of_tree(&root, "", &mut m)?;
        let mut cache = self.manifests.lock().unwrap();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, Arc::new(m.clone()));
        Ok(m)
    }

    /// Sparse view: only entries under the given prefixes (empty = all).
    pub fn tree(&self, repo: &str, cs: &str, prefixes: &[String]) -> Result<Manifest> {
        let m = self.manifest(repo, cs)?;
        if prefixes.is_empty() {
            return Ok(m);
        }
        Ok(m.into_iter()
            .filter(|(p, _)| {
                prefixes.iter().any(|x| {
                    let x = x.trim_end_matches('/');
                    x.is_empty() || p == x || p.starts_with(&format!("{x}/"))
                })
            })
            .collect())
    }

    /// Store a blob and remember its size.
    pub fn put_blob(&self, data: &[u8]) -> Result<String> {
        let id = self.blobs.put(data)?;
        self.db.exec("INSERT INTO blob_meta(blob,size) VALUES(?1,?2) ON CONFLICT DO NOTHING", p![&id, data.len() as i64])?;
        Ok(id)
    }
    /// Blob contents as text if a line merge is safe: valid UTF-8, no NUL byte, at most
    /// 8 MiB. None means "treat as binary".
    fn mergeable_text(&self, id: &str) -> Result<Option<String>> {
        let data = self.blobs.get(id)?.ok_or_else(|| Error::NotFound(format!("blob {id}")))?;
        if data.len() > 8 << 20 || data.contains(&0) {
            return Ok(None);
        }
        Ok(String::from_utf8(data).ok())
    }
    /// Store a blob uploaded by `who` and remember that they had its bytes.
    pub fn put_blob_as(&self, data: &[u8], who: &Principal) -> Result<String> {
        let id = self.put_blob(data)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        self.db
            .exec("INSERT INTO blob_uploads(blob,principal,created_at) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING", p![&id, &who.user, now])?;
        Ok(id)
    }
    /// May `who` reference blob `id` in a commit or diff? Yes if they can read it
    /// through some path, or if they uploaded its bytes themselves.
    pub fn blob_usable(&self, id: &str, who: &Principal) -> Result<bool> {
        if self.db.one("SELECT 1 FROM blob_uploads WHERE blob=?1 AND principal=?2", p![id, &who.user])?.is_some() {
            return Ok(true);
        }
        self.blob_readable(id, who)
    }
    /// Uncompressed size of a blob (recorded at upload, computed once otherwise).
    pub fn blob_size(&self, id: &str) -> Result<i64> {
        if let Some(r) = self.db.one("SELECT size FROM blob_meta WHERE blob=?1", p![id])? {
            return Ok(r.i(0));
        }
        let n = self.blobs.get(id)?.ok_or_else(|| Error::NotFound(format!("blob {id}")))?.len() as i64;
        self.db.exec("INSERT INTO blob_meta(blob,size) VALUES(?1,?2) ON CONFLICT DO NOTHING", p![id, n])?;
        Ok(n)
    }

    pub fn read_text(&self, blob: &str) -> Result<String> {
        let d = self.blobs.get(blob)?.ok_or_else(|| Error::NotFound(format!("blob {blob}")))?;
        Ok(String::from_utf8_lossy(&d).into_owned())
    }

    /// Blame: per-line changeset ids, derived recursively along first parents.
    pub fn blame(&self, repo: &str, cs: &str, path: &str) -> Result<Vec<(String, String)>> {
        let m = self.manifest(repo, cs)?;
        let blob = split_entry(m.get(path).ok_or_else(|| Error::NotFound(format!("{path} at {cs}")))?).0.to_string();
        // walk back to changesets that touched path
        let mut touching = Vec::new();
        let mut cur = Some(cs.to_string());
        let mut start: Option<(String, Vec<String>)> = None;
        while let Some(c) = cur {
            let ch = self.get_changeset(repo, &c)?;
            if let Some(f) = ch.files.iter().find(|f| f.path == path) {
                if f.blob.is_none() {
                    break;
                }
                if let Some(v) = self.derived_get("blame", &c, path)? {
                    start = Some((f.blob.clone().unwrap(), serde_json::from_str(&v).unwrap()));
                    break;
                }
                touching.push((c.clone(), f.blob.clone().unwrap()));
            }
            cur = ch.parents.first().cloned();
        }
        let (mut text_prev, mut bl) = match start {
            Some((b, bl)) => (self.read_text(&b)?, bl),
            None => (String::new(), vec![]),
        };
        for (c, b) in touching.iter().rev() {
            let t = self.read_text(b)?;
            bl = text::carry_blame(&text_prev, &bl, &t, c);
            self.derived_put("blame", c, path, &serde_json::to_string(&bl).unwrap())?;
            text_prev = t;
        }
        let content = self.read_text(&blob)?;
        Ok(bl.into_iter().zip(content.lines().map(String::from)).collect())
    }

    /// History of `cs`, newest first by generation. With `path`, only changesets that
    /// touched it (file or directory), found through the path history index.
    pub fn log(&self, repo: &str, cs: &str, path: Option<&str>, limit: usize) -> Result<Vec<Changeset>> {
        let want: Option<std::collections::HashMap<String, i64>> = match path {
            None => None,
            Some(pth) => {
                let key = pth.trim_end_matches('/');
                Some(
                    self.db
                        .query("SELECT cs_id,gen FROM path_history WHERE repo=?1 AND dir=?2", p![repo, key])?
                        .into_iter()
                        .map(|r| (r.s(0), r.i(1)))
                        .collect(),
                )
            }
        };
        let floor = want.as_ref().map(|w| w.values().copied().min().unwrap_or(i64::MAX)).unwrap_or(0);
        let mut out = vec![];
        self.walk(repo, &[cs.to_string()], |id, g| {
            if g < floor || out.len() >= limit {
                return Ok(Step::Stop);
            }
            if want.as_ref().is_none_or(|w| w.contains_key(id)) {
                out.push(id.to_string());
            }
            Ok(Step::Go)
        })?;
        out.into_iter().map(|id| self.get_changeset(repo, &id)).collect()
    }

    /// Visit ancestors of `starts` (inclusive) in decreasing generation order; `f`
    /// returns Stop to end the walk, Prune to skip this node's parents. Each node is visited once.
    fn walk(&self, repo: &str, starts: &[String], mut f: impl FnMut(&str, i64) -> Result<Step>) -> Result<()> {
        let mut heap = std::collections::BinaryHeap::new();
        let mut seen = HashSet::new();
        for s in starts {
            if seen.insert(s.clone()) {
                heap.push((self.gen(repo, s)?, s.clone()));
            }
        }
        while let Some((g, id)) = heap.pop() {
            match f(&id, g)? {
                Step::Stop => break,
                Step::Prune => continue,
                Step::Go => {}
            }
            for par in self.parents(repo, &id)?.0 {
                if seen.insert(par.clone()) {
                    let pg = self.gen(repo, &par)?;
                    heap.push((pg, par));
                }
            }
        }
        Ok(())
    }

    /// Is `a` an ancestor of (or equal to) `d`? Walks down from `d` and stops at
    /// generation gen(a): nothing below can reach `a`.
    pub fn is_ancestor(&self, repo: &str, a: &str, d: &str) -> Result<bool> {
        let ga = self.gen(repo, a)?;
        // linear history (the common case without feature branches): skip-list jumps
        if self.first_parent_reaches(repo, a, d, ga)? == Some(true) {
            return Ok(true);
        }
        let mut found = false;
        self.walk(repo, &[d.to_string()], |id, g| {
            if id == a {
                found = true;
                return Ok(Step::Stop);
            }
            Ok(if g > ga { Step::Go } else { Step::Prune })
        })?;
        Ok(found)
    }

    /// Best common ancestor: walk both sides together in decreasing generation
    /// order; the first node reached from both sides has the highest generation.
    pub fn merge_base(&self, repo: &str, a: &str, b: &str) -> Result<Option<String>> {
        if a == b {
            return Ok(Some(a.into()));
        }
        let mut marks: std::collections::HashMap<String, u8> = Default::default();
        let mut heap = std::collections::BinaryHeap::new();
        marks.insert(a.into(), 1);
        *marks.entry(b.into()).or_insert(0) |= 2;
        heap.push((self.gen(repo, a)?, a.to_string()));
        heap.push((self.gen(repo, b)?, b.to_string()));
        let mut done = HashSet::new();
        while let Some((_, id)) = heap.pop() {
            if !done.insert(id.clone()) {
                continue;
            }
            let m = marks[&id];
            if m == 3 {
                return Ok(Some(id));
            }
            for par in self.parents(repo, &id)?.0 {
                let e = marks.entry(par.clone()).or_insert(0);
                let before = *e;
                *e |= m;
                if before != *e || !done.contains(&par) {
                    let g = self.gen(repo, &par)?;
                    heap.push((g, par));
                }
            }
        }
        Ok(None)
    }

    pub fn diff(&self, repo: &str, a: &str, b: &str, prefixes: &[String]) -> Result<String> {
        let ma = self.tree(repo, a, prefixes)?;
        let mb = self.tree(repo, b, prefixes)?;
        self.diff_manifests(&ma, &mb)
    }

    pub fn diff_manifests(&self, ma: &Manifest, mb: &Manifest) -> Result<String> {
        let mut paths: Vec<&String> = ma.keys().chain(mb.keys()).collect();
        paths.sort();
        paths.dedup();
        // copy / rename detection by content: an added path whose blob was removed
        // elsewhere is a rename; one whose blob still exists elsewhere is a copy.
        let blob_of = |e: &String| split_entry(e).0.to_string();
        let mut removed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (p, e) in ma {
            if !mb.contains_key(p) {
                removed.entry(blob_of(e)).or_default().push(p.clone());
            }
        }
        let mut kept: BTreeMap<String, String> = BTreeMap::new();
        for (p, e) in ma {
            if mb.contains_key(p) {
                kept.entry(blob_of(e)).or_insert_with(|| p.clone());
            }
        }
        let mut renamed_from: HashSet<String> = HashSet::new();
        let mut out = String::new();
        for p in &paths {
            if ma.contains_key(*p) || !mb.contains_key(*p) {
                continue;
            }
            let b = blob_of(&mb[*p]);
            if let Some(src) = removed.get_mut(&b).and_then(|v| if v.is_empty() { None } else { Some(v.remove(0)) }) {
                out.push_str(&format!("rename {src} -> {p}\n"));
                renamed_from.insert(src);
                renamed_from.insert((*p).clone());
            } else if let Some(src) = kept.get(&b) {
                out.push_str(&format!("copy {src} -> {p}\n"));
                renamed_from.insert((*p).clone());
            }
        }
        for p in paths {
            if renamed_from.contains(p) {
                continue;
            }
            let (x, y) = (ma.get(p), mb.get(p));
            if x == y {
                continue;
            }
            let (bx, mx) = x.map(|e| split_entry(e)).map_or((None, None), |(b, m)| (Some(b), Some(m)));
            let (by, my) = y.map(|e| split_entry(e)).map_or((None, None), |(b, m)| (Some(b), Some(m)));
            if let (Some(a), Some(b)) = (mx, my) {
                if a != b {
                    out.push_str(&format!("mode {p}: {a:o} -> {b:o}\n"));
                }
            }
            if bx == by {
                continue;
            }
            let tx = match bx {
                Some(b) => self.read_text(b)?,
                None => String::new(),
            };
            let ty = match by {
                Some(b) => self.read_text(b)?,
                None => String::new(),
            };
            out.push_str(&text::unified(p, &tx, &ty));
        }
        Ok(out)
    }

    /// Commit = create a changeset on top of the ref head, then CAS the ref.
    pub fn commit(&self, repo: &str, refname: &str, expected: &str, author: &str, msg: &str, files: Vec<FileChange>) -> Result<Changeset> {
        if files.is_empty() {
            return Err(Error::Bad("nothing to commit".into()));
        }
        let head = self.get_ref(repo, refname)?.ok_or_else(|| Error::NotFound(format!("ref {refname}")))?;
        if head != expected {
            return Err(Error::RefMoved { expected: Some(expected.into()), found: Some(head) });
        }
        let cs = self.create_changeset(repo, vec![head.clone()], author, msg, files)?;
        self.cas_ref(repo, refname, Some(&head), Some(&cs.id))?;
        Ok(cs)
    }

    pub(crate) fn merge3_manifests(
        &self,
        base: &Manifest,
        ours: &Manifest,
        theirs: &Manifest,
    ) -> Result<std::result::Result<Manifest, Vec<String>>> {
        self.merge_manifests(base, ours, theirs)
    }
    fn merge_manifests(&self, base: &Manifest, ours: &Manifest, theirs: &Manifest) -> Result<std::result::Result<Manifest, Vec<String>>> {
        let mut out = ours.clone();
        let mut conflicts = vec![];
        let mut paths: Vec<&String> = base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
        paths.sort();
        paths.dedup();
        for p in paths {
            let (b, o, t) = (base.get(p), ours.get(p), theirs.get(p));
            if o == t || t == b {
                continue;
            }
            if o == b {
                match t {
                    Some(x) => {
                        out.insert(p.clone(), x.clone());
                    }
                    None => {
                        out.remove(p);
                    }
                }
                continue;
            }
            match (b, o, t) {
                (_, Some(o), Some(t)) => {
                    let (bb, bm) = b.map(|e| split_entry(e)).map_or((None, None), |(x, m)| (Some(x), Some(m)));
                    let ((ob, om), (tb, tm)) = (split_entry(o), split_entry(t));
                    // mode: take whichever side changed it; both changed differently = conflict
                    let mode = if om == tm {
                        om
                    } else if Some(om) == bm {
                        tm
                    } else if Some(tm) == bm {
                        om
                    } else {
                        conflicts.push(p.clone());
                        continue;
                    };
                    let blob = if ob == tb {
                        ob.to_string()
                    } else if Some(ob) == bb {
                        tb.to_string()
                    } else if Some(tb) == bb {
                        ob.to_string()
                    } else {
                        // line merge only for real text: binary files, symlinks and huge
                        // files are conflicts, never lossily merged
                        let texts = if mode == MODE_LINK || om == MODE_LINK || tm == MODE_LINK {
                            None
                        } else {
                            let bt = match bb {
                                Some(b) => self.mergeable_text(b)?,
                                None => Some(String::new()),
                            };
                            match (bt, self.mergeable_text(ob)?, self.mergeable_text(tb)?) {
                                (Some(b), Some(o), Some(t)) => Some((b, o, t)),
                                _ => None,
                            }
                        };
                        match texts.map(|(b, o, t)| text::merge3(&b, &o, &t)) {
                            Some(Ok(m)) => self.blobs.put(m.as_bytes())?,
                            _ => {
                                conflicts.push(p.clone());
                                continue;
                            }
                        }
                    };
                    out.insert(p.clone(), entry(&blob, mode));
                }
                _ => conflicts.push(p.clone()),
            }
        }
        Ok(if conflicts.is_empty() { Ok(out) } else { Err(conflicts) })
    }

    pub fn changes_between(a: &Manifest, b: &Manifest) -> Vec<FileChange> {
        let mut v = vec![];
        for (p, x) in b {
            if a.get(p) != Some(x) {
                let (bl, m) = split_entry(x);
                v.push(FileChange { path: p.clone(), blob: Some(bl.into()), mode: m });
            }
        }
        for p in a.keys() {
            if !b.contains_key(p) {
                v.push(FileChange { path: p.clone(), blob: None, mode: MODE_FILE });
            }
        }
        v
    }

    /// Server-side merge of `source` into ref `target`.
    pub fn merge(&self, repo: &str, target: &str, source: &str, author: &str) -> Result<Changeset> {
        self.merge_checked(repo, target, source, author, &|_| Ok(()))
    }
    /// Merge, calling `check` on the files the merge would change before anything is written.
    pub fn merge_checked(
        &self,
        repo: &str,
        target: &str,
        source: &str,
        author: &str,
        check: &dyn Fn(&[FileChange]) -> Result<()>,
    ) -> Result<Changeset> {
        let t = self.get_ref(repo, target)?.ok_or_else(|| Error::NotFound(format!("ref {target}")))?;
        let s = self.resolve(repo, source)?;
        let base = self.merge_base(repo, &t, &s)?.ok_or_else(|| Error::Bad("no common ancestor".into()))?;
        if base == s {
            return Err(Error::Bad("already up to date".into()));
        }
        let (mb, mt, ms) = (self.manifest(repo, &base)?, self.manifest(repo, &t)?, self.manifest(repo, &s)?);
        let merged = self.merge_manifests(&mb, &mt, &ms)?.map_err(Error::Conflict)?;
        let files = Self::changes_between(&mt, &merged);
        check(&files)?;
        let parents = vec![t.clone(), s.clone()];
        let cs = self.create_changeset(repo, parents, author, &format!("merge {source} into {target}"), files)?;
        self.cas_ref(repo, target, Some(&t), Some(&cs.id))?;
        Ok(cs)
    }

    /// Server-side rebase: replay first-parent commits of `branch` since the
    /// merge base onto `onto`, then CAS the branch ref.
    pub fn rebase(&self, repo: &str, branch: &str, onto: &str) -> Result<String> {
        self.rebase_checked(repo, branch, onto, &|_| Ok(()))
    }
    pub fn rebase_checked(&self, repo: &str, branch: &str, onto: &str, check: &dyn Fn(&[FileChange]) -> Result<()>) -> Result<String> {
        let head = self.get_ref(repo, branch)?.ok_or_else(|| Error::NotFound(format!("ref {branch}")))?;
        let onto_id = self.resolve(repo, onto)?;
        let base = self.merge_base(repo, &head, &onto_id)?.ok_or_else(|| Error::Bad("no common ancestor".into()))?;
        if base == onto_id {
            // already on top of `onto`: nothing to replay, keep the head (and its approvals)
            return Ok(head);
        }
        let mut todo = vec![];
        let mut cur = head.clone();
        while cur != base {
            let ch = self.get_changeset(repo, &cur)?;
            if ch.parents.len() > 1 {
                return Err(Error::Bad("cannot rebase across merges".into()));
            }
            cur = ch.parents.first().cloned().ok_or_else(|| Error::Bad("unrelated".into()))?;
            todo.push(ch);
        }
        let mut tip = onto_id.clone();
        for ch in todo.iter().rev() {
            let mp = self.manifest(repo, &ch.parents[0])?;
            let mc = self.manifest(repo, &ch.id)?;
            let mt = self.manifest(repo, &tip)?;
            let merged = self.merge_manifests(&mp, &mt, &mc)?.map_err(Error::Conflict)?;
            let files = Self::changes_between(&mt, &merged);
            check(&files)?;
            tip = self.create_changeset(repo, vec![tip], &ch.author, &ch.message, files)?.id;
        }
        self.cas_ref(repo, branch, Some(&head), Some(&tip))?;
        Ok(tip)
    }

    /// Remove a repository and everything that hangs off it (used to undo a failed
    /// import). Blobs stay: they are content-addressed and may be shared.
    pub fn discard_repo(&self, repo: &str) -> Result<()> {
        let r = repo.to_string();
        self.db.tx(move |t| {
            t.exec("DELETE FROM refs WHERE repo=?1", &p![&r])?;
            t.exec("DELETE FROM path_history WHERE repo=?1", &p![&r])?;
            t.exec("DELETE FROM git_import_map WHERE repo=?1", &p![&r])?;
            t.exec("DELETE FROM repos WHERE name=?1", &p![&r])?;
            Ok(())
        })
    }

    /// Replay the changesets between `merge_base(cs, onto)` and `cs` on top of `onto`
    /// and return the new tip. Writes changesets only, never a ref. If `cs` is already
    /// on top of `onto`, returns `cs`; if it is already contained in `onto`, returns `onto`.
    pub fn rebase_onto(&self, repo: &str, cs: &str, onto: &str, check: &dyn Fn(&[FileChange]) -> Result<()>) -> Result<String> {
        let base = self.merge_base(repo, cs, onto)?.ok_or_else(|| Error::Bad("no common ancestor".into()))?;
        if base == onto {
            return Ok(cs.to_string());
        }
        if base == cs {
            return Ok(onto.to_string());
        }
        let mut todo = vec![];
        let mut cur = cs.to_string();
        while cur != base {
            let ch = self.get_changeset(repo, &cur)?;
            if ch.parents.len() > 1 {
                return Err(Error::Bad("cannot rebase across merges".into()));
            }
            cur = ch.parents.first().cloned().ok_or_else(|| Error::Bad("unrelated".into()))?;
            todo.push(ch);
        }
        let mut tip = onto.to_string();
        for ch in todo.iter().rev() {
            let mp = self.manifest(repo, &ch.parents[0])?;
            let mc = self.manifest(repo, &ch.id)?;
            let mt = self.manifest(repo, &tip)?;
            let merged = self.merge_manifests(&mp, &mt, &mc)?.map_err(Error::Conflict)?;
            let files = Self::changes_between(&mt, &merged);
            if files.is_empty() {
                continue; // already applied upstream (e.g. landed before): drop it
            }
            check(&files)?;
            tip = self.create_changeset(repo, vec![tip], &ch.author, &ch.message, files)?.id;
        }
        Ok(tip)
    }
    /// Land changeset `cs` (and the drafts under it) on ref `target`: rebase onto the
    /// current head and compare-and-swap, retrying if someone else landed first.
    /// Lock-free, no merge queue. Returns the new head.
    pub fn land(&self, repo: &str, cs: &str, target: &str, check: &dyn Fn(&[FileChange]) -> Result<()>) -> Result<String> {
        let cs = self.resolve(repo, cs)?;
        for _ in 0..16 {
            let head = self.get_ref(repo, target)?.ok_or_else(|| Error::NotFound(format!("ref {target}")))?;
            let tip = self.rebase_onto(repo, &cs, &head, check)?;
            if tip == head {
                return Ok(head); // already landed
            }
            match self.cas_ref(repo, target, Some(&head), Some(&tip)) {
                Ok(()) => return Ok(tip),
                Err(Error::RefMoved { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::Bad(format!("{target} kept moving; try again")))
    }

    pub fn wal_len(&self, repo: &str) -> Result<i64> {
        Ok(self.db.one("SELECT count(*) FROM wal WHERE repo=?1", p![repo])?.unwrap().i(0))
    }
}
