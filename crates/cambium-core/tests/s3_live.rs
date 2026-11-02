//! Blob store and engine against a real S3-compatible server (the reference deployment
//! uses RustFS). Skipped unless CAMBIUM_TEST_S3_ENDPOINT is set, e.g.:
//!   docker run -d -p 9000:9000 -e RUSTFS_ACCESS_KEY=cambium -e RUSTFS_SECRET_KEY=cambium-secret rustfs/rustfs:1.0.4
//!   CAMBIUM_TEST_S3_ENDPOINT=http://127.0.0.1:9000 CAMBIUM_TEST_S3_ACCESS_KEY=cambium \
//!   CAMBIUM_TEST_S3_SECRET_KEY=cambium-secret cargo test --test s3_live -- --ignored
use cambium_core::{
    blob::{hash_bytes, BlobStore},
    engine::split_entry,
    s3::{S3BlobStore, S3Config},
    Engine, FileChange,
};
use std::sync::Arc;

fn store() -> Option<S3BlobStore> {
    let endpoint = std::env::var("CAMBIUM_TEST_S3_ENDPOINT").ok()?;
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
    let cfg = S3Config {
        endpoint,
        // a fresh bucket per run, so runs never see each other's objects
        bucket: format!("cambium-test-{:x}", rand_suffix()),
        region: env("CAMBIUM_TEST_S3_REGION", "us-east-1"),
        access_key: env("CAMBIUM_TEST_S3_ACCESS_KEY", "cambium"),
        secret_key: env("CAMBIUM_TEST_S3_SECRET_KEY", "cambium-secret"),
        prefix: "blobs/".into(),
    };
    let s = S3BlobStore::new(cfg);
    s.ensure_bucket().expect("create bucket");
    s.ensure_bucket().expect("creating it again is fine");
    Some(s)
}
fn rand_suffix() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    t.as_nanos() as u64 ^ (std::process::id() as u64) << 32
}

#[test]
#[ignore = "needs an S3-compatible server: set CAMBIUM_TEST_S3_ENDPOINT"]
fn live_s3_put_get_has() {
    let Some(s) = store() else { return };
    let data = b"stored in a real object store";
    let id = s.put(data).unwrap();
    assert_eq!(id, hash_bytes(data));
    assert_eq!(s.put(data).unwrap(), id, "idempotent");
    assert_eq!(s.get(&id).unwrap().unwrap(), data);
    assert!(s.has(&id).unwrap());
    let absent = "0".repeat(64);
    assert!(!s.has(&absent).unwrap(), "404 is 'absent', not an error");
    assert!(s.get(&absent).unwrap().is_none());
    // a 1 MiB object round-trips intact
    let big: Vec<u8> = (0..1 << 20).map(|i| (i * 31 % 251) as u8).collect();
    let bid = s.put(&big).unwrap();
    assert_eq!(s.get(&bid).unwrap().unwrap(), big);
}

#[test]
#[ignore = "needs an S3-compatible server: set CAMBIUM_TEST_S3_ENDPOINT"]
fn live_s3_engine_commit_land_merge() {
    let Some(s) = store() else { return };
    let d = tempfile::tempdir().unwrap();
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(s)).unwrap();
    let root = e.create_repo("r", "u").unwrap();
    let put = |p: &str, c: &str| FileChange { path: p.into(), blob: Some(e.put_blob(c.as_bytes()).unwrap()), mode: 0o100644 };
    let base = e.commit("r", "main", &root, "u", "base", vec![put("f", "1\n2\n3\n")]).unwrap().id;
    let draft = e.create_changeset("r", vec![base.clone()], "u", "mine", vec![put("f", "ONE\n2\n3\n")]).unwrap().id;
    e.commit("r", "main", &base, "v", "theirs", vec![put("f", "1\n2\nTHREE\n")]).unwrap();
    let tip = e.land("r", &draft, "main", &|_| Ok(())).unwrap();
    let m = e.manifest("r", &tip).unwrap();
    assert_eq!(e.read_text(split_entry(&m["f"]).0).unwrap(), "ONE\n2\nTHREE\n", "line merge on S3-backed blobs");
}
