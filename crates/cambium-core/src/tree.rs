//! Content-addressed directory trees, the unit of the fetch protocol: every directory is
//! an immutable object (sorted entries of name, kind, id, size) identified by the hash
//! of its canonical form. A changeset's root tree is derived from its first parent's
//! root plus its changes, so only directories on a changed path get new objects and
//! everything else is shared. Clients walk from the root tree and fetch only the trees
//! and blobs they are missing.
use crate::blob::hash_bytes;
use crate::engine::{entry, split_entry, Engine, Error, FileChange, Manifest, Result, MODE_EXEC, MODE_FILE, MODE_LINK};
use crate::p;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TreeEntry {
    pub name: String,
    /// "file" | "exec" | "link" | "tree"
    pub kind: String,
    pub id: String,
    /// bytes for files and links; 0 for trees
    pub size: i64,
}

/// A directory: entries sorted by name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Canonical bytes: the id is computed from these, by anyone, so caches and
    /// clients can verify what they were sent.
    pub fn canonical(&self) -> String {
        serde_json::to_string(&self.entries.iter().map(|e| (&e.name, &e.kind, &e.id, e.size)).collect::<Vec<_>>()).unwrap()
    }
    pub fn id(&self) -> String {
        hash_bytes(format!("cambium-tree-v1\n{}", self.canonical()).as_bytes())
    }
    fn from_canonical(s: &str) -> Result<Self> {
        let v: Vec<(String, String, String, i64)> = serde_json::from_str(s).map_err(|e| Error::Db(format!("bad tree: {e}")))?;
        Ok(Tree { entries: v.into_iter().map(|(name, kind, id, size)| TreeEntry { name, kind, id, size }).collect() })
    }
}

fn kind_of(mode: u32) -> &'static str {
    match mode {
        MODE_EXEC => "exec",
        MODE_LINK => "link",
        _ => "file",
    }
}
fn mode_of(kind: &str) -> u32 {
    match kind {
        "exec" => MODE_EXEC,
        "link" => MODE_LINK,
        _ => MODE_FILE,
    }
}
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Pending edits for one directory while building a new tree.
#[derive(Default)]
struct Edit {
    files: BTreeMap<String, Option<(String, u32)>>,
    dirs: BTreeMap<String, Edit>,
}
impl Edit {
    fn add(&mut self, path: &str, v: Option<(String, u32)>) {
        match path.split_once('/') {
            Some((d, rest)) => self.dirs.entry(d.to_string()).or_default().add(rest, v),
            None => {
                self.files.insert(path.to_string(), v);
            }
        }
    }
}

impl Engine {
    pub fn get_tree(&self, id: &str) -> Result<Tree> {
        let r = self.db.one("SELECT body FROM trees WHERE id=?1", p![id])?.ok_or_else(|| Error::NotFound(format!("tree {id}")))?;
        Tree::from_canonical(&r.s(0))
    }
    /// Store `t` (if new) and record that it occurs at `path` in `repo`.
    fn put_tree(&self, repo: &str, path: &str, t: &Tree) -> Result<String> {
        let id = t.id();
        self.db.exec("INSERT INTO trees(id,body) VALUES(?1,?2) ON CONFLICT DO NOTHING", p![&id, t.canonical()])?;
        self.db.exec("INSERT INTO tree_paths(tree_id,repo,path) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING", p![&id, repo, path])?;
        Ok(id)
    }
    /// Does tree `id` occur at `path` in `repo`? (Trees are only served where they are.)
    pub fn tree_at(&self, repo: &str, id: &str, path: &str) -> Result<bool> {
        Ok(self.db.one("SELECT 1 FROM tree_paths WHERE tree_id=?1 AND repo=?2 AND path=?3", p![id, repo, path])?.is_some())
    }

