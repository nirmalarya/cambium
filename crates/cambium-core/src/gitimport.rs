//! Import a Git repository with its full history.
//!
//! Every commit reachable from a branch or tag becomes a changeset (oldest
//! first, parents before children) with its original author, timestamp and
//! message; merges keep both parents. Branches and tags become refs (`tags/<name>`).
//! The local `git` binary does the reading (rev-list, diff-tree, cat-file),
//! so no Git library is linked in.
use crate::engine::{Engine, Error, FileChange, Result, MODE_EXEC, MODE_FILE, MODE_LINK};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Default)]
pub struct ImportReport {
    pub commits: usize,
    pub blobs: usize,
    pub refs: Vec<(String, String)>,
    pub skipped: Vec<String>,
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| Error::Bad(format!("git: {e}")))?;
    if !o.status.success() {
        return Err(Error::Bad(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())));
    }
    Ok(o.stdout)
}

/// Long-running `git cat-file --batch` for blob contents.
struct Cat {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
}
impl Cat {
    fn new(dir: &Path) -> Result<Cat> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Bad(format!("git: {e}")))?;
        let out = BufReader::new(child.stdout.take().unwrap());
        Ok(Cat { child, out })
    }
    fn get(&mut self, sha: &str) -> Result<Vec<u8>> {
        let io = |e: std::io::Error| Error::Bad(format!("git cat-file: {e}"));
        writeln!(self.child.stdin.as_mut().unwrap(), "{sha}").map_err(io)?;
        let mut hdr = String::new();
        self.out.read_line(&mut hdr).map_err(io)?;
        let size: usize = hdr
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::Bad(format!("git cat-file: unexpected reply {hdr:?}")))?;
        let mut buf = vec![0; size + 1]; // content + trailing newline
        self.out.read_exact(&mut buf).map_err(io)?;
        buf.truncate(size);
        Ok(buf)
    }
}
impl Drop for Cat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mode(m: &str) -> Option<u32> {
    match m {
        "100644" | "100664" => Some(MODE_FILE),
        "100755" => Some(MODE_EXEC),
        "120000" => Some(MODE_LINK),
        _ => None,
    }
}

