//! Git mirror: export `main` and `release/*` to a git repository so git-based tools
//! (CI systems, IDEs, review bots) keep working during and after a migration.
//! Incremental: each changeset becomes one git commit with exactly its file changes
//! (merges keep both parents); `git_export_map` remembers the result, so later runs
//! only add new changesets and existing git commits never change.
use crate::engine::{Engine, Error, Result, MODE_EXEC, MODE_LINK};
use crate::p;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Default)]
pub struct ExportReport {
    pub commits: usize,
    pub refs: Vec<(String, String)>,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| Error::Bad(format!("git: {e}")))?;
    if !o.status.success() {
        return Err(Error::Bad(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// "Name <email>" as git wants it; plain user names get a placeholder address.
fn ident(author: &str) -> String {
    let a = author.replace(['\n', '<', '>'], " ");
    match author.split_once(" <") {
        Some((name, rest)) if rest.ends_with('>') && !rest[..rest.len() - 1].contains(['<', '>', '\n']) => {
            format!("{} <{}>", name.replace(['\n', '<', '>'], " ").trim(), &rest[..rest.len() - 1])
        }
        _ => format!("{} <{}@users.cambium.invalid>", a.trim(), a.trim().replace(' ', ".")),
    }
}

impl Engine {
    /// Export `repo`'s main and release/* refs into the git repository at `git_dir`
    /// (created as a bare repository if missing). Refuses repositories with permission
    /// rules unless `allow_restricted`: a mirror contains every file.
    pub fn export_git(&self, repo: &str, git_dir: &Path, allow_restricted: bool) -> Result<ExportReport> {
        self.check_repo_exists(repo)?;
        if !allow_restricted && !self.acl(repo)?.is_empty() {
            return Err(Error::Forbidden(format!(
                "{repo} has permission rules; a git mirror would include every directory. Pass --allow-restricted if that is intended"
            )));
        }
        if !git_dir.join("HEAD").exists() && !git_dir.join(".git").exists() {
            std::fs::create_dir_all(git_dir)?;
            git(git_dir, &["init", "-q", "--bare"])?;
        }
        // the mapping is per mirror: another destination starts from scratch
        let mirror = git_dir.canonicalize()?.to_string_lossy().to_string();
        let mut sha_of: HashMap<String, String> = self
            .db
            .query("SELECT cs_id,git_sha FROM git_export_map WHERE repo=?1 AND mirror=?2", p![repo, &mirror])?
            .into_iter()
            .map(|r| (r.s(0), r.s(1)))
            .collect();
        // the mirror was deleted or replaced since the last run: start it afresh
        if let Some(any) = sha_of.values().next() {
            if git(git_dir, &["cat-file", "-e", &format!("{any}^{{commit}}")]).is_err() {
                self.db.exec("DELETE FROM git_export_map WHERE repo=?1 AND mirror=?2", p![repo, &mirror])?;
                sha_of.clear();
            }
        }
        let refs: Vec<(String, String)> =
            self.list_refs(repo)?.into_iter().filter(|(n, _, _)| crate::engine::valid_ref_name(n)).map(|(n, c, _)| (n, c)).collect();
        // changesets to export: reachable from the refs, not exported yet, oldest first
        let mut todo: Vec<crate::engine::Changeset> = vec![];
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = refs.iter().map(|(_, c)| c.clone()).collect();
        while let Some(id) = stack.pop() {
            if sha_of.contains_key(&id) || !seen.insert(id.clone()) {
                continue;
            }
            let c = self.get_changeset(repo, &id)?;
            stack.extend(c.parents.iter().cloned());
            todo.push(c);
        }
        todo.sort_by_key(|c| c.seq);
        let mut rep = ExportReport::default();
        if !todo.is_empty() {
            let marks = git_dir.join(format!("cambium-export-{}.marks", std::process::id()));
            let mut child = Command::new("git")
                .arg("-C")
                .arg(git_dir)
                .args(["fast-import", "--quiet", "--done"])
                .arg(format!("--export-marks={}", marks.display()))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .map_err(|e| Error::Bad(format!("git fast-import: {e}")))?;
            let mut w = std::io::BufWriter::new(child.stdin.take().unwrap());
            let mut mark_of: HashMap<String, usize> = HashMap::new();
            let blob_written: std::cell::RefCell<HashMap<String, usize>> = Default::default();
            let mut next = 1usize;
            let r#ref = |c: &str, mark_of: &HashMap<String, usize>, sha_of: &HashMap<String, String>| -> Option<String> {
                mark_of.get(c).map(|m| format!(":{m}")).or_else(|| sha_of.get(c).cloned())
            };
            for c in &todo {
                // blobs first (each once per run), then the commit referencing them
                for f in &c.files {
                    if let Some(b) = &f.blob {
                        if !blob_written.borrow().contains_key(b) {
                            let data = self.blobs.get(b)?.ok_or_else(|| Error::NotFound(format!("blob {b}")))?;
                            writeln!(w, "blob\nmark :{next}\ndata {}", data.len())?;
                            w.write_all(&data)?;
                            writeln!(w)?;
                            blob_written.borrow_mut().insert(b.clone(), next);
                            next += 1;
                        }
                    }
                }
                let me = next;
                next += 1;
                let who = ident(&c.author);
                let msg = if c.message.is_empty() { "(no message)".to_string() } else { c.message.clone() };
                if c.parents.is_empty() {
                    // a root commit: start the scratch branch afresh (no parent)
                    writeln!(w, "reset refs/cambium/export\n")?;
                }
                writeln!(w, "commit refs/cambium/export\nmark :{me}")?;
                writeln!(w, "author {who} {} +0000\ncommitter {who} {} +0000", c.created_at, c.created_at)?;
                writeln!(w, "data {}\n{msg}", msg.len())?;
                if let Some(p1) = c.parents.first() {
                    writeln!(w, "from {}", r#ref(p1, &mark_of, &sha_of).ok_or_else(|| Error::Db(format!("parent {p1} not exported")))?)?;
                }
                for p2 in c.parents.iter().skip(1) {
                    writeln!(w, "merge {}", r#ref(p2, &mark_of, &sha_of).ok_or_else(|| Error::Db(format!("parent {p2} not exported")))?)?;
                }
                for f in &c.files {
                    match &f.blob {
                        Some(b) => {
                            let mode = match f.mode {
                                MODE_EXEC => "100755",
                                MODE_LINK => "120000",
                                _ => "100644",
                            };
                            writeln!(w, "M {mode} :{} {}", blob_written.borrow()[b], quote_path(&f.path))?;
                        }
                        None => writeln!(w, "D {}", quote_path(&f.path))?,
                    }
                }
                writeln!(w)?;
                mark_of.insert(c.id.clone(), me);
            }
            for (name, cs) in &refs {
                if let Some(target) = r#ref(cs, &mark_of, &sha_of) {
                    writeln!(w, "reset refs/heads/{name}\nfrom {target}\n")?;
                }
            }
            writeln!(w, "done")?;
            w.flush()?;
            drop(w);
            let st = child.wait()?;
            if !st.success() {
                return Err(Error::Bad("git fast-import failed".into()));
            }
            // record changeset -> commit for the next run
            let marks_text = std::fs::read_to_string(&marks)?;
            let _ = std::fs::remove_file(&marks);
            let by_mark: HashMap<usize, String> = marks_text
                .lines()
                .filter_map(|l| l.strip_prefix(':')?.split_once(' ').and_then(|(m, s)| Some((m.parse().ok()?, s.to_string()))))
                .collect();
            for (cs, m) in &mark_of {
                let sha = by_mark.get(m).ok_or_else(|| Error::Db("fast-import marks incomplete".into()))?.clone();
                self.db.exec(
                    "INSERT INTO git_export_map(repo,mirror,cs_id,git_sha) VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
                    p![repo, &mirror, cs, &sha],
                )?;
                sha_of.insert(cs.clone(), sha);
            }
            let _ = git(git_dir, &["update-ref", "-d", "refs/cambium/export"]);
            rep.commits = todo.len();
        } else {
            // nothing new, but refs may have moved to already-exported changesets
            for (name, cs) in &refs {
                if let Some(sha) = sha_of.get(cs) {
                    git(git_dir, &["update-ref", &format!("refs/heads/{name}"), sha])?;
                }
            }
        }
        for (name, cs) in &refs {
            if let Some(sha) = sha_of.get(cs) {
                rep.refs.push((name.clone(), sha.clone()));
            }
        }
        if git(git_dir, &["symbolic-ref", "HEAD"]).ok().as_deref() != Some("refs/heads/main") {
            let _ = git(git_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        }
        Ok(rep)
    }
    fn check_repo_exists(&self, repo: &str) -> Result<()> {
        self.db.one("SELECT 1 FROM repos WHERE name=?1", p![repo])?.map(|_| ()).ok_or_else(|| Error::NotFound(format!("repo {repo}")))
    }
}

/// fast-import path quoting (C-style when needed).
fn quote_path(p: &str) -> String {
    if p.bytes().all(|b| b > 0x20 && b != b'"' && b != b'\\' && b < 0x7f) {
        return p.to_string();
    }
    let mut o = String::from("\"");
    for ch in p.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
