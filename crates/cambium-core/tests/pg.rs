//! Multi-node: two engines (two server nodes) sharing one Postgres database and
//! one blob store. Needs a reachable Postgres in CAMBIUM_TEST_PG (tests/multinode.sh
//! starts one in a container); skipped with a notice otherwise.
use cambium_core::blob::{BlobStore, FsBlobStore};
use cambium_core::{Engine, Error, FileChange, Perm, Principal};
use std::sync::Arc;

fn nodes() -> Option<(Arc<Engine>, Arc<Engine>, String, tempfile::TempDir)> {
    let Ok(url) = std::env::var("CAMBIUM_TEST_PG") else {
        eprintln!("CAMBIUM_TEST_PG unset: Postgres tests skipped");
        return None;
    };
    // many test engines share one server: keep each pool small (max_connections is 100)
    std::env::set_var("CAMBIUM_PG_POOL", "2");
    let d = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(d.path()).unwrap());
    let a = Arc::new(Engine::open_pg(&url, blobs.clone()).unwrap());
    let b = Arc::new(Engine::open_pg(&url, blobs).unwrap());
    let repo = format!("r{}", rand_suffix());
    Some((a, b, repo, d))
}
fn rand_suffix() -> String {
    format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}
fn fc(p: &str, b: &str) -> FileChange {
    FileChange { path: p.into(), blob: Some(b.into()), mode: 0o100644 }
}

#[test]
#[ignore = "needs Postgres: run tests/multinode.sh"]
fn pg_commit_on_one_node_is_visible_on_the_other() {
    let Some((a, b, repo, _d)) = nodes() else { return };
    let root = a.create_repo(&repo, "alice").unwrap();
    let x = a.blobs.put(b"x\n").unwrap();
    let c1 = a.commit(&repo, "main", &root, "alice", "one", vec![fc("src/x.txt", &x)]).unwrap();
    assert_eq!(b.get_ref(&repo, "main").unwrap().unwrap(), c1.id);
    assert_eq!(b.manifest(&repo, &c1.id).unwrap().len(), 1);
    let y = b.blobs.put(b"y\n").unwrap();
    let c2 = b.commit(&repo, "main", &c1.id, "bob", "two", vec![fc("docs/y.txt", &y)]).unwrap();
    assert_eq!(a.log(&repo, &c2.id, None, 10).unwrap().len(), 3);
    assert_eq!(a.log(&repo, &c2.id, Some("src"), 10).unwrap().len(), 1);
    assert_eq!(a.gen(&repo, &c2.id).unwrap(), 3);
    assert!(b.is_ancestor(&repo, &c1.id, &c2.id).unwrap());
    // stale write from node A loses
    assert!(matches!(a.commit(&repo, "main", &c1.id, "alice", "stale", vec![fc("z", &x)]), Err(Error::RefMoved { .. })));
}

#[test]
#[ignore = "needs Postgres: run tests/multinode.sh"]
fn pg_cas_has_exactly_one_winner_across_nodes() {
    let Some((a, b, repo, _d)) = nodes() else { return };
    let root = a.create_repo(&repo, "alice").unwrap();
    let mut cands = vec![];
    for i in 0..16 {
        let bl = a.blobs.put(format!("{i}").as_bytes()).unwrap();
        cands.push(a.create_changeset(&repo, vec![root.clone()], "u", "race", vec![fc("f", &bl)]).unwrap().id);
    }
    let hs: Vec<_> = cands
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let e = if i % 2 == 0 { a.clone() } else { b.clone() };
            let (repo, root) = (repo.clone(), root.clone());
            std::thread::spawn(move || e.cas_ref(&repo, "main", Some(&root), Some(&c)).is_ok())
        })
        .collect();
    let wins = hs.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count();
    assert_eq!(wins, 1);
    let (_, _, version) = b.list_refs(&repo).unwrap().into_iter().find(|r| r.0 == "main").unwrap();
    assert_eq!(version, 2);
    // creating the same new ref on both nodes: one wins
    let c = a.get_ref(&repo, "main").unwrap().unwrap();
    let r1 = a.cas_ref(&repo, "feat", None, Some(&c));
    let r2 = b.cas_ref(&repo, "feat", None, Some(&c));
    assert!(r1.is_ok() ^ r2.is_ok());
}

#[test]
#[ignore = "needs Postgres: run tests/multinode.sh"]
fn pg_acl_and_merge_shared() {
    let Some((a, b, repo, _d)) = nodes() else { return };
    let root = a.create_repo(&repo, "alice").unwrap();
    let alice = Principal::new("alice");
    a.set_acl(&repo, &alice, "", "user:alice", Some(Perm::Admin)).unwrap();
    a.set_acl(&repo, &alice, "secret", "*", Some(Perm::None)).unwrap();
    assert!(!b.authz(&repo, &Principal::new("bob")).unwrap().can("secret/k", Perm::Read));
    let x = a.blobs.put(b"base\n").unwrap();
    let c1 = a.commit(&repo, "main", &root, "alice", "base", vec![fc("f", &x)]).unwrap();
    b.cas_ref(&repo, "feat", None, Some(&c1.id)).unwrap();
    let y = b.blobs.put(b"feat\n").unwrap();
    b.commit(&repo, "feat", &c1.id, "bob", "feat", vec![fc("g", &y)]).unwrap();
    let z = a.blobs.put(b"main\n").unwrap();
    a.commit(&repo, "main", &c1.id, "alice", "main", vec![fc("h", &z)]).unwrap();
    let m = b.merge(&repo, "main", "feat", "bob").unwrap();
    assert_eq!(a.manifest(&repo, &m.id).unwrap().len(), 3);
    let feat = b.get_ref(&repo, "feat").unwrap().unwrap();
    assert_eq!(a.merge_base(&repo, &m.id, &feat).unwrap(), Some(feat.clone()));
    assert!(a.is_ancestor(&repo, &feat, &m.id).unwrap());
}

