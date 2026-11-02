use cambium_core::{
    blob::{BlobStore, FsBlobStore, MemBlobStore},
    Engine, Error, FileChange,
};
use std::sync::Arc;

use cambium_core::engine::{split_entry, MODE_EXEC, MODE_FILE, MODE_LINK};
fn exec(e: &Engine, path: &str, s: &str) -> FileChange {
    FileChange { mode: MODE_EXEC, ..put(e, path, s) }
}
fn eng() -> (Engine, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    (e, d)
}
fn put(e: &Engine, path: &str, s: &str) -> FileChange {
    FileChange { path: path.into(), blob: Some(e.blobs.put(s.as_bytes()).unwrap()), mode: MODE_FILE }
}
fn read(e: &Engine, cs: &str, p: &str) -> String {
    e.read_text(split_entry(&e.manifest("r", cs).unwrap()[p]).0).unwrap()
}

#[test]
fn init_creates_main() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    assert_eq!(e.get_ref("r", "main").unwrap(), Some(root));
    assert!(e.create_repo("r", "u").is_err());
    assert!(e.create_repo("bad name", "u").is_err());
}
#[test]
fn commit_and_manifest() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "one", vec![put(&e, "a.txt", "hi\n"), put(&e, "d/b.txt", "b\n")]).unwrap();
    let c2 = e.commit("r", "main", &c1.id, "u", "two", vec![FileChange { path: "a.txt".into(), blob: None, mode: MODE_FILE }]).unwrap();
    assert_eq!(e.manifest("r", &c1.id).unwrap().len(), 2);
    assert_eq!(e.manifest("r", &c2.id).unwrap().len(), 1);
    assert_eq!(c2.files.len(), 1, "changeset stores only the delta");
}
#[test]
fn stale_commit_rejected() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    e.commit("r", "main", &root, "u", "one", vec![put(&e, "a", "1\n")]).unwrap();
    assert!(matches!(e.commit("r", "main", &root, "u", "two", vec![put(&e, "a", "2\n")]), Err(Error::RefMoved { .. })));
}
#[test]
fn cas_semantics() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    e.cas_ref("r", "feat", None, Some(&root)).unwrap();
    assert!(e.cas_ref("r", "feat", None, Some(&root)).is_err(), "create twice fails");
    let c = e.create_changeset("r", vec![root.clone()], "u", "x", vec![put(&e, "a", "a\n")]).unwrap();
    assert!(e.get_ref("r", "feat").unwrap() == Some(root.clone()), "changeset creation does not move refs");
    assert!(e.cas_ref("r", "feat", Some("deadbeef"), Some(&c.id)).is_err());
    e.cas_ref("r", "feat", Some(&root), Some(&c.id)).unwrap();
    e.cas_ref("r", "feat", Some(&c.id), None).unwrap();
    assert_eq!(e.get_ref("r", "feat").unwrap(), None);
}
#[test]
fn concurrent_cas_one_winner() {
    let d = tempfile::tempdir().unwrap();
    let e = Arc::new(Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap());
    let root = e.create_repo("r", "u").unwrap();
    let hs: Vec<_> = (0..8)
        .map(|i| {
            let e = e.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                let b = e.blobs.put(format!("{i}\n").as_bytes()).unwrap();
                e.commit("r", "main", &root, "u", "race", vec![FileChange { path: "f".into(), blob: Some(b), mode: MODE_FILE }]).is_ok()
            })
        })
        .collect();
    let wins = hs.into_iter().map(|h| h.join().unwrap()).filter(|x| *x).count();
    assert_eq!(wins, 1);
}
#[test]
fn wal_records_everything() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    assert_eq!(e.wal_len("r").unwrap(), 2); // root changeset + main ref
    e.commit("r", "main", &root, "u", "one", vec![put(&e, "a", "1\n")]).unwrap();
    assert_eq!(e.wal_len("r").unwrap(), 4);
}
#[test]
fn merge_clean_and_conflict() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let base = e.commit("r", "main", &root, "u", "base", vec![put(&e, "f", "1\n2\n3\n4\n5\n")]).unwrap().id;
    e.cas_ref("r", "feat", None, Some(&base)).unwrap();
    e.commit("r", "feat", &base, "u", "f", vec![put(&e, "f", "1\n2\n3\n4\nFIVE\n"), put(&e, "g", "new\n")]).unwrap();
    let m1 = e.commit("r", "main", &base, "u", "m", vec![put(&e, "f", "ONE\n2\n3\n4\n5\n")]).unwrap().id;
    let mc = e.merge("r", "main", "feat", "u").unwrap();
    assert_eq!(mc.parents.len(), 2);
    assert_eq!(mc.parents[0], m1);
    assert_eq!(read(&e, &mc.id, "f"), "ONE\n2\n3\n4\nFIVE\n");
    assert_eq!(read(&e, &mc.id, "g"), "new\n");
    // conflict
    let h = e.get_ref("r", "main").unwrap().unwrap();
    e.cas_ref("r", "x", None, Some(&h)).unwrap();
    e.commit("r", "x", &h, "u", "x", vec![put(&e, "g", "xx\n")]).unwrap();
    e.commit("r", "main", &h, "u", "y", vec![put(&e, "g", "yy\n")]).unwrap();
    assert!(matches!(e.merge("r", "main", "x", "u"), Err(Error::Conflict(p)) if p == vec!["g".to_string()]));
}
#[test]
fn rebase_replays() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let base = e.commit("r", "main", &root, "u", "base", vec![put(&e, "f", "a\nb\nc\n")]).unwrap().id;
    e.cas_ref("r", "feat", None, Some(&base)).unwrap();
    let f1 = e.commit("r", "feat", &base, "u", "f1", vec![put(&e, "f", "a\nb\nC\n")]).unwrap().id;
    e.commit("r", "feat", &f1, "u", "f2", vec![put(&e, "h", "h\n")]).unwrap();
    let m = e.commit("r", "main", &base, "u", "m", vec![put(&e, "f", "A\nb\nc\n")]).unwrap().id;
    let tip = e.rebase("r", "feat", "main").unwrap();
    let log = e.log("r", &tip, None, 10).unwrap();
    assert_eq!(log.iter().map(|c| c.message.as_str()).collect::<Vec<_>>(), vec!["f2", "f1", "m", "base", "repository created"]);
    assert_eq!(read(&e, &tip, "f"), "A\nb\nC\n");
    assert_eq!(e.merge_base("r", &tip, &m).unwrap(), Some(m));
}
#[test]
fn blame_tracks_lines() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "1", vec![put(&e, "f", "a\nb\n")]).unwrap().id;
    let c2 = e.commit("r", "main", &c1, "u", "2", vec![put(&e, "other", "x\n")]).unwrap().id;
    let c3 = e.commit("r", "main", &c2, "u", "3", vec![put(&e, "f", "a\nNEW\nb\n")]).unwrap().id;
    let b = e.blame("r", &c3, "f").unwrap();
    assert_eq!(b.iter().map(|x| x.0.clone()).collect::<Vec<_>>(), vec![c1.clone(), c3.clone(), c1.clone()]);
    // second call hits derived cache, same answer
    assert_eq!(e.blame("r", &c3, "f").unwrap(), b);
}
#[test]
fn log_path_filter_and_diff() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "1", vec![put(&e, "src/a", "a\n")]).unwrap().id;
    let c2 = e.commit("r", "main", &c1, "u", "2", vec![put(&e, "doc/x", "x\n")]).unwrap().id;
    assert_eq!(e.log("r", &c2, Some("src"), 10).unwrap().len(), 1);
    let d = e.diff("r", &c1, &c2, &[]).unwrap();
    assert!(d.contains("+++ b/doc/x") && d.contains("+x"));
    assert_eq!(e.diff("r", &c1, &c2, &["src".into()]).unwrap(), "");
}
#[test]
fn sparse_tree() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c = e.commit("r", "main", &root, "u", "1", vec![put(&e, "a/1", "1"), put(&e, "ab/2", "2"), put(&e, "b/3", "3")]).unwrap().id;
    let t = e.tree("r", &c, &["a".into()]).unwrap();
    assert_eq!(t.keys().cloned().collect::<Vec<_>>(), vec!["a/1"]);
}
#[test]
fn rejects_bad_paths_and_missing_blobs() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    for p in ["../x", "/abs", "a//b", ".cambium/state.json"] {
        assert!(e.commit("r", "main", &root, "u", "x", vec![put(&e, p, "x")]).is_err(), "{p}");
    }
    assert!(e
        .commit("r", "main", &root, "u", "x", vec![FileChange { path: "a".into(), blob: Some("00".repeat(32)), mode: MODE_FILE }])
        .is_err());
}
#[test]
fn tokens() {
    let (e, _d) = eng();
    let t = e.create_token("alice").unwrap();
    assert_eq!(e.verify_token(&t).unwrap(), "alice");
    assert!(e.verify_token("cmbt_nope").is_err());
}
#[test]
fn fs_blob_store_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let s = FsBlobStore::new(d.path()).unwrap();
    let id = s.put(b"hello").unwrap();
    assert_eq!(s.get(&id).unwrap().unwrap(), b"hello");
    assert_eq!(s.put(b"hello").unwrap(), id, "content addressed");
    assert!(s.get("../../etc/passwd").unwrap().is_none());
}
#[test]
fn fs_blobs_are_compressed() {
    let d = tempfile::tempdir().unwrap();
    let s = FsBlobStore::new(d.path()).unwrap();
    let data = "same line\n".repeat(1000);
    let id = s.put(data.as_bytes()).unwrap();
    let on_disk = std::fs::metadata(d.path().join(&id[..2]).join(&id)).unwrap().len();
    assert!(on_disk < 500, "{on_disk}");
    assert_eq!(s.get(&id).unwrap().unwrap(), data.as_bytes());
}

