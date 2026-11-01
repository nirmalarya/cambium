//! Filesystem monitor: `cmb watch` records which paths change so `cmb status`
//! (and everything built on it: add, commit, diff) hashes only those paths
//! instead of every file in the working copy.
//!
//! Files in `.cambium/`:
//!   watch.pid       the running watcher's pid (status ignores a dead one)
//!   watch.dirty     paths that changed since the watcher started (appended)
//!   watch.baseline  paths that differed when the first full scan ran while
//!                   the watcher was live; missing => next status does a full scan
//! If the kernel queue overflows the watcher deletes the baseline, so the next
//! status falls back to a full scan instead of trusting a lossy event list.
use notify::{RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

fn meta_file(root: &Path, f: &str) -> std::path::PathBuf {
    crate::meta(root).join(f)
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Paths to examine instead of a full walk, if a live watcher has a baseline.
pub fn candidates(root: &Path) -> Option<BTreeSet<String>> {
    let pid: u32 = std::fs::read_to_string(meta_file(root, "watch.pid")).ok()?.trim().parse().ok()?;
    if !alive(pid) {
        return None;
    }
    let base = std::fs::read_to_string(meta_file(root, "watch.baseline")).ok()?;
    let dirty = std::fs::read_to_string(meta_file(root, "watch.dirty")).unwrap_or_default();
    Some(base.lines().chain(dirty.lines()).filter(|l| !l.is_empty()).map(String::from).collect())
}
/// After a full scan while a watcher is live, remember what differed then.
pub fn record_baseline(root: &Path, changed: &[String]) {
    let Ok(p) = std::fs::read_to_string(meta_file(root, "watch.pid")) else { return };
    if !p.trim().parse().map(alive).unwrap_or(false) {
        return;
    }
    let tmp = meta_file(root, "watch.baseline.tmp");
    if std::fs::write(&tmp, changed.join("\n")).is_ok() {
        let _ = std::fs::rename(tmp, meta_file(root, "watch.baseline"));
    }
}

pub fn run(root: &Path) -> ! {
    let root = root.canonicalize().expect("working copy");
    for f in ["watch.dirty", "watch.baseline"] {
        let _ = std::fs::remove_file(meta_file(&root, f));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = notify::recommended_watcher(tx).unwrap_or_else(|e| crate::die(format!("watch: {e}")));
    w.watch(&root, RecursiveMode::Recursive).unwrap_or_else(|e| crate::die(format!("watch: {e}")));
    // the watch is armed before the pid appears, so no change after this point is missed
    std::fs::write(meta_file(&root, "watch.pid"), std::process::id().to_string()).unwrap();
    eprintln!("watching {} (Ctrl-C to stop)", root.display());
    let bvdir = root.join(".cambium");
    for ev in rx {
        let ev = match ev {
            Ok(ev) if !ev.need_rescan() => ev,
            _ => {
                let _ = std::fs::remove_file(meta_file(&root, "watch.baseline"));
                continue;
            }
        };
        let mut lines = String::new();
        for p in ev.paths {
            if p.starts_with(&bvdir) {
                continue;
            }
            if let Ok(r) = p.strip_prefix(&root) {
                let r = r.to_string_lossy().replace('\\', "/");
                if !r.is_empty() {
                    lines.push_str(&r);
                    lines.push('\n');
                }
            }
        }
        if lines.is_empty() {
            continue;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(meta_file(&root, "watch.dirty")) {
            let _ = f.write_all(lines.as_bytes());
        }
    }
    std::process::exit(0)
}