#[test]
#[ignore = "needs Postgres: run tests/multinode.sh"]
fn pg_audit_chain_from_two_nodes_concurrently_stays_linear() {
    // regression: the advisory-lock query returns `void`, which used to panic the pool worker
    let Some((a, b, _repo, _d)) = nodes() else { return };
    let (n0, _) = a.verify_audit().unwrap();
    let hs: Vec<_> = (0..8)
        .map(|i| {
            let e = if i % 2 == 0 { a.clone() } else { b.clone() };
            std::thread::spawn(move || {
                for _ in 0..10 {
                    e.audit(cambium_core::audit::AuditEvent {
                        actor: "t".into(),
                        action: "test.event".into(),
                        outcome: "ok".into(),
                        ..Default::default()
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let (n, bad) = b.verify_audit().unwrap();
    assert_eq!((n - n0, bad), (80, None));
    assert!(a.db.exec("UPDATE audit_log SET actor='x'", vec![]).is_err());
    assert!(a.db.exec("TRUNCATE audit_log", vec![]).is_err());
    assert_eq!(a.db.pool_stats().unwrap().panics.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
#[ignore = "needs Postgres: run tests/multinode.sh"]
fn pg_tokens_sessions_and_migrations() {
    let Some((a, b, _repo, _d)) = nodes() else { return };
    let u = format!("u{}", rand_suffix());
    let t = a.create_token_full(&u, &["g".into()], Some(60), "l").unwrap();
    assert_eq!(b.verify_principal(&t).unwrap().groups, vec!["g".to_string()]);
    let n = a.rotate_token(&u, None, 0).unwrap();
    assert!(b.verify_principal(&t).is_err() && b.verify_principal(&n).is_ok());
    let s = a.create_session(&Principal::new(&u), 60, "jwt-or-other", None).unwrap();
    assert_eq!(b.session_principal(&s).unwrap().user, u);
    b.end_session(&s).unwrap();
    assert!(a.session_principal(&s).is_err());
    let v = a.db.query("SELECT max(version) FROM schema_migrations", vec![]).unwrap();
    assert_eq!(v[0].i(0), cambium_core::migrate::latest());
}

#[test]
#[ignore = "needs Postgres: run tests/multinode.sh"]
fn pg_backup_streams_and_restores_into_sqlite() {
    // a database of its own: a backup copies everything, and other tests' blobs are gone
    let Ok(url) = std::env::var("CAMBIUM_TEST_PG") else { return };
    std::env::set_var("CAMBIUM_PG_POOL", "2");
    let name = format!("bk{}", rand_suffix());
    use cambium_core::db::Conn;
    cambium_core::db::pg_connect(&url).unwrap().batch(&format!("CREATE DATABASE {name}")).unwrap();
    let url = format!("{}/{name}", url.rsplit_once('/').unwrap().0);
    let d = tempfile::tempdir().unwrap();
    let a = Engine::open_pg(&url, Arc::new(FsBlobStore::new(d.path().join("b")).unwrap())).unwrap();
    let repo = "r".to_string();
    let root = a.create_repo(&repo, "alice").unwrap();
    // the same content under two paths: copied once
    let x = a.blobs.put(b"same\n").unwrap();
    let mut files: Vec<FileChange> =
        (0..300).map(|i| fc(&format!("many/f{i}.txt"), &a.blobs.put(format!("{i}\n").as_bytes()).unwrap())).collect();
    files.push(fc("one.txt", &x));
    files.push(fc("two.txt", &x));
    let c = a.commit(&repo, "main", &root, "alice", "files", files).unwrap();
    let rep = a.backup(&d.path().join("bk")).unwrap();
    assert!(rep.rows > 300 && rep.blobs >= 301);
    let ids: Vec<_> =
        std::fs::read_dir(d.path().join("bk/blobs")).unwrap().flat_map(|s| std::fs::read_dir(s.unwrap().path()).unwrap()).collect();
    assert_eq!(ids.len() as i64, rep.blobs, "every blob copied exactly once");
    let b2: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(d.path().join("restored")).unwrap());
    let f = Engine::open(d.path().join("r.db").to_str().unwrap(), b2).unwrap();
    let r2 = f.restore(&d.path().join("bk")).unwrap();
    assert_eq!((r2.rows, r2.blobs), (rep.rows, rep.blobs));
    assert_eq!(f.get_ref(&repo, "main").unwrap(), Some(c.id.clone()));
    assert_eq!(f.manifest(&repo, &c.id).unwrap(), a.manifest(&repo, &c.id).unwrap());
}