// ---------- file modes ----------
#[test]
fn exec_mode_round_trips_through_commit_and_manifest() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c = e.commit("r", "main", &root, "u", "add script", vec![exec(&e, "run.sh", "echo hi\n"), put(&e, "a.txt", "a\n")]).unwrap();
    let m = e.manifest("r", &c.id).unwrap();
    assert_eq!(split_entry(&m["run.sh"]).1, MODE_EXEC);
    assert_eq!(split_entry(&m["a.txt"]).1, MODE_FILE);
    assert_eq!(e.get_changeset("r", &c.id).unwrap().files.iter().find(|f| f.path == "run.sh").unwrap().mode, MODE_EXEC);
    assert_eq!(read(&e, &c.id, "run.sh"), "echo hi\n");
}
#[test]
fn mode_only_change_shows_in_diff_and_not_as_content() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "one", vec![put(&e, "run.sh", "echo\n")]).unwrap();
    let c2 = e.commit("r", "main", &c1.id, "u", "chmod", vec![exec(&e, "run.sh", "echo\n")]).unwrap();
    let d = e.diff("r", &c1.id, &c2.id, &[]).unwrap();
    assert_eq!(d, "mode run.sh: 100644 -> 100755\n");
}
#[test]
fn merge_keeps_mode_change_from_one_side_and_content_from_other() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "base", vec![put(&e, "run.sh", "a\nb\n")]).unwrap();
    e.cas_ref("r", "feat", None, Some(&c1.id)).unwrap();
    let f = e.commit("r", "feat", &c1.id, "u", "chmod", vec![exec(&e, "run.sh", "a\nb\n")]).unwrap();
    e.commit("r", "main", &c1.id, "u", "edit", vec![put(&e, "run.sh", "a\nb\nc\n")]).unwrap();
    let m = e.merge("r", "main", &f.id, "u").unwrap();
    let man = e.manifest("r", &m.id).unwrap();
    assert_eq!(split_entry(&man["run.sh"]).1, MODE_EXEC);
    assert_eq!(read(&e, &m.id, "run.sh"), "a\nb\nc\n");
}
#[test]
fn rebase_preserves_exec_bit() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    e.cas_ref("r", "feat", None, Some(&root)).unwrap();
    e.commit("r", "feat", &root, "u", "script", vec![exec(&e, "t.sh", "x\n")]).unwrap();
    e.commit("r", "main", &root, "u", "other", vec![put(&e, "o.txt", "o\n")]).unwrap();
    let tip = e.rebase("r", "feat", "main").unwrap();
    assert_eq!(split_entry(&e.manifest("r", &tip).unwrap()["t.sh"]).1, MODE_EXEC);
}
#[test]
fn unsupported_mode_rejected() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let bad = FileChange { mode: 0o040000, ..put(&e, "l", "x") };
    assert!(matches!(e.commit("r", "main", &root, "u", "link", vec![bad]), Err(cambium_core::Error::Bad(_))));
}
#[test]
fn symlink_entry_stored_and_retargeting_diffs() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "link", vec![FileChange { mode: MODE_LINK, ..put(&e, "cur", "v1") }]).unwrap();
    assert_eq!(split_entry(&e.manifest("r", &c1.id).unwrap()["cur"]).1, MODE_LINK);
    let c2 = e.commit("r", "main", &c1.id, "u", "retarget", vec![FileChange { mode: MODE_LINK, ..put(&e, "cur", "v2") }]).unwrap();
    let d = e.diff("r", &c1.id, &c2.id, &[]).unwrap();
    assert!(d.contains("-v1") && d.contains("+v2"));
    let c3 = e.commit("r", "main", &c2.id, "u", "to file", vec![put(&e, "cur", "v2")]).unwrap();
    assert!(e.diff("r", &c2.id, &c3.id, &[]).unwrap().starts_with("mode cur: 120000 -> 100644"));
}