impl Engine {
    /// Import `git_dir` into a new repository `repo` owned by `owner`. All or nothing:
    /// if the import fails part-way, the half-built repo is removed so it can be retried.
    ///
    /// If `repo` already exists and was imported from git, only commits not yet
    /// imported are added and `main`/`release/*` are fast-forwarded (incremental
    /// migration: git stays live while teams move over). A ref that has diverged in
    /// Cambium is left alone and reported.
    pub fn import_git(&self, git_dir: &Path, repo: &str, owner: &str) -> Result<ImportReport> {
        let known: HashMap<String, String> = self
            .db
            .query("SELECT git_sha,cs_id FROM git_import_map WHERE repo=?1", crate::p![repo])?
            .into_iter()
            .map(|r| (r.s(0), r.s(1)))
            .collect();
        let exists = self.db.one("SELECT 1 FROM repos WHERE name=?1", crate::p![repo])?.is_some();
        if exists {
            if known.is_empty() {
                return Err(Error::Bad(format!("repo {repo} exists and was not imported from git")));
            }
            let root = self.get_changeset(repo, &self.resolve(repo, "main")?)?.id; // only used for a source without main
            return self.import_into(git_dir, repo, &root, known, true);
        }
        let root = self.create_repo(repo, owner)?;
        self.import_into(git_dir, repo, &root, known, false).inspect_err(|e| {
            crate::log::error("git import failed; removing the partial repo", serde_json::json!({"repo": repo, "error": e.to_string()}));
            let _ = self.discard_repo(repo);
        })
    }
    fn import_into(
        &self,
        git_dir: &Path,
        repo: &str,
        root: &str,
        known: HashMap<String, String>,
        incremental: bool,
    ) -> Result<ImportReport> {
        let root = root.to_string();
        let mut rep = ImportReport::default();
        let log =
            git(git_dir, &["log", "--topo-order", "--reverse", "--branches", "--tags", "--format=%H%x00%P%x00%an <%ae>%x00%at%x00%B%x1e"])?;
        let mut cat = Cat::new(git_dir)?;
        let mut cs_of: HashMap<String, String> = known;
        let mut blob_of: HashMap<String, String> = HashMap::new();
        for rec in log.split(|b| *b == 0x1e) {
            let rec = String::from_utf8_lossy(rec);
            let rec = rec.trim_start_matches('\n');
            if rec.is_empty() {
                continue;
            }
            let f: Vec<&str> = rec.splitn(5, '\0').collect();
            if f.len() < 5 {
                return Err(Error::Bad("unexpected git log output".into()));
            }
            let (sha, parents, author, ts) = (f[0], f[1], f[2], f[3].parse::<i64>().unwrap_or(0));
            if cs_of.contains_key(sha) {
                continue; // imported by an earlier run
            }
            let mut msg = f[4].trim_end().to_string();
            let gp: Vec<&str> = parents.split_whitespace().collect();
            if gp.len() > 2 {
                msg.push_str(&format!("\n\n(imported octopus merge; extra parents: {})", gp[2..].join(" ")));
            }
            // changes relative to the first parent (or everything, for a root commit)
            let raw = match gp.first() {
                Some(p1) => git(git_dir, &["diff-tree", "-r", "-z", "--no-renames", "--raw", p1, sha])?,
                None => git(git_dir, &["diff-tree", "-r", "-z", "--no-renames", "--raw", "--root", sha])?,
            };
            let mut files = vec![];
            let mut it = raw.split(|b| *b == 0).filter(|x| !x.is_empty());
            // --root/-r output for a single commit starts with the commit id; skip it
            while let Some(meta) = it.next() {
                let meta = String::from_utf8_lossy(meta);
                if !meta.starts_with(':') {
                    continue;
                }
                let Some(path) = it.next() else { break };
                let path = String::from_utf8_lossy(path).to_string();
                if !crate::engine::valid_path(&path) {
                    // git allows paths we do not (backslashes, a .cambium folder, ...)
                    rep.skipped.push(format!("{} {path:?} (path not allowed)", &sha[..12]));
                    continue;
                }
                let m: Vec<&str> = meta[1..].split_whitespace().collect();
                let (new_mode, new_sha, status) = (m[1], m[3], m[4]);
                if status == "D" {
                    files.push(FileChange { path, blob: None, mode: MODE_FILE });
                    continue;
                }
                let Some(md) = mode(new_mode) else {
                    rep.skipped.push(format!("{} {path} (mode {new_mode})", &sha[..12]));
                    if mode(m[0]).is_some() {
                        files.push(FileChange { path, blob: None, mode: MODE_FILE });
                    }
                    continue;
                };
                let id = match blob_of.get(new_sha) {
                    Some(id) => id.clone(),
                    None => {
                        let id = self.put_blob(&cat.get(new_sha)?)?;
                        rep.blobs += 1;
                        blob_of.insert(new_sha.to_string(), id.clone());
                        id
                    }
                };
                files.push(FileChange { path, blob: Some(id), mode: md });
            }
            let ps: Vec<String> = if gp.is_empty() {
                vec![root.clone()]
            } else {
                gp.iter()
                    .take(2)
                    .map(|p| cs_of.get(*p).cloned().ok_or_else(|| Error::Bad(format!("parent {p} not imported yet"))))
                    .collect::<Result<_>>()?
            };
            let cs = self.create_changeset_at(repo, ps, author, &msg, files, ts)?;
            self.db.exec(
                "INSERT INTO git_import_map(repo,git_sha,cs_id) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
                crate::p![repo, sha, &cs.id],
            )?;
            cs_of.insert(sha.to_string(), cs.id);
            rep.commits += 1;
        }
        let refs = String::from_utf8_lossy(&git(
            git_dir,
            &["for-each-ref", "--format=%(refname)%00%(objectname)%00%(*objectname)", "refs/heads", "refs/tags"],
        )?)
        .to_string();
        for l in refs.lines() {
            let f: Vec<&str> = l.split('\0').collect();
            let target = if f.get(2).is_some_and(|x| !x.is_empty()) { f[2] } else { f[1] };
            let name = if let Some(b) = f[0].strip_prefix("refs/heads/") {
                b.to_string()
            } else {
                format!("tags/{}", f[0].trim_start_matches("refs/tags/"))
            };
            if !crate::engine::valid_ref_name(&name) {
                // Scion's model: only main and release branches; the commits stay
                // imported (reachable by id), only the ref is not created
                rep.skipped.push(format!("ref {name} (only main and release/* refs are imported)"));
                continue;
            }
            let Some(cs) = cs_of.get(target) else {
                rep.skipped.push(format!("ref {name} (points at a non-commit)"));
                continue;
            };
            let cur = self.get_ref(repo, &name)?;
            if cur.as_deref() == Some(cs.as_str()) {
                continue; // unchanged since the last run
            }
            if let Some(c) = &cur {
                // move only along imported history; never over work done in Cambium
                let imported = cs_of.values().any(|v| v == c);
                if incremental && !(imported && self.is_ancestor(repo, c, cs)?) {
                    rep.skipped.push(format!("ref {name} (diverged in Cambium; not moved)"));
                    continue;
                }
            }
            match self.cas_ref(repo, &name, cur.as_deref(), Some(cs)) {
                Ok(()) => rep.refs.push((name, cs.clone())),
                Err(e) => rep.skipped.push(format!("ref {name}: {e}")),
            }
        }
        // no `main` branch in the source: point main at the source's HEAD (first run)
        if !incremental && !rep.refs.iter().any(|(n, _)| n == "main") {
            if let Ok(h) = git(git_dir, &["rev-parse", "HEAD"]) {
                if let Some(cs) = cs_of.get(String::from_utf8_lossy(&h).trim()) {
                    self.cas_ref(repo, "main", Some(&root), Some(cs))?;
                    rep.refs.push(("main".into(), cs.clone()));
                }
            }
        }
        Ok(rep)
    }
}
