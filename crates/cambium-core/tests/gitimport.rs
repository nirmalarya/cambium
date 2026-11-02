//! Git history import, against a real repository built with the `git` binary.
use cambium_core::{
    blob::MemBlobStore,
    engine::{split_entry, MODE_EXEC, MODE_LINK},
    Engine,
};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn g(dir: &Path, args: &[&str], date: i64) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Ada Example",
            "-c",
            "user.email=ada@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_AUTHOR_DATE", format!("@{date} +0000"))
        .env("GIT_COMMITTER_DATE", format!("@{date} +0000"))
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}
fn w(dir: &Path, p: &str, s: &str) {
    let f = dir.join(p);
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(f, s).unwrap();
}

#[test]
fn imports_full_history_with_branches_merges_and_modes() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().join("src");
    std::fs::create_dir(&r).unwrap();
    g(&r, &["init", "-q"], 0);
    w(&r, "README.md", "hello\n");
    w(&r, "src/lib.rs", "fn a() {}\n");
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "first"], 1_600_000_000);
    g(&r, &["checkout", "-qb", "feature"], 0);
    w(&r, "src/feat.rs", "fn f() {}\n");
    w(&r, "run.sh", "#!/bin/sh\necho hi\n");
    std::process::Command::new("chmod").arg("+x").arg(r.join("run.sh")).status().unwrap();
    std::os::unix::fs::symlink("README.md", r.join("link")).unwrap();
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "feature work\n\nwith a body"], 1_600_000_100);
    g(&r, &["checkout", "-q", "main"], 0);
    w(&r, "README.md", "hello\nworld\n");
    g(&r, &["rm", "-q", "src/lib.rs"], 0);
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "main work"], 1_600_000_200);
    g(&r, &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"], 1_600_000_300);
    g(&r, &["tag", "-a", "v1", "-m", "release"], 1_600_000_300);
    g(&r, &["mv", "src/feat.rs", "src/renamed.rs"], 0);
    g(&r, &["commit", "-qm", "rename"], 1_600_000_400);

    let dd = tempfile::tempdir().unwrap();
    let e = Engine::open(dd.path().join("cambium.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let rep = e.import_git(&r, "imp", "alice").unwrap();
    assert_eq!(rep.commits, 5);
    assert!(rep.skipped.is_empty(), "{:?}", rep.skipped);
    let names: Vec<&str> = rep.refs.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"main") && names.contains(&"feature") && names.contains(&"tags/v1"), "{names:?}");

    // every ref's tree equals Git's, byte for byte
    for (bvref, gitref) in [("main", "main"), ("feature", "feature"), ("tags/v1", "v1")] {
        let cs = e.get_ref("imp", bvref).unwrap().unwrap();
        let m = e.manifest("imp", &cs).unwrap();
        let ls = g(&r, &["ls-tree", "-r", gitref], 0);
        assert_eq!(m.len(), ls.lines().count(), "{bvref}");
        for l in ls.lines() {
            let (meta, path) = l.split_once('\t').unwrap();
            let sha = meta.split_whitespace().nth(2).unwrap();
            let want = Command::new("git").arg("-C").arg(&r).args(["cat-file", "blob", sha]).output().unwrap().stdout;
            let (blob, mode) = split_entry(&m[path]);
            assert_eq!(e.blobs.get(blob).unwrap().unwrap(), want, "{bvref}:{path}");
            if path == "run.sh" {
                assert_eq!(mode, MODE_EXEC);
            }
            if path == "link" {
                assert_eq!(mode, MODE_LINK);
            }
        }
    }
    // history: order, authors, timestamps, messages, merge parents
    let main = e.get_ref("imp", "main").unwrap().unwrap();
    let log = e.log("imp", &main, None, 100).unwrap();
    let msgs: Vec<&str> = log.iter().map(|c| c.message.as_str()).collect();
    // the two merge parents have the same generation; their relative order is a hash tie-break
    assert_eq!(&msgs[..2], &["rename", "merge feature"][..], "{msgs:?}");
    let mut mid = msgs[2..4].to_vec();
    mid.sort();
    assert_eq!(mid, ["feature work\n\nwith a body", "main work"], "{msgs:?}");
    assert_eq!(msgs[4], "first", "{msgs:?}");
    assert_eq!(log[0].author, "Ada Example <ada@example.invalid>");
    assert_eq!(log[0].created_at, 1_600_000_400);
    assert_eq!(log[1].parents.len(), 2, "merge keeps both parents");
    let feature = e.get_ref("imp", "feature").unwrap().unwrap();
    assert!(e.is_ancestor("imp", &feature, &main).unwrap());
    // path history works on imported data
    let lp = e.log("imp", &main, Some("src/renamed.rs"), 10).unwrap();
    assert_eq!(lp.len(), 1);
    // re-importing into the same name is refused rather than merged
    assert!(e.import_git(&r, "imp", "alice").is_err());
}

#[test]
fn disallowed_paths_are_skipped_and_failed_imports_can_be_retried() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    g(&src, &["init", "-q"], 1_700_000_000);
    w(&src, "ok.txt", "fine\n");
    w(&src, ".cambium/state.json", "{}\n");
    #[cfg(unix)]
    w(&src, "back\\slash.txt", "odd\n");
    g(&src, &["add", "-A"], 1_700_000_000);
    g(&src, &["commit", "-qm", "c1"], 1_700_000_000);

    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    // a failed import (not a git repo) leaves nothing behind...
    assert!(e.import_git(&d.path().join("missing"), "r", "u").is_err());
    // ...so the same name can be imported right after
    let rep = e.import_git(&src, "r", "u").unwrap();
    assert_eq!(rep.commits, 1);
    assert!(rep.skipped.iter().any(|s| s.contains(".cambium/state.json")), "{:?}", rep.skipped);
    let main = e.get_ref("r", "main").unwrap().unwrap();
    let m = e.manifest("r", &main).unwrap();
    assert!(m.contains_key("ok.txt") && !m.keys().any(|k| k.starts_with(".cambium")));
}