// ---------- per-directory permissions ----------
use cambium_core::{Perm, Principal};
fn who(u: &str, g: &[&str]) -> Principal {
    Principal { user: u.into(), groups: g.iter().map(|s| s.to_string()).collect() }
}
#[test]
fn open_repo_until_owner_sets_first_rule() {
    let (e, _d) = eng();
    e.create_repo("r", "owner").unwrap();
    assert!(e.authz("r", &who("anyone", &[])).unwrap().can("x/y", Perm::Write));
    assert!(e.set_acl("r", &who("mallory", &[]), "", "user:mallory", Some(Perm::Admin)).is_err(), "only the owner may lock an open repo");
    e.set_acl("r", &who("owner", &[]), "", "user:owner", Some(Perm::Admin)).unwrap();
    assert!(!e.authz("r", &who("anyone", &[])).unwrap().can("x", Perm::Read));
}
#[test]
fn longest_prefix_wins_and_groups_apply() {
    let (e, _d) = eng();
    e.create_repo("r", "o").unwrap();
    let o = who("o", &[]);
    e.set_acl("r", &o, "", "user:o", Some(Perm::Admin)).unwrap();
    e.set_acl("r", &o, "", "group:eng", Some(Perm::Read)).unwrap();
    e.set_acl("r", &o, "services/pay", "group:pay", Some(Perm::Write)).unwrap();
    e.set_acl("r", &o, "secret/", "group:eng", Some(Perm::None)).unwrap();
    let dev = who("dev", &["eng"]);
    let az = e.authz("r", &dev).unwrap();
    assert_eq!(az.perm("README"), Perm::Read);
    assert_eq!(az.perm("secret/keys.txt"), Perm::None);
    assert_eq!(az.perm("services/pay/x.rs"), Perm::Read, "pay rule does not name dev");
    let payer = e.authz("r", &who("p", &["eng", "pay"])).unwrap();
    assert_eq!(payer.perm("services/pay/x.rs"), Perm::Write);
    assert_eq!(payer.perm("services/payments/x.rs"), Perm::Read, "prefix matches whole segments only");
    assert!(e.set_acl("r", &dev, "", "user:dev", Some(Perm::Admin)).is_err());
}
#[test]
fn filter_hides_unreadable_paths_and_blobs() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "o").unwrap();
    let c = e.commit("r", "main", &root, "o", "two dirs", vec![put(&e, "pub/a", "public\n"), put(&e, "priv/b", "private\n")]).unwrap();
    let o = who("o", &[]);
    e.set_acl("r", &o, "", "user:o", Some(Perm::Admin)).unwrap();
    e.set_acl("r", &o, "pub", "*", Some(Perm::Read)).unwrap();
    let guest = who("g", &[]);
    let m = e.authz("r", &guest).unwrap().filter(e.manifest("r", &c.id).unwrap());
    assert_eq!(m.keys().collect::<Vec<_>>(), vec!["pub/a"]);
    let priv_blob = split_entry(&e.manifest("r", &c.id).unwrap()["priv/b"]).0.to_string();
    assert!(!e.blob_readable(&priv_blob, &guest).unwrap());
    assert!(e.blob_readable(&priv_blob, &o).unwrap());
}
#[test]
fn merge_checked_blocks_writes_outside_grant() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "o").unwrap();
    e.cas_ref("r", "feat", None, Some(&root)).unwrap();
    let f = e.commit("r", "feat", &root, "o", "f", vec![put(&e, "locked/x", "x\n")]).unwrap();
    let res = e.merge_checked("r", "main", &f.id, "o", &|fs| {
        if fs.iter().any(|f| f.path.starts_with("locked/")) {
            Err(cambium_core::Error::Forbidden("locked".into()))
        } else {
            Ok(())
        }
    });
    assert!(matches!(res, Err(cambium_core::Error::Forbidden(_))));
    assert_eq!(e.get_ref("r", "main").unwrap().unwrap(), root, "nothing moved");
}

