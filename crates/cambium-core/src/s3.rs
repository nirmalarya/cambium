//! S3-compatible object-store backend (path-style requests, AWS Signature V4).
//! Works with any S3-compatible object store. Objects are zlib-compressed,
//! keyed `<prefix><id>`; `id` is the sha256 of the uncompressed bytes.
use crate::blob::{hash_bytes, BlobStore};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::time::Duration;

#[derive(Clone)]
pub struct S3Config {
    /// e.g. `http://127.0.0.1:9000` or `https://objects.example.internal`
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub prefix: String,
}

impl S3Config {
    pub fn from_env() -> Result<Self, String> {
        let need = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).ok_or(format!("{k} is required for CAMBIUM_BLOB=s3"));
        let prefix = std::env::var("CAMBIUM_S3_PREFIX").unwrap_or_else(|_| "blobs/".into());
        if !prefix.chars().all(|c| c.is_ascii_alphanumeric() || "/_-.".contains(c)) {
            return Err("CAMBIUM_S3_PREFIX: use [A-Za-z0-9/_-.]".into());
        }
        Ok(Self {
            endpoint: need("CAMBIUM_S3_ENDPOINT")?.trim_end_matches('/').into(),
            bucket: need("CAMBIUM_S3_BUCKET")?,
            region: std::env::var("CAMBIUM_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
            access_key: need("CAMBIUM_S3_ACCESS_KEY")?,
            secret_key: need("CAMBIUM_S3_SECRET_KEY")?,
            prefix,
        })
    }
}

pub struct S3BlobStore {
    cfg: S3Config,
    agent: ureq::Agent,
}

type HmacSha256 = Hmac<Sha256>;
fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).unwrap();
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// AWS Signature V4 for S3. `headers` must include host, x-amz-content-sha256 and
/// x-amz-date; every header given is signed. Returns the Authorization header value.
#[allow(clippy::too_many_arguments)]
pub fn sign_v4(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    amz_date: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let mut h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).collect();
    h.sort();
    let canon_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let creq = format!("{method}\n{path}\n{query}\n{canon_headers}\n{signed}\n{payload_hash}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(creq.as_bytes())));
    let k = hmac(format!("AWS4{secret_key}").as_bytes(), date);
    let k = hmac(&k, region);
    let k = hmac(&k, "s3");
    let k = hmac(&k, "aws4_request");
    let sig = hex::encode(hmac(&k, &sts));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

/// UTC `YYYYMMDDTHHMMSSZ` from unix seconds (no date crate needed).
pub fn amz_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // civil-from-days (proleptic Gregorian)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl S3BlobStore {
    pub fn new(cfg: S3Config) -> Self {
        Self { cfg, agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build() }
    }
    /// Create the bucket if it does not exist yet (e.g. a fresh RustFS or MinIO).
    /// "Already exists / owned by you" counts as success.
    pub fn ensure_bucket(&self) -> std::io::Result<()> {
        let path = format!("/{}", self.cfg.bucket);
        match self.signed(&path, "PUT", b"") {
            Ok(_) | Err(ureq::Error::Status(409, _)) => Ok(()),
            Err(e) => Err(io(e)),
        }
    }
    #[allow(clippy::result_large_err)] // ureq's own error type; boxed at the BlobStore boundary
    fn request(&self, method: &str, id: &str, body: &[u8]) -> Result<ureq::Response, ureq::Error> {
        let path = format!("/{}/{}{}", self.cfg.bucket, self.cfg.prefix, id);
        self.signed(&path, method, body)
    }
    #[allow(clippy::result_large_err)]
    fn signed(&self, path: &str, method: &str, body: &[u8]) -> Result<ureq::Response, ureq::Error> {
        let path = path.to_string();
        let host = self.cfg.endpoint.split("://").nth(1).unwrap_or(&self.cfg.endpoint).to_string();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let date = amz_date(now);
        let ph = hex::encode(Sha256::digest(body));
        let auth = sign_v4(
            method,
            &path,
            "",
            &[("host", &host), ("x-amz-content-sha256", &ph), ("x-amz-date", &date)],
            &ph,
            &date,
            &self.cfg.region,
            &self.cfg.access_key,
            &self.cfg.secret_key,
        );
        let req = self
            .agent
            .request(method, &format!("{}{}", self.cfg.endpoint, path))
            .set("x-amz-content-sha256", &ph)
            .set("x-amz-date", &date)
            .set("authorization", &auth);
        if method == "PUT" {
            req.send_bytes(body)
        } else {
            req.call()
        }
    }
}

impl BlobStore for S3BlobStore {
    fn put(&self, data: &[u8]) -> std::io::Result<String> {
        let id = hash_bytes(data);
        if self.has(&id)? {
            return Ok(id);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data)?;
        self.request("PUT", &id, &z.finish()?).map_err(io)?;
        Ok(id)
    }
    fn get(&self, id: &str) -> std::io::Result<Option<Vec<u8>>> {
        if !valid_id(id) {
            return Ok(None);
        }
        match self.request("GET", id, b"") {
            Ok(r) => {
                let mut raw = Vec::new();
                r.into_reader().read_to_end(&mut raw)?;
                let mut out = Vec::new();
                flate2::read::ZlibDecoder::new(&raw[..]).read_to_end(&mut out)?;
                if hash_bytes(&out) != id {
                    return Err(io(format!("object {id} failed integrity check")));
                }
                Ok(Some(out))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(io(e)),
        }
    }
    fn has(&self, id: &str) -> std::io::Result<bool> {
        if !valid_id(id) {
            return Ok(false);
        }
        match self.request("HEAD", id, b"") {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Published example from the S3 SigV4 documentation (GET object with a Range header).
    #[test]
    fn sigv4_matches_published_example() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let auth = sign_v4(
            "GET",
            "/test.txt",
            "",
            &[
                ("Host", "examplebucket.s3.amazonaws.com"),
                ("Range", "bytes=0-9"),
                ("x-amz-content-sha256", empty),
                ("x-amz-date", "20130524T000000Z"),
            ],
            empty,
            "20130524T000000Z",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert!(auth.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }
    #[test]
    fn dates() {
        assert_eq!(amz_date(1369353600), "20130524T000000Z");
        assert_eq!(amz_date(951782400 + 3661), "20000229T010101Z");
    }
}