    /// Apply `changes` to the tree `base` (None = empty) at directory `dir`; returns the
    /// new tree id, or None if the directory ends up empty.
    fn apply(&self, repo: &str, dir: &str, base: Option<&str>, ed: &Edit) -> Result<Option<String>> {
        let mut entries: BTreeMap<String, TreeEntry> = match base {
            Some(id) => self.get_tree(id)?.entries.into_iter().map(|e| (e.name.clone(), e)).collect(),
            None => BTreeMap::new(),
        };
        for (name, v) in &ed.files {
            match v {
                Some((blob, mode)) => {
                    let size = self.blob_size(blob)?;
                    entries.insert(name.clone(), TreeEntry { name: name.clone(), kind: kind_of(*mode).into(), id: blob.clone(), size });
                }
                None => {
                    if entries.get(name).is_some_and(|e| e.kind != "tree") {
                        entries.remove(name);
                    }
                }
            }
        }
        for (name, sub) in &ed.dirs {
            let cur = entries.get(name).filter(|e| e.kind == "tree").map(|e| e.id.clone());
            match self.apply(repo, &join(dir, name), cur.as_deref(), sub)? {
                Some(id) => {
                    entries.insert(name.clone(), TreeEntry { name: name.clone(), kind: "tree".into(), id, size: 0 });
                }
                None => {
                    entries.remove(name);
                }
            }
        }
        if entries.is_empty() && !dir.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.put_tree(repo, dir, &Tree { entries: entries.into_values().collect() })?))
    }

    /// Root tree of changeset `cs`, computed (and stored) on first use from the
    /// nearest ancestor that has one.
    pub fn root_tree(&self, repo: &str, cs: &str) -> Result<String> {
        let mut chain = vec![];
        let mut cur = Some(cs.to_string());
        let mut root: Option<String> = None;
        while let Some(c) = cur {
            if let Some(r) = self.db.one("SELECT root_tree FROM changesets WHERE id=?1 AND repo=?2", p![&c, repo])?.and_then(|r| r.os(0)) {
                root = Some(r);
                break;
            }
            let ch = self.get_changeset(repo, &c)?;
            cur = ch.parents.first().cloned();
            chain.push((ch.id, ch.files));
        }
        for (id, files) in chain.iter().rev() {
            root = Some(self.apply_files(repo, root.as_deref(), files)?);
            self.db.exec("UPDATE changesets SET root_tree=?1 WHERE id=?2", p![root.as_deref().unwrap(), id])?;
        }
        root.ok_or_else(|| Error::NotFound(format!("changeset {cs}")))
    }
    fn apply_files(&self, repo: &str, base: Option<&str>, files: &[FileChange]) -> Result<String> {
        let mut ed = Edit::default();
        for f in files {
            ed.add(&f.path, f.blob.as_ref().map(|b| (b.clone(), f.mode)));
        }
        Ok(self.apply(repo, "", base, &ed)?.expect("the root always exists"))
    }

    /// Every file under tree `id` (at directory `dir`), as a manifest.
    pub fn manifest_of_tree(&self, id: &str, dir: &str, out: &mut Manifest) -> Result<()> {
        for e in self.get_tree(id)?.entries {
            let p = join(dir, &e.name);
            if e.kind == "tree" {
                self.manifest_of_tree(&e.id, &p, out)?;
            } else {
                out.insert(p, entry(&e.id, mode_of(&e.kind)));
            }
        }
        Ok(())
    }
}

fn entry_str(e: &TreeEntry) -> String {
    entry(&e.id, mode_of(&e.kind))
}
fn change(path: String, e: &TreeEntry) -> FileChange {
    FileChange { path, blob: Some(e.id.clone()), mode: mode_of(&e.kind) }
}
fn delete(path: String) -> FileChange {
    FileChange { path, blob: None, mode: MODE_FILE }
}