// ---------- fast ancestry & path history ----------
fn cs(e: &Engine, parents: &[&str], f: &str) -> String {
    e.create_changeset("r", parents.iter().map(|s| s.to_string()).collect(), "u", f, vec![put(e, f, f)]).unwrap().id
}
#[test]
fn generation_numbers() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let a = cs(&e, &[&root], "a");
    let b = cs(&e, &[&a], "b");
    let c = cs(&e, &[&root], "c");
    let m = cs(&e, &[&b, &c], "m");
    assert_eq!((e.gen("r", &root).unwrap(), e.gen("r", &b).unwrap(), e.gen("r", &c).unwrap(), e.gen("r", &m).unwrap()), (1, 3, 2, 4));
}
#[test]
fn ancestry_with_generation_pruning() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let mut tip = root.clone();
    for i in 0..200 {
        tip = cs(&e, &[&tip], &format!("f{i}"));
    }
    let side = cs(&e, &[&root], "side");
    assert!(e.is_ancestor("r", &root, &tip).unwrap());
    assert!(!e.is_ancestor("r", &side, &tip).unwrap(), "side branch is not an ancestor");
    assert!(!e.is_ancestor("r", &tip, &root).unwrap());
    assert!(e.is_ancestor("r", &tip, &tip).unwrap());
    assert_eq!(e.merge_base("r", &tip, &side).unwrap(), Some(root.clone()));
}
#[test]
fn merge_base_criss_cross_picks_highest_generation() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let a1 = cs(&e, &[&root], "a1");
    let b1 = cs(&e, &[&root], "b1");
    let a2 = cs(&e, &[&a1, &b1], "a2");
    let b2 = cs(&e, &[&b1, &a1], "b2");
    let mb = e.merge_base("r", &a2, &b2).unwrap().unwrap();
    assert!(mb == a1 || mb == b1, "a best common ancestor, not the root");
    let x = cs(&e, &[&a2], "x");
    assert_eq!(e.merge_base("r", &x, &a2).unwrap(), Some(a2.clone()));
}
#[test]
fn path_history_index_answers_directory_log() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let mut tip = root.clone();
    for i in 0..50 {
        let p = if i % 10 == 0 { format!("deep/dir/f{i}") } else { format!("other/f{i}") };
        tip = e.create_changeset("r", vec![tip], "u", &p, vec![put(&e, &p, "x")]).unwrap().id;
    }
    let l = e.log("r", &tip, Some("deep"), 100).unwrap();
    assert_eq!(l.len(), 5);
    assert_eq!(e.log("r", &tip, Some("deep/dir/f40"), 100).unwrap().len(), 1);
    assert_eq!(e.log("r", &tip, Some("deep/"), 2).unwrap().len(), 2, "limit respected");
    assert!(l.windows(2).all(|w| w[0].seq > w[1].seq), "newest first");
    let n: i64 = e.db.one("SELECT count(*) FROM path_history WHERE dir='deep'", vec![]).unwrap().unwrap().i(0);
    assert_eq!(n, 5);
}
#[test]
fn legacy_database_is_backfilled() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("old.db");
    {
        // a database as written before generations / path history existed
        let e = Engine::open(path.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
        let root = e.create_repo("r", "u").unwrap();
        let a = cs(&e, &[&root], "x/a");
        cs(&e, &[&a], "x/b");
    }
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("DROP TABLE path_history; ALTER TABLE changesets DROP COLUMN gen;").unwrap();
    drop(c);
    let e = Engine::open(path.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let tip = e.get_ref("r", "main").unwrap().unwrap();
    let all: Vec<i64> = e.db.query("SELECT gen FROM changesets ORDER BY seq", vec![]).unwrap().iter().map(|r| r.i(0)).collect();
    assert_eq!(all, vec![1, 2, 3]);
    let _ = tip;
    let last = e.db.one("SELECT id FROM changesets ORDER BY seq DESC LIMIT 1", vec![]).unwrap().unwrap().s(0);
    assert_eq!(e.log("r", &last, Some("x"), 10).unwrap().len(), 2);
}

// ---------- reviews & approval rules ----------
#[test]
fn review_needs_n_approvals_from_others_on_current_head() {
    use cambium_core::Principal;
    let (e, _d) = eng();
    let root = e.create_repo("r", "alice").unwrap();
    let alice = Principal::new("alice");
    e.set_protected("r", &alice, "main", 2).unwrap();
    assert!(e.set_protected("r", &Principal::new("mallory"), "main", 0).is_err(), "non-owner cannot lift protection");
    // work is a draft changeset (no branch); the review points at it
    let c1 = e.create_changeset("r", vec![root.clone()], "alice", "f1", vec![put(&e, "a.txt", "1\n")]).unwrap();
    let id = e.create_review("r", &c1.id, "main", "add a", "alice").unwrap();
    assert!(matches!(e.approve("r", id, "alice"), Err(Error::Forbidden(_))), "no self-approval");
    e.approve("r", id, "bob").unwrap();
    assert!(matches!(e.merge_review("r", id, "alice", &|_| Ok(())), Err(Error::Forbidden(_))), "1 of 2");
    // a revised changeset makes earlier approvals stale
    let c2 = e.create_changeset("r", vec![c1.id.clone()], "alice", "f2", vec![put(&e, "b.txt", "2\n")]).unwrap();
    assert!(e.update_review("r", id, "bob", &c2.id).is_err(), "only the author updates");
    e.update_review("r", id, "alice", &c2.id).unwrap();
    e.approve("r", id, "carol").unwrap();
    assert_eq!(e.review("r", id).unwrap().approvals, vec!["carol".to_string()]);
    e.approve("r", id, "bob").unwrap();
    let rv = e.review("r", id).unwrap();
    assert_eq!((rv.approvals.len(), rv.required), (2, 2));
    e.comment("r", id, "bob", Some("a.txt"), "looks good").unwrap();
    assert_eq!(e.comments("r", id).unwrap().len(), 1);
    // main moved meanwhile: landing rebases the reviewed stack onto it
    let m0 = e.commit("r", "main", &root, "alice", "other", vec![put(&e, "c.txt", "3\n")]);
    assert!(m0.is_ok());
    let m = e.merge_review("r", id, "alice", &|_| Ok(())).unwrap();
    assert_eq!(e.get_ref("r", "main").unwrap(), Some(m.id.clone()));
    assert_eq!(e.manifest("r", &m.id).unwrap().len(), 3);
    assert_eq!(e.review("r", id).unwrap().state, "merged");
    assert!(e.merge_review("r", id, "alice", &|_| Ok(())).is_err(), "cannot merge twice");
    assert!(e.approve("r", id, "dave").is_err());
}
#[test]
fn review_validation() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "alice").unwrap();
    assert!(e.create_review("r", "main", "main", "t", "alice").is_err(), "already in main");
    assert!(e.create_review("r", "nope", "main", "t", "alice").is_err());
    let d = e.create_changeset("r", vec![root.clone()], "alice", "d", vec![put(&e, "x", "1\n")]).unwrap().id;
    assert!(e.create_review("r", &d, "main", "   ", "alice").is_err());
    let id = e.create_review("r", &d, "main", "t", "alice").unwrap();
    assert!(e.close_review("r", id, "bob").is_err());
    e.close_review("r", id, "alice").unwrap();
    assert_eq!(e.reviews("r", Some("closed")).unwrap().len(), 1);
    assert!(e.comment("r", id, "bob", None, "").is_err());
    // unprotected target: zero approvals needed
    let id2 = e.create_review("r", &d, "main", "t2", "alice").unwrap();
    assert_eq!(e.review("r", id2).unwrap().required, 0);
}
#[test]
fn land_rebases_onto_moved_head_without_a_lock() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let draft = e.create_changeset("r", vec![root.clone()], "u", "mine", vec![put(&e, "a", "1\n")]).unwrap().id;
    // someone else lands first
    let other = e.commit("r", "main", &root, "v", "theirs", vec![put(&e, "b", "1\n")]).unwrap().id;
    let tip = e.land("r", &draft, "main", &|_| Ok(())).unwrap();
    assert_eq!(e.get_changeset("r", &tip).unwrap().parents, vec![other.clone()]);
    assert_eq!(e.manifest("r", &tip).unwrap().len(), 2);
    // landing again is a no-op
    assert_eq!(e.land("r", &draft, "main", &|_| Ok(())).unwrap(), tip);
    assert!(cambium_core::engine::valid_ref_name("main") && cambium_core::engine::valid_ref_name("release/2026.11"));
    assert!(
        !cambium_core::engine::valid_ref_name("feature/x")
            && !cambium_core::engine::valid_ref_name("release/")
            && !cambium_core::engine::valid_ref_name("release/../x")
    );
}

