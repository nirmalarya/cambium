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

    g(&r, &["branch", "release/1.0", "feature"], 0);
    let dd = tempfile::tempdir().unwrap();
    let e = Engine::open(dd.path().join("cambium.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let rep = e.import_git(&r, "imp", "alice").unwrap();
    assert_eq!(rep.commits, 5);
    let names: Vec<&str> = rep.refs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["main", "release/1.0"], "only main and release refs");
    assert!(
        rep.skipped.iter().any(|s| s.contains("ref feature")) && rep.skipped.iter().any(|s| s.contains("ref tags/v1")),
        "{:?}",
        rep.skipped
    );

    // every ref's tree equals Git's, byte for byte
    for (bvref, gitref) in [("main", "main"), ("release/1.0", "feature")] {
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
    let feature = e.get_ref("imp", "release/1.0").unwrap().unwrap();
    assert!(e.is_ancestor("imp", &feature, &main).unwrap());
    // path history works on imported data
    let lp = e.log("imp", &main, Some("src/renamed.rs"), 10).unwrap();
    assert_eq!(lp.len(), 1);
    // running again with nothing new imports nothing and moves nothing
    let again = e.import_git(&r, "imp", "alice").unwrap();
    assert_eq!((again.commits, again.refs.len()), (0, 0), "{again:?}");
    assert_eq!(e.get_ref("imp", "main").unwrap(), Some(main.clone()));

    // incremental: new git commits arrive while git is still in use
    w(&r, "src/later.rs", "later\n");
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "later work"], 1_600_000_500);
    let inc = e.import_git(&r, "imp", "alice").unwrap();
    assert_eq!(inc.commits, 1);
    let main2 = e.get_ref("imp", "main").unwrap().unwrap();
    assert_eq!(e.get_changeset("imp", &main2).unwrap().parents, vec![main.clone()], "fast-forwarded");
    assert!(e.manifest("imp", &main2).unwrap().contains_key("src/later.rs"));

    // someone commits to main in Cambium; a later git commit must not overwrite it
    let b = e.put_blob(b"x\n").unwrap();
    let mine = e
        .commit(
            "imp",
            "main",
            &main2,
            "bob",
            "cambium work",
            vec![cambium_core::FileChange { path: "c.txt".into(), blob: Some(b), mode: 0o100644 }],
        )
        .unwrap()
        .id;
    w(&r, "src/more.rs", "more\n");
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "more git work"], 1_600_000_600);
    let div = e.import_git(&r, "imp", "alice").unwrap();
    assert_eq!(div.commits, 1, "the commit itself is still imported");
    assert!(div.skipped.iter().any(|s| s.contains("ref main (diverged")), "{:?}", div.skipped);
    assert_eq!(e.get_ref("imp", "main").unwrap(), Some(mine), "Cambium work kept");
}