impl Engine {
    fn entries(&self, id: Option<&str>) -> Result<BTreeMap<String, TreeEntry>> {
        Ok(match id {
            Some(id) => self.get_tree(id)?.entries.into_iter().map(|e| (e.name.clone(), e)).collect(),
            None => BTreeMap::new(),
        })
    }
    /// Entry at `path` under root tree `root`, if any (walks the path, not the tree).
    pub fn lookup(&self, root: &str, path: &str) -> Result<Option<TreeEntry>> {
        let mut cur = root.to_string();
        let segs: Vec<&str> = path.split('/').collect();
        for (i, s) in segs.iter().enumerate() {
            let Some(e) = self.get_tree(&cur)?.entries.into_iter().find(|e| e.name == *s) else { return Ok(None) };
            if i + 1 == segs.len() {
                return Ok(Some(e));
            }
            if e.kind != "tree" {
                return Ok(None);
            }
            cur = e.id;
        }
        Ok(None)
    }
    /// Every file path under tree `id` (at directory `dir`).
    fn file_paths(&self, id: &str, dir: &str, out: &mut Vec<String>) -> Result<()> {
        for e in self.get_tree(id)?.entries {
            let p = join(dir, &e.name);
            if e.kind == "tree" {
                self.file_paths(&e.id, &p, out)?;
            } else {
                out.push(p);
            }
        }
        Ok(())
    }
    fn add_all(&self, id: &str, dir: &str, out: &mut Vec<FileChange>) -> Result<()> {
        for e in self.get_tree(id)?.entries {
            let p = join(dir, &e.name);
            if e.kind == "tree" {
                self.add_all(&e.id, &p, out)?;
            } else {
                out.push(change(p, &e));
            }
        }
        Ok(())
    }
    fn delete_all(&self, id: &str, dir: &str, out: &mut Vec<FileChange>) -> Result<()> {
        let mut ps = vec![];
        self.file_paths(id, dir, &mut ps)?;
        out.extend(ps.into_iter().map(delete));
        Ok(())
    }
    /// File changes that turn tree `a` into tree `b`; identical subtrees are skipped.
    pub fn diff_trees(&self, a: Option<&str>, b: Option<&str>, dir: &str, out: &mut Vec<FileChange>) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let (ea, eb) = (self.entries(a)?, self.entries(b)?);
        for name in ea.keys().chain(eb.keys()).collect::<std::collections::BTreeSet<_>>() {
            let p = join(dir, name);
            match (ea.get(name), eb.get(name)) {
                (Some(x), Some(y)) if x == y => {}
                (Some(x), Some(y)) if x.kind == "tree" && y.kind == "tree" => self.diff_trees(Some(&x.id), Some(&y.id), &p, out)?,
                (Some(x), y) => {
                    if x.kind == "tree" {
                        self.delete_all(&x.id, &p, out)?;
                    } else if y.is_none_or(|y| y.kind == "tree") {
                        out.push(delete(p.clone()));
                    }
                    match y {
                        Some(y) if y.kind == "tree" => self.add_all(&y.id, &p, out)?,
                        Some(y) => out.push(change(p, y)),
                        None => {}
                    }
                }
                (None, Some(y)) => {
                    if y.kind == "tree" {
                        self.add_all(&y.id, &p, out)?;
                    } else {
                        out.push(change(p, y));
                    }
                }
                (None, None) => {}
            }
        }
        Ok(())
    }
    /// Three-way merge on trees: the file changes that apply `base -> theirs` on top of
    /// `ours` (conflicting paths go to `conflicts`). Only directories that differ are
    /// visited, so the cost follows the size of the change, not of the repository.
    pub fn merge_tree_changes(
        &self,
        base: Option<&str>,
        ours: Option<&str>,
        theirs: Option<&str>,
        dir: &str,
        out: &mut Vec<FileChange>,
        conflicts: &mut Vec<String>,
    ) -> Result<()> {
        if theirs == base || ours == theirs {
            return Ok(());
        }
        if ours == base {
            return self.diff_trees(ours, theirs, dir, out);
        }
        let (eb, eo, et) = (self.entries(base)?, self.entries(ours)?, self.entries(theirs)?);
        let names: std::collections::BTreeSet<&String> = eb.keys().chain(eo.keys()).chain(et.keys()).collect();
        for name in names {
            let p = join(dir, name);
            let (b, o, t) = (eb.get(name), eo.get(name), et.get(name));
            let tree = |e: Option<&TreeEntry>| e.is_some_and(|e| e.kind == "tree");
            let file = |e: Option<&TreeEntry>| e.is_some_and(|e| e.kind != "tree");
            let id = |e: Option<&TreeEntry>| e.map(|e| e.id.clone());
            if !file(b) && !file(o) && !file(t) {
                // directories (or absent) on every side
                self.merge_tree_changes(id(b).as_deref(), id(o).as_deref(), id(t).as_deref(), &p, out, conflicts)?;
            } else if !tree(b) && !tree(o) && !tree(t) {
                // files (or absent) on every side
                match self.merge_entry(b.map(entry_str).as_ref(), o.map(entry_str).as_ref(), t.map(entry_str).as_ref())? {
                    crate::engine::EntryMerge::Keep => {}
                    crate::engine::EntryMerge::Take(Some(e)) => {
                        let (blob, mode) = split_entry(&e);
                        out.push(FileChange { path: p, blob: Some(blob.to_string()), mode });
                    }
                    crate::engine::EntryMerge::Take(None) => out.push(delete(p)),
                    crate::engine::EntryMerge::Conflict => conflicts.push(p),
                }
            } else if t == b || o == t {
                // file <-> directory on one side only, the other unchanged
            } else if o == b {
                let mut ch = vec![];
                let wrap = |e: Option<&TreeEntry>| e.map(|e| Tree { entries: vec![TreeEntry { name: name.clone(), ..e.clone() }] });
                let (ow, tw) = (wrap(o), wrap(t));
                let (oid, tid) =
                    (ow.as_ref().map(|x| self.store_tree(x)).transpose()?, tw.as_ref().map(|x| self.store_tree(x)).transpose()?);
                self.diff_trees(oid.as_deref(), tid.as_deref(), dir, &mut ch)?;
                out.extend(ch);
            } else {
                conflicts.push(p);
            }
        }
        Ok(())
    }
    /// Store a tree object without recording a path (temporary single-entry trees).
    fn store_tree(&self, t: &Tree) -> Result<String> {
        let id = t.id();
        self.db.exec("INSERT INTO trees(id,body) VALUES(?1,?2) ON CONFLICT DO NOTHING", p![&id, t.canonical()])?;
        Ok(id)
    }
    /// A changeset may not make a path both a file and a directory (checked against the
    /// parent's tree by lookups, not by listing the repository).
    pub(crate) fn file_dir_conflict_in(&self, repo: &str, parent: Option<&String>, files: &[FileChange]) -> Result<Option<String>> {
        let root = parent.map(|p| self.root_tree(repo, p)).transpose()?;
        let added: std::collections::HashSet<&str> = files.iter().filter(|f| f.blob.is_some()).map(|f| f.path.as_str()).collect();
        let deleted: std::collections::HashSet<&str> = files.iter().filter(|f| f.blob.is_none()).map(|f| f.path.as_str()).collect();
        let msg = |p: &str, c: &str| format!("{p} conflicts with {c}: a path cannot be both a file and a directory");
        for f in files.iter().filter(|f| f.blob.is_some()) {
            let mut i = 0;
            while let Some(j) = f.path[i..].find('/') {
                let q = &f.path[..i + j];
                if added.contains(q) {
                    return Ok(Some(msg(&f.path, q)));
                }
                if let Some(r) = &root {
                    if self.lookup(r, q)?.is_some_and(|e| e.kind != "tree") && !deleted.contains(q) {
                        return Ok(Some(msg(&f.path, q)));
                    }
                }
                i += j + 1;
            }
            if let Some(r) = &root {
                if let Some(e) = self.lookup(r, &f.path)?.filter(|e| e.kind == "tree") {
                    let mut ps = vec![];
                    self.file_paths(&e.id, &f.path, &mut ps)?;
                    if let Some(left) = ps.into_iter().find(|p| !deleted.contains(p.as_str())) {
                        return Ok(Some(msg(&f.path, &left)));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Manifest entry back to (blob, kind) — used by the fetch API.
pub fn kind_of_entry(e: &str) -> (&str, &'static str) {
    let (b, m) = split_entry(e);
    (b, kind_of(m))
}