#[test]
fn diff_detects_renames_and_copies() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "1", vec![put(&e, "old.txt", "same\n"), put(&e, "keep.txt", "k\n")]).unwrap();
    let c2 = e
        .commit(
            "r",
            "main",
            &c1.id,
            "u",
            "2",
            vec![
                FileChange { path: "old.txt".into(), blob: None, mode: MODE_FILE },
                put(&e, "new.txt", "same\n"),
                put(&e, "dup.txt", "k\n"),
            ],
        )
        .unwrap();
    let d = e.diff("r", &c1.id, &c2.id, &[]).unwrap();
    assert!(d.contains("rename old.txt -> new.txt"), "{d}");
    assert!(d.contains("copy keep.txt -> dup.txt"), "{d}");
    assert!(!d.contains("+same"), "no content diff for a pure rename: {d}");
}

#[test]
fn search_index_finds_exact_matches_and_is_shared() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e
        .commit(
            "r",
            "main",
            &root,
            "u",
            "1",
            vec![
                put(&e, "src/a.rs", "fn open_engine() {}\nlet x = 100%;\n"),
                put(&e, "b.txt", "nothing here\n"),
                put(&e, "c.txt", "MyEngineFactory builds things\n"),
            ],
        )
        .unwrap();
    let m = e.manifest("r", &c1.id).unwrap();
    let h = e.search(&m, "open_engine", 50).unwrap();
    assert_eq!(h, vec![cambium_core::search::Hit { path: "src/a.rs".into(), line: 1, text: "fn open_engine() {}".into() }]);
    let h = e.search(&m, "engine", 50).unwrap();
    assert_eq!(h.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["c.txt", "src/a.rs"], "partial words match, case-insensitive");
    assert_eq!(e.search(&m, "100%", 50).unwrap().len(), 1, "punctuation is literal");
    assert!(e.search(&m, "builds nothing", 50).unwrap().is_empty());
    let h = e.search(&m, "src/a", 50).unwrap();
    assert_eq!((h[0].path.as_str(), h[0].line), ("src/a.rs", 0), "path matches come first");
    // a second revision only indexes its new blob
    let c2 = e.commit("r", "main", &c1.id, "u", "2", vec![put(&e, "d.txt", "open_engine again\n")]).unwrap();
    assert_eq!(e.index_blobs(&e.manifest("r", &c2.id).unwrap()).unwrap(), 1);
    assert_eq!(e.search(&e.manifest("r", &c2.id).unwrap(), "OPEN_ENGINE", 1).unwrap().len(), 1, "limit");
}

