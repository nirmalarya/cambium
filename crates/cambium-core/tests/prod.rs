//! Production features: migrations, audit chain, tokens/sessions, code owners, backup/restore.
use cambium_core::audit::AuditEvent;
use cambium_core::blob::{BlobStore, FsBlobStore, MemBlobStore};
use cambium_core::engine::MODE_FILE;
use cambium_core::{Engine, Error, FileChange, Principal};
use std::sync::Arc;

fn eng() -> (Engine, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    (e, d)
}
fn put(e: &Engine, path: &str, s: &str) -> FileChange {
    FileChange { path: path.into(), blob: Some(e.blobs.put(s.as_bytes()).unwrap()), mode: MODE_FILE }
}
fn who(u: &str, g: &[&str]) -> Principal {
    Principal { user: u.into(), groups: g.iter().map(|s| s.to_string()).collect() }
}
fn ev(a: &str) -> AuditEvent {
    AuditEvent { actor: "alice".into(), action: a.into(), outcome: "ok".into(), ..Default::default() }
}

#[test]
fn migrations_recorded_and_newer_db_refused() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("m.db");
    let e = Engine::open(path.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let v: Vec<i64> =
        e.db.query("SELECT version FROM schema_migrations ORDER BY version", vec![]).unwrap().iter().map(|r| r.i(0)).collect();
    assert_eq!(*v.last().unwrap(), cambium_core::migrate::latest());
    assert!(v.contains(&1));
    e.db.exec("INSERT INTO schema_migrations(version,name,applied_at) VALUES(999,'future',0)", vec![]).unwrap();
    drop(e);
    let r = Engine::open(path.to_str().unwrap(), Arc::new(MemBlobStore::default()));
    assert!(r.is_err(), "a database from a newer binary must be refused");
    // reopening an up-to-date db is a no-op
    let d2 = tempfile::tempdir().unwrap();
    let p2 = d2.path().join("n.db");
    drop(Engine::open(p2.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap());
    Engine::open(p2.to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
}

#[test]
fn audit_chain_verifies_and_detects_tampering() {
    let (e, d) = eng();
    for a in ["auth.login", "acl.change", "ref.update"] {
        e.audit(ev(a)).unwrap();
    }
    assert_eq!(e.verify_audit().unwrap(), (3, None));
    let x = e.audit_export(0, 100).unwrap();
    assert_eq!(x.len(), 3);
    assert_eq!(x[1]["action"], "acl.change");
    assert_eq!(x[0]["product"], "cambium");
    assert!(e.db.exec("UPDATE audit_log SET actor='mallory' WHERE seq=2", vec![]).is_err(), "UPDATE blocked by trigger");
    assert!(e.db.exec("DELETE FROM audit_log WHERE seq=2", vec![]).is_err(), "DELETE blocked by trigger");
    // an attacker with raw file access who drops the triggers is still caught by the chain
    let c = rusqlite::Connection::open(d.path().join("t.db")).unwrap();
    let trig: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name='audit_log'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(!trig.is_empty());
    for t in trig {
        c.execute(&format!("DROP TRIGGER {t}"), []).unwrap();
    }
    c.execute("UPDATE audit_log SET actor='mallory' WHERE seq=2", []).unwrap();
    assert_eq!(e.verify_audit().unwrap().1, Some(2));
}

#[test]
fn token_expiry_rotation_revocation() {
    let (e, _d) = eng();
    let t = e.create_token_full("alice", &["devs".into()], Some(3600), "ci").unwrap();
    assert_eq!(e.verify_principal(&t).unwrap().groups, vec!["devs".to_string()]);
    let dead = e.create_token_full("bob", &[], Some(3600), "").unwrap();
    e.db.exec("UPDATE tokens SET expires_at=1 WHERE user_name='bob'", vec![]).unwrap();
    assert!(e.verify_principal(&dead).is_err(), "expired token refused");
    let new = e.rotate_token("alice", Some(3600), 0).unwrap();
    let p = e.verify_principal(&new).unwrap();
    assert_eq!((p.user.as_str(), p.groups.clone()), ("alice", vec!["devs".to_string()]), "rotation keeps groups");
    assert!(e.verify_principal(&t).is_err(), "old token dead with zero grace");
    let g = e.rotate_token("alice", None, 3600).unwrap();
    assert!(e.verify_principal(&new).is_ok(), "grace period keeps old token alive");
    assert!(e.list_tokens().unwrap().iter().all(|v| !v.to_string().contains(&g)), "list never shows secrets");
    assert!(e.revoke_tokens("alice").unwrap() >= 1);
    assert!(e.verify_principal(&g).is_err() && e.verify_principal(&new).is_err());
    assert!(cambium_core::tokens::parse_ttl("bogus").is_err());
}

#[test]
fn sessions_are_server_side_and_end() {
    let (e, _d) = eng();
    let s = e.create_session(&who("alice", &["g"]), 3600, "jwt-or-other", None).unwrap();
    assert!(s.starts_with("cmbs_"));
    assert_eq!(e.session_principal(&s).unwrap().groups, vec!["g".to_string()]);
    assert!(e.verify_principal(&s).is_err(), "a session id is not an API token");
    e.end_session(&s).unwrap();
    assert!(e.session_principal(&s).is_err());
    let s2 = e.create_session(&who("bob", &[]), 1, "jwt-or-other", None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    assert!(e.session_principal(&s2).is_err(), "session expires");
}

#[test]
fn sessions_end_when_access_is_revoked() {
    let (e, _d) = eng();
    // linked to a static token: revoking the token ends the session
    let tok = e.create_token("carol").unwrap();
    let p = e.verify_principal(&tok).unwrap();
    let s = e.create_session(&p, 3600, &tok, None).unwrap();
    assert!(e.session_principal(&s).is_ok());
    e.revoke_tokens("carol").unwrap();
    assert!(e.session_principal(&s).is_err(), "revoked token ends its session");
    // a JWT session is capped at the JWT's exp
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let j = e.create_session(&who("dave", &[]), 3600, "a.b.c", Some(now - 1)).unwrap();
    assert!(e.session_principal(&j).is_err(), "session cannot outlive the JWT");
    // revoking a user ends their sessions even without a linked token
    let j2 = e.create_session(&who("dave", &[]), 3600, "a.b.c", None).unwrap();
    assert!(e.session_principal(&j2).is_ok());
    e.revoke_tokens("dave").unwrap();
    assert!(e.session_principal(&j2).is_err());
}

fn owners_repo() -> (Engine, tempfile::TempDir, String) {
    let (e, d) = eng();
    let root = e.create_repo("r", "alice").unwrap();
    let c = e
        .commit(
            "r",
            "main",
            &root,
            "alice",
            "owners",
            vec![
                put(&e, "OWNERS", "user:root-owner\n"),
                put(&e, "pay/OWNERS", "# payments\ngroup:payments carol\n"),
                put(&e, "pay/a.txt", "1\n"),
                put(&e, "docs/x.md", "x\n"),
            ],
        )
        .unwrap();
    e.cas_ref("r", "feat", None, Some(&c.id)).unwrap();
    (e, d, c.id)
}

#[test]
fn code_owner_group_and_user_rules() {
    let (e, _d, base) = owners_repo();
    e.commit("r", "feat", &base, "alice", "pay change", vec![put(&e, "pay/a.txt", "2\n")]).unwrap();
    let id = e.create_review("r", "feat", "main", "t", "alice").unwrap();
    let rv = e.review("r", id).unwrap();
    assert_eq!(rv.owners_pending.len(), 1, "{:?}", rv.owners_pending);
    assert!(rv.owners_pending[0].starts_with("pay/OWNERS"));
    e.approve_as("r", id, &who("dave", &["other"])).unwrap();
    assert!(matches!(e.merge_review("r", id, "alice", &|_| Ok(())), Err(Error::Forbidden(m)) if m.contains("code-owner")));
    e.approve_as("r", id, &who("erin", &["payments"])).unwrap();
    assert!(e.review("r", id).unwrap().owners_pending.is_empty(), "group rule satisfied");
    e.merge_review("r", id, "alice", &|_| Ok(())).unwrap();
}

#[test]
fn code_owner_author_cannot_satisfy_and_rules_come_from_target() {
    let (e, _d, base) = owners_repo();
    // the source branch rewrites OWNERS to name the author; target's OWNERS still applies
    e.commit("r", "feat", &base, "carol", "sneaky", vec![put(&e, "pay/OWNERS", "carol mallory\n"), put(&e, "pay/a.txt", "3\n")]).unwrap();
    let id = e.create_review("r", "feat", "main", "t", "carol").unwrap();
    assert!(e.approve_as("r", id, &who("carol", &["payments"])).is_err(), "no self-approval");
    e.approve_as("r", id, &who("mallory", &[])).unwrap();
    assert!(!e.review("r", id).unwrap().owners_pending.is_empty(), "source-branch OWNERS ignored");
    e.approve_as("r", id, &who("frank", &["payments"])).unwrap();
    assert!(e.review("r", id).unwrap().owners_pending.is_empty());
    // a root-level change needs the root owner (user rule)
    let (e2, _d2, b2) = owners_repo();
    e2.commit("r", "feat", &b2, "alice", "docs", vec![put(&e2, "docs/x.md", "y\n")]).unwrap();
    let id2 = e2.create_review("r", "feat", "main", "t", "alice").unwrap();
    assert!(e2.review("r", id2).unwrap().owners_pending[0].starts_with("OWNERS"));
    e2.approve("r", id2, "root-owner").unwrap();
    assert!(e2.review("r", id2).unwrap().owners_pending.is_empty());
}

#[test]
fn parse_owners_formats() {
    let o = cambium_core::review::parse_owners("# c\nuser:a, b @c\ngroup:g1 # trailing\n\n");
    assert_eq!(o.users, vec!["a", "b", "c"]);
    assert_eq!(o.groups, vec!["g1"]);
}

#[test]
fn backup_restore_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(d.path().join("b1")).unwrap());
    let e = Engine::open(d.path().join("a.db").to_str().unwrap(), blobs).unwrap();
    let root = e.create_repo("r", "alice").unwrap();
    let c = e.commit("r", "main", &root, "alice", "one", vec![put(&e, "a.txt", "hello\n"), put(&e, "b/c.bin", "\u{0}\u{1}")]).unwrap();
    e.set_acl("r", &Principal::new("alice"), "b", "user:bob", Some(cambium_core::engine::Perm::Read)).unwrap();
    e.create_token("alice").unwrap();
    e.audit(ev("admin.backup")).unwrap();
    let draft = e.create_changeset("r", vec![c.id.clone()], "alice", "draft", vec![put(&e, "d.txt", "d\n")]).unwrap();
    let id = e.create_review("r", &draft.id, "main", "t", "alice").unwrap();
    e.comment("r", id, "bob", None, "hi").unwrap();
    let rep = e.backup(&d.path().join("bk")).unwrap();
    assert!(rep.rows > 0 && rep.blobs >= 2);
    let b2: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(d.path().join("b2")).unwrap());
    let f = Engine::open(d.path().join("b.db").to_str().unwrap(), b2).unwrap();
    let r2 = f.restore(&d.path().join("bk")).unwrap();
    assert_eq!((r2.rows, r2.blobs), (rep.rows, rep.blobs));
    assert_eq!(f.get_ref("r", "main").unwrap(), Some(c.id.clone()));
    assert_eq!(f.manifest("r", &c.id).unwrap(), e.manifest("r", &c.id).unwrap());
    assert_eq!(f.verify_audit().unwrap(), (1, None));
    assert_eq!(f.acl("r").unwrap().len(), e.acl("r").unwrap().len());
    // serial counters continue after restore
    f.audit(ev("x")).unwrap();
    assert_eq!(f.verify_audit().unwrap(), (2, None));
    assert!(f.restore(&d.path().join("bk")).is_err(), "restore refuses a non-empty target");
    // tampered backup refused
    let g = Engine::open(d.path().join("c.db").to_str().unwrap(), Arc::new(MemBlobStore::default())).unwrap();
    let man = d.path().join("bk/manifest.json");
    let j = std::fs::read_dir(d.path().join("bk"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    std::fs::write(&j, std::fs::read_to_string(&j).unwrap() + "{}\n").unwrap();
    assert!(g.restore(&d.path().join("bk")).is_err(), "checksum mismatch refused");
    assert!(man.exists());
}
