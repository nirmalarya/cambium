//! Blob storage abstraction. Content-addressed, immutable objects.
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub fn hash_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub trait BlobStore: Send + Sync {
    fn put(&self, data: &[u8]) -> std::io::Result<String>;
    fn get(&self, id: &str) -> std::io::Result<Option<Vec<u8>>>;
    fn has(&self, id: &str) -> bool {
        matches!(self.get(id), Ok(Some(_)))
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

impl BlobStore for FsBlobStore {
    fn put(&self, data: &[u8]) -> std::io::Result<String> {
        let id = hash_bytes(data);
        let p = self.path(&id);
        if !p.exists() {
            std::fs::create_dir_all(p.parent().unwrap())?;
            let tmp = p.with_extension("tmp");
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut enc, data)?;
            std::fs::write(&tmp, enc.finish()?)?;
            std::fs::rename(tmp, &p)?;
        }
        Ok(id)
    }
    fn get(&self, id: &str) -> std::io::Result<Option<Vec<u8>>> {
        if !valid_id(id) {
            return Ok(None);
        }
        match std::fs::read(self.path(id)) {
            Ok(d) => {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&d[..]), &mut out)?;
                Ok(Some(out))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// In-memory backend, used by tests and as a stand-in for an object-store
/// client (an S3-compatible backend implements the same trait; see
/// docs/backend.md — not shipped in this slice).
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
}