#[test]
fn file_and_directory_at_same_path_rejected() {
    // A symlink `a` plus a file `a/x` would let a client write through the link.
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let link = FileChange { mode: MODE_LINK, ..put(&e, "a", "/home/victim/.ssh") };
    let r = e.commit("r", "main", &root, "u", "evil", vec![link.clone(), put(&e, "a/authorized_keys", "key")]);
    assert!(matches!(r, Err(Error::Bad(_))), "same changeset: {r:?}");
    // split over two changesets
    let c1 = e.commit("r", "main", &root, "u", "link", vec![link]).unwrap();
    assert!(e.commit("r", "main", &c1.id, "u", "under link", vec![put(&e, "a/authorized_keys", "key")]).is_err());
    // and the other way round: a file where a folder already is
    let c2 = e.commit("r", "main", &c1.id, "u", "dir", vec![put(&e, "d/x", "1")]).unwrap();
    assert!(e.commit("r", "main", &c2.id, "u", "file over dir", vec![put(&e, "d", "2")]).is_err());
}
#[test]
fn replacing_a_link_with_a_folder_is_allowed() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 = e.commit("r", "main", &root, "u", "link", vec![FileChange { mode: MODE_LINK, ..put(&e, "a", "t") }]).unwrap();
    let del = FileChange { path: "a".into(), blob: None, mode: MODE_FILE };
    let c2 = e.commit("r", "main", &c1.id, "u", "folder", vec![del, put(&e, "a/x", "1")]).unwrap();
    assert_eq!(e.manifest("r", &c2.id).unwrap().keys().collect::<Vec<_>>(), vec!["a/x"]);
}
#[test]
fn more_than_two_parents_rejected() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let r = e.create_changeset("r", vec![root.clone(), root.clone(), root], "u", "octopus", vec![]);
    assert!(r.is_err());
}

