//! S3 backend against an in-process fake S3 server that checks every request's
//! SigV4 signature, path-style bucket routing and payload hash.
use cambium_core::blob::{hash_bytes, BlobStore};
use cambium_core::s3::{sign_v4, S3BlobStore, S3Config};
use cambium_core::{Engine, FileChange};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const AK: &str = "TESTACCESSKEY";
const SK: &str = "test-secret-key-not-real";

struct Fake {
    addr: String,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    rejected: Arc<Mutex<u32>>,
}

fn fake_s3() -> Fake {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let objects: Arc<Mutex<HashMap<String, Vec<u8>>>> = Default::default();
    let rejected: Arc<Mutex<u32>> = Default::default();
    let (o, r, host) = (objects.clone(), rejected.clone(), addr.clone());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let (o, r, host) = (o.clone(), r.clone(), host.clone());
            std::thread::spawn(move || {
                let mut s = s.unwrap();
                let mut rd = BufReader::new(s.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut it = line.split_whitespace();
                    let (method, path) = (it.next().unwrap().to_string(), it.next().unwrap().to_string());
                    let mut h: HashMap<String, String> = HashMap::new();
                    loop {
                        let mut l = String::new();
                        rd.read_line(&mut l).unwrap();
                        let l = l.trim_end();
                        if l.is_empty() {
                            break;
                        }
                        let (k, v) = l.split_once(':').unwrap();
                        h.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    }
                    let n: usize = h.get("content-length").map_or(0, |v| v.parse().unwrap());
                    let mut body = vec![0; n];
                    rd.read_exact(&mut body).unwrap();
                    // verify signature exactly as a real S3 endpoint would
                    let ph = hex::encode(Sha256::digest(&body));
                    let date = h.get("x-amz-date").cloned().unwrap_or_default();
                    let expect = sign_v4(
                        &method,
                        &path,
                        "",
                        &[("host", &host), ("x-amz-content-sha256", &ph), ("x-amz-date", &date)],
                        &ph,
                        &date,
                        "eu-central-1",
                        AK,
                        SK,
                    );
                    let ok_sig = h.get("authorization") == Some(&expect) && h.get("x-amz-content-sha256") == Some(&ph);
                    let (code, out): (u16, Vec<u8>) = if !ok_sig {
                        *r.lock().unwrap() += 1;
                        (403, b"SignatureDoesNotMatch".to_vec())
                    } else if !path.starts_with("/cambium-bucket/") {
                        (404, b"NoSuchBucket".to_vec())
                    } else {
                        match method.as_str() {
                            "PUT" => {
                                o.lock().unwrap().insert(path.clone(), body);
                                (200, vec![])
                            }
                            "GET" => match o.lock().unwrap().get(&path) {
                                Some(b) => (200, b.clone()),
                                None => (404, b"NoSuchKey".to_vec()),
                            },
                            "HEAD" => {
                                if o.lock().unwrap().contains_key(&path) {
                                    (200, vec![])
                                } else {
                                    (404, vec![])
                                }
                            }
                            _ => (405, vec![]),
                        }
                    };
                    let len = if method == "HEAD" { 0 } else { out.len() };
                    write!(s, "HTTP/1.1 {code} X\r\nContent-Length: {len}\r\n\r\n").unwrap();
                    if method != "HEAD" {
                        s.write_all(&out).unwrap();
                    }
                }
            });
        }
    });
    Fake { addr, objects, rejected }
}

fn cfg(f: &Fake, secret: &str) -> S3Config {
    S3Config {
        endpoint: format!("http://{}", f.addr),
        bucket: "cambium-bucket".into(),
        region: "eu-central-1".into(),
        access_key: AK.into(),
        secret_key: secret.into(),
        prefix: "blobs/".into(),
    }
}

#[test]
fn s3_put_get_has_roundtrip() {
    let f = fake_s3();
    let st = S3BlobStore::new(cfg(&f, SK));
    let data = b"hello object store\n".repeat(100);
    let id = st.put(&data).unwrap();
    assert_eq!(id, hash_bytes(&data));
    assert!(st.has(&id));
    assert_eq!(st.get(&id).unwrap().unwrap(), data);
    // stored compressed under the prefix
    let stored = f.objects.lock().unwrap()[&format!("/cambium-bucket/blobs/{id}")].clone();
    assert!(stored.len() < data.len());
    // missing and malformed ids
    assert!(st.get(&"0".repeat(64)).unwrap().is_none());
    assert!(!st.has("../etc/passwd"));
    assert!(st.get("zz").unwrap().is_none());
    // idempotent put
    assert_eq!(st.put(&data).unwrap(), id);
    assert_eq!(*f.rejected.lock().unwrap(), 0);
}

#[test]
fn s3_wrong_secret_is_rejected() {
    let f = fake_s3();
    let st = S3BlobStore::new(cfg(&f, "wrong"));
    assert!(st.put(b"x").is_err());
    assert!(*f.rejected.lock().unwrap() > 0);
}

#[test]
fn s3_detects_corrupted_object() {
    let f = fake_s3();
    let st = S3BlobStore::new(cfg(&f, SK));
    let id = st.put(b"original").unwrap();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(b"tampered").unwrap();
    f.objects.lock().unwrap().insert(format!("/cambium-bucket/blobs/{id}"), z.finish().unwrap());
    assert!(st.get(&id).is_err());
}

#[test]
fn engine_runs_on_s3_blobs() {
    let f = fake_s3();
    let d = tempfile::tempdir().unwrap();
    let e = Engine::open(d.path().join("t.db").to_str().unwrap(), Arc::new(S3BlobStore::new(cfg(&f, SK)))).unwrap();
    let root = e.create_repo("r", "u").unwrap();
    let b = e.blobs.put(b"line1\n").unwrap();
    let cs =
        e.commit("r", "main", &root, "u", "m", vec![FileChange { path: "a.txt".into(), blob: Some(b.clone()), mode: 0o100644 }]).unwrap();
    assert_eq!(e.read_text(&b).unwrap(), "line1\n");
    assert!(e.diff("r", &root, &cs.id, &[]).unwrap().contains("+line1"));
    // blob not in the store -> rejected
    assert!(e
        .commit("r", "main", &cs.id, "u", "m", vec![FileChange { path: "b".into(), blob: Some("1".repeat(64)), mode: 0o100644 }])
        .is_err());
}