#[test]
fn existing_repo_not_from_git_is_not_imported_into() {
    let d = tempfile::tempdir().unwrap();
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    e.create_repo("r", "u").unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    g(&src, &["init", "-q"], 0);
    w(&src, "a", "1\n");
    g(&src, &["add", "-A"], 0);
    g(&src, &["commit", "-qm", "c"], 1_600_000_000);
    assert!(e.import_git(&src, "r", "u").is_err());
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

#[test]
fn export_to_git_is_faithful_and_incremental() {
    use cambium_core::FileChange;
    let d = tempfile::tempdir().unwrap();
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let root = e.create_repo("r", "u").unwrap();
    let put = |p: &str, c: &str, mode: u32| FileChange { path: p.into(), blob: Some(e.put_blob(c.as_bytes()).unwrap()), mode };
    let c1 = e
        .commit(
            "r",
            "main",
            &root,
            "Ada Example <ada@example.invalid>",
            "first",
            vec![
                put("a.txt", "one\n", 0o100644),
                put("bin/run.sh", "#!/bin/sh\n", MODE_EXEC),
                put("link", "a.txt", MODE_LINK),
                put("sp ace/é.txt", "u\n", 0o100644),
            ],
        )
        .unwrap()
        .id;
    e.cas_ref("r", "release/1", None, Some(&c1)).unwrap();
    let fix = e.commit("r", "release/1", &c1, "bob", "fix on release", vec![put("fix.txt", "fix\n", 0o100644)]).unwrap().id;
    let c2 = e
        .commit(
            "r",
            "main",
            &c1,
            "alice",
            "second\n\nwith a body",
            vec![put("a.txt", "two\n", 0o100644), FileChange { path: "link".into(), blob: None, mode: 0o100644 }],
        )
        .unwrap()
        .id;
    let m = e.merge("r", "main", "release/1", "alice").unwrap().id;
    let gd = d.path().join("mirror.git");
    let rep = e.export_git("r", &gd, false).unwrap();
    assert_eq!(rep.commits, 5, "root, first, fix, second, merge");
    // every ref's tree matches Cambium's manifest, byte for byte
    for (r#ref, cs) in [("main", &m), ("release/1", &fix)] {
        let man = e.manifest("r", cs).unwrap();
        let ls = g(&gd, &["-c", "core.quotepath=off", "ls-tree", "-r", r#ref], 0);
        assert_eq!(ls.lines().count(), man.len(), "{ref}");
        for l in ls.lines() {
            let (meta, path) = l.split_once('\t').unwrap();
            let mut it = meta.split_whitespace();
            let (mode, sha) = (it.next().unwrap(), it.nth(1).unwrap());
            let want = e.blobs.get(split_entry(&man[path.trim_matches('"')]).0).unwrap().unwrap();
            let got = Command::new("git").arg("-C").arg(&gd).args(["cat-file", "blob", sha]).output().unwrap().stdout;
            assert_eq!(got, want, "{ref}:{path}");
            if path == "bin/run.sh" {
                assert_eq!(mode, "100755");
            }
        }
    }
    assert!(g(&gd, &["log", "--format=%P", "-1", "main"], 0).split_whitespace().count() == 2, "merge keeps both parents");
    assert!(g(&gd, &["log", "--format=%an <%ae>|%s", "main~1"], 0).starts_with("alice <alice@users.cambium.invalid>|second"));
    g(&gd, &["fsck", "--strict"], 0);
    // incremental: a new changeset adds one commit; old commits keep their ids
    let before = g(&gd, &["rev-parse", "main"], 0);
    e.commit("r", "main", &m, "alice", "third", vec![put("c.txt", "3\n", 0o100644)]).unwrap();
    let rep2 = e.export_git("r", &gd, false).unwrap();
    assert_eq!(rep2.commits, 1);
    assert_eq!(g(&gd, &["rev-parse", "main~1"], 0), before);
    assert_eq!(e.export_git("r", &gd, false).unwrap().commits, 0);
    // repositories with permission rules need explicit consent
    e.set_acl("r", &cambium_core::Principal::new("u"), "", "user:u", Some(cambium_core::Perm::Admin)).unwrap();
    assert!(e.export_git("r", &d.path().join("other.git"), false).is_err());
    assert!(e.export_git("r", &d.path().join("other.git"), true).is_ok());
    let _ = c2;
}

#[test]
fn messages_with_any_bytes_import_intact() {
    // a commit message may contain the record separator the old log parser used
    // (0x1E), other control bytes and non-UTF-8-looking text
    let d = tempfile::tempdir().unwrap();
    let r = d.path().join("src");
    std::fs::create_dir(&r).unwrap();
    g(&r, &["init", "-q"], 0);
    w(&r, "a.txt", "1\n");
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "before \u{1e} after\n\nbody \u{1f} line\u{1e}"], 1_600_000_000);
    w(&r, "a.txt", "2\n");
    g(&r, &["add", "-A"], 0);
    g(&r, &["commit", "-qm", "second"], 1_600_000_100);
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let rep = e.import_git(&r, "imp", "alice").unwrap();
    assert_eq!(rep.commits, 2);
    let head = e.get_ref("imp", "main").unwrap().unwrap();
    let log = e.log("imp", &head, None, 10).unwrap();
    assert_eq!(log.len(), 3, "two commits on the repository's root changeset");
    assert_eq!(log[0].message, "second");
    assert_eq!(log[1].message, "before \u{1e} after\n\nbody \u{1f} line\u{1e}");
    assert_eq!(log[1].author, "Ada Example <ada@example.invalid>");
}
