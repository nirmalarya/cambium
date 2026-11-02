//! Blob storage abstraction. Content-addressed, immutable objects.
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub fn hash_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Constant-time comparison of two secrets (compares their hashes, so the time does
/// not depend on where they differ or on their lengths).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

pub trait BlobStore: Send + Sync {
    fn put(&self, data: &[u8]) -> std::io::Result<String>;
    fn get(&self, id: &str) -> std::io::Result<Option<Vec<u8>>>;
    /// Whether the blob exists. Storage errors are errors, not "absent".
    fn has(&self, id: &str) -> std::io::Result<bool> {
        Ok(self.get(id)?.is_some())
    }
}

/// Local filesystem backend: zlib-compressed objects, keyed by the hash of the
/// uncompressed bytes, fanned out by the first two hex chars.
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    fn path(&self, id: &str) -> PathBuf {
        self.root.join(&id[..2]).join(id)
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Temp names unique per process and call, so concurrent uploads of the same
/// content never share (and truncate) one temp file.
fn tmp_name(p: &std::path::Path) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    p.with_extension(format!("tmp-{}-{n}", std::process::id()))
}

impl BlobStore for FsBlobStore {
    /// Write to a unique temp file, fsync it, rename into place, fsync the folder.
    /// A crash leaves either no object or a complete one, never a partial file
    /// under the final name.
    fn put(&self, data: &[u8]) -> std::io::Result<String> {
        let id = hash_bytes(data);
        let p = self.path(&id);
        if !p.exists() {
            let dir = p.parent().unwrap();
            std::fs::create_dir_all(dir)?;
            let tmp = tmp_name(&p);
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut enc, data)?;
            let r = (|| {
                let mut f = std::fs::File::create(&tmp)?;
                std::io::Write::write_all(&mut f, &enc.finish()?)?;
                f.sync_all()?;
                std::fs::rename(&tmp, &p)?;
                std::fs::File::open(dir)?.sync_all()
            })();
            if r.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            r?;
        }
        Ok(id)
    }
    /// Read and verify against the id. A corrupt object is moved aside (so the next
    /// upload of the same content rewrites it) and reported as an error, never served.
    fn get(&self, id: &str) -> std::io::Result<Option<Vec<u8>>> {
        if !valid_id(id) {
            return Ok(None);
        }
        let p = self.path(id);
        let d = match std::fs::read(&p) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        let ok = std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&d[..]), &mut out).is_ok() && hash_bytes(&out) == id;
        if !ok {
            let _ = std::fs::rename(&p, p.with_extension("corrupt"));
            crate::log::error("blob failed integrity check; moved aside", serde_json::json!({"blob": id}));
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("blob {id} is corrupt")));
        }
        Ok(Some(out))
    }
    /// Existence only: no read or decompression.
    fn has(&self, id: &str) -> std::io::Result<bool> {
        Ok(valid_id(id) && self.path(id).try_exists()?)
    }
}

/// In-memory backend, used by tests.
#[derive(Default)]
pub struct MemBlobStore {
    m: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl BlobStore for MemBlobStore {
    fn put(&self, data: &[u8]) -> std::io::Result<String> {
        let id = hash_bytes(data);
        self.m.lock().unwrap().insert(id.clone(), data.to_vec());
        Ok(id)
    }
    fn get(&self, id: &str) -> std::io::Result<Option<Vec<u8>>> {
        Ok(self.m.lock().unwrap().get(id).cloned())
    }
    fn has(&self, id: &str) -> std::io::Result<bool> {
        Ok(self.m.lock().unwrap().contains_key(id))
    }
}