#[test]
fn fs_blob_store_concurrent_puts_and_integrity() {
    let d = tempfile::tempdir().unwrap();
    let s = Arc::new(FsBlobStore::new(d.path()).unwrap());
    let data = vec![7u8; 1 << 20];
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let (s, data) = (s.clone(), data.clone());
            std::thread::spawn(move || s.put(&data).unwrap())
        })
        .collect();
    let ids: Vec<String> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(s.get(&ids[0]).unwrap().unwrap(), data, "object complete after racing uploads");
    assert!(s.has(&ids[0]).unwrap() && !s.has(&"0".repeat(64)).unwrap());
    // no temp files left behind
    let sub = d.path().join(&ids[0][..2]);
    assert_eq!(std::fs::read_dir(&sub).unwrap().count(), 1);
    // a corrupted object is refused, moved aside, and healed by the next upload
    std::fs::write(sub.join(&ids[0]), b"garbage").unwrap();
    assert!(s.get(&ids[0]).is_err());
    assert!(!s.has(&ids[0]).unwrap());
    s.put(&data).unwrap();
    assert_eq!(s.get(&ids[0]).unwrap().unwrap(), data);
}

#[test]
fn binary_files_conflict_instead_of_lossy_merge() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    // non-overlapping edits that a line merge would happily combine
    let bin = |tail: &[u8]| {
        let mut v = b"\x89PNG\x00\xff\xfe\n".to_vec();
        v.extend_from_slice(b"a\nb\nc\nd\ne\n");
        v.extend_from_slice(tail);
        FileChange { path: "img.png".into(), blob: Some(e.blobs.put(&v).unwrap()), mode: MODE_FILE }
    };
    let base = e.commit("r", "main", &root, "u", "base", vec![bin(b"z\n")]).unwrap().id;
    e.cas_ref("r", "feat", None, Some(&base)).unwrap();
    let mut first = b"\x89PNG\x00\xff\xfe\n".to_vec();
    first.extend_from_slice(b"A\nb\nc\nd\ne\nz\n");
    let fc = FileChange { path: "img.png".into(), blob: Some(e.blobs.put(&first).unwrap()), mode: MODE_FILE };
    e.commit("r", "feat", &base, "u", "edit head", vec![fc]).unwrap();
    e.commit("r", "main", &base, "u", "edit tail", vec![bin(b"Z\n")]).unwrap();
    assert!(matches!(e.merge("r", "main", "feat", "u"), Err(Error::Conflict(p)) if p == vec!["img.png".to_string()]));
    // non-UTF-8 "text" too
    let h = e.get_ref("r", "main").unwrap().unwrap();
    let lat = |s: &[u8]| FileChange { path: "l1.txt".into(), blob: Some(e.blobs.put(s).unwrap()), mode: MODE_FILE };
    let b2 = e.commit("r", "main", &h, "u", "latin1", vec![lat(b"caf\xe9\n1\n2\n3\n")]).unwrap().id;
    e.cas_ref("r", "x", None, Some(&b2)).unwrap();
    e.commit("r", "x", &b2, "u", "x", vec![lat(b"caf\xe9\n1\n2\nTHREE\n")]).unwrap();
    e.commit("r", "main", &b2, "u", "y", vec![lat(b"CAF\xe9\n1\n2\n3\n")]).unwrap();
    assert!(matches!(e.merge("r", "main", "x", "u"), Err(Error::Conflict(_))));
}
#[test]
fn symlink_targets_conflict_instead_of_merging() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let link = |t: &str| FileChange { mode: MODE_LINK, ..put(&e, "l", t) };
    let base = e.commit("r", "main", &root, "u", "base", vec![link("a\nb\nc\n")]).unwrap().id;
    e.cas_ref("r", "feat", None, Some(&base)).unwrap();
    e.commit("r", "feat", &base, "u", "f", vec![link("A\nb\nc\n")]).unwrap();
    e.commit("r", "main", &base, "u", "m", vec![link("a\nb\nC\n")]).unwrap();
    assert!(matches!(e.merge("r", "main", "feat", "u"), Err(Error::Conflict(_))));
}

#[test]
fn rebase_when_already_on_top_keeps_head() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let m = e.commit("r", "main", &root, "u", "m", vec![put(&e, "a", "1\n")]).unwrap().id;
    e.cas_ref("r", "feat", None, Some(&m)).unwrap();
    let f = e.commit("r", "feat", &m, "u", "f", vec![put(&e, "b", "1\n")]).unwrap().id;
    assert_eq!(e.rebase("r", "feat", "main").unwrap(), f, "no new changesets when nothing to replay");
    assert_eq!(e.get_ref("r", "feat").unwrap(), Some(f));
}
#[test]
fn reopening_does_not_rewrite_indexes() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("t.db");
    {
        let e = Engine::open(path.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
        let root = e.create_repo("r", "u").unwrap();
        e.commit("r", "main", &root, "u", "one", vec![put(&e, "a", "1\n")]).unwrap();
    }
    // corrupt a generation number; a reopen must not "fix" it (proves no full rewrite)
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute("UPDATE changesets SET gen=42", []).unwrap();
    drop(c);
    let _e = Engine::open(path.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let c = rusqlite::Connection::open(&path).unwrap();
    let g: i64 = c.query_row("SELECT max(gen) FROM changesets", [], |r| r.get(0)).unwrap();
    assert_eq!(g, 42);
}

#[test]
fn trees_share_unchanged_directories_and_verify() {
    let (e, _d) = eng();
    let root = e.create_repo("r", "u").unwrap();
    let c1 =
        e.commit("r", "main", &root, "u", "one", vec![put(&e, "a/x", "1\n"), put(&e, "b/y", "2\n"), put(&e, "top", "t\n")]).unwrap().id;
    let c2 = e.commit("r", "main", &c1, "u", "two", vec![put(&e, "a/x", "changed\n")]).unwrap().id;
    let (t1, t2) = (e.get_tree(&e.root_tree("r", &c1).unwrap()).unwrap(), e.get_tree(&e.root_tree("r", &c2).unwrap()).unwrap());
    let id_of = |t: &cambium_core::tree::Tree, n: &str| t.entries.iter().find(|x| x.name == n).unwrap().id.clone();
    assert_ne!(id_of(&t1, "a"), id_of(&t2, "a"), "changed directory gets a new tree");
    assert_eq!(id_of(&t1, "b"), id_of(&t2, "b"), "unchanged directory is the same object");
    // ids are verifiable from content
    let rid = e.root_tree("r", &c2).unwrap();
    assert_eq!(e.get_tree(&rid).unwrap().id(), rid);
    assert!(e.tree_at("r", &id_of(&t2, "b"), "b").unwrap() && !e.tree_at("r", &id_of(&t2, "b"), "a").unwrap());
    // file sizes are in the tree
    let a = e.get_tree(&id_of(&t2, "a")).unwrap();
    assert_eq!(a.entries[0].size, 8);
    // emptied directories disappear
    let c3 = e.commit("r", "main", &c2, "u", "rm", vec![FileChange { path: "b/y".into(), blob: None, mode: MODE_FILE }]).unwrap().id;
    let t3 = e.get_tree(&e.root_tree("r", &c3).unwrap()).unwrap();
    assert!(!t3.entries.iter().any(|x| x.name == "b"));
    assert_eq!(e.manifest("r", &c3).unwrap().keys().collect::<Vec<_>>(), vec!["a/x", "top"]);
}
