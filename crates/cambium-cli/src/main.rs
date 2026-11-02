// ureq::Error and axum Response are large by design; these paths are network-bound.
#![allow(clippy::result_large_err)]
//! cmb: the thin Cambium client. It lays out files, tracks what you touched,
//! and asks the server for everything else (commits, merges, history, blame, diffs).
use cambium_core::engine::{entry, split_entry, MODE_EXEC, MODE_FILE, MODE_LINK};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
#[cfg(target_os = "linux")]
mod vfs;
mod watch;

#[derive(Serialize, Deserialize, Default)]
struct State {
    server: String,
    repo: String,
    branch: String,
    head: String,
    sparse: Vec<String>,
    stage: Vec<String>,
    /// what is materialised on disk: path -> entry (blob id, ":x" suffix if executable)
    files: BTreeMap<String, String>,
}

struct Client {
    server: String,
    token: String,
}

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("cmb: {}", msg.as_ref());
    exit(1)
}

impl Client {
    fn from_env(server: Option<String>) -> Self {
        let server = std::env::var("CAMBIUM_SERVER").ok().or(server).unwrap_or_else(|| "http://127.0.0.1:7878".into());
        // CAMBIUM_TOKEN wins; otherwise CAMBIUM_TOKEN_CMD runs a credential helper that prints a
        // token on stdout (e.g. an SSO / OIDC device-code login tool).
        let token = std::env::var("CAMBIUM_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| {
                let cmd = std::env::var("CAMBIUM_TOKEN_CMD").ok()?;
                let out = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .output()
                    .unwrap_or_else(|e| die(format!("CAMBIUM_TOKEN_CMD failed to start: {e}")));
                if !out.status.success() {
                    die(format!("CAMBIUM_TOKEN_CMD exited with {}", out.status));
                }
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|t| !t.is_empty())
            })
            .unwrap_or_else(|| {
                die("set CAMBIUM_TOKEN, or CAMBIUM_TOKEN_CMD to a command that prints one (static tokens: `cmbd token <user>`)")
            });
        Client { server: server.trim_end_matches('/').into(), token }
    }
    fn handle(r: Result<ureq::Response, ureq::Error>) -> Value {
        match r {
            Ok(resp) => resp.into_json().unwrap_or(Value::Null),
            Err(ureq::Error::Status(_, resp)) => {
                let v: Value = resp.into_json().unwrap_or(Value::Null);
                let mut m = v["message"].as_str().unwrap_or("request failed").to_string();
                if v["error"] == "forbidden" {
                    m.push_str("\nhint: ask a repo admin for access (`cmb acl list` shows the rules if you are one)");
                }
                if v["error"] == "ref_moved" {
                    m.push_str("\nhint: the branch moved on the server; run `cmb sync` then retry, or `cmb rebase`");
                }
                die(m)
            }
            Err(e) => die(format!("cannot reach server: {e}")),
        }
    }
    /// Send a request, waiting and retrying when the server says 429 (rate limited).
    /// Honors Retry-After; falls back to exponential backoff. CAMBIUM_MAX_RETRIES caps attempts.
    fn send(f: impl Fn() -> Result<ureq::Response, ureq::Error>) -> Result<ureq::Response, ureq::Error> {
        let max: u32 = std::env::var("CAMBIUM_MAX_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
        let mut attempt = 0;
        loop {
            match f() {
                Err(ureq::Error::Status(429, r)) if attempt < max => {
                    let backoff = 1u64 << attempt.min(5);
                    let wait = r.header("retry-after").and_then(|v| v.parse::<u64>().ok()).unwrap_or(backoff).clamp(1, 60);
                    eprintln!("cmb: rate limited by server; retrying in {wait}s ({}/{max})", attempt + 1);
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                    attempt += 1;
                }
                other => return other,
            }
        }
    }
    fn auth(&self, r: ureq::Request) -> ureq::Request {
        r.set("Authorization", &format!("Bearer {}", self.token))
    }
    fn get(&self, path: &str, q: &[(&str, &str)]) -> Value {
        let mut r = self.auth(http().get(&format!("{}{}", self.server, path)));
        for (k, v) in q {
            r = r.query(k, v);
        }
        Self::handle(Self::send(|| r.clone().call()))
    }
    fn post(&self, path: &str, body: Value) -> Value {
        Self::handle(Self::send(|| self.auth(http().post(&format!("{}{}", self.server, path))).send_json(body.clone())))
    }
    /// POST with an Idempotency-Key: a dropped connection is retried with the same
    /// key, so the server applies the commit/merge at most once.
    fn post_idem(&self, path: &str, body: Value) -> Value {
        let key = {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            hex::encode(b)
        };
        let max: u32 = std::env::var("CAMBIUM_MAX_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
        let mut attempt = 0;
        loop {
            let r = Self::send(|| {
                self.auth(http().post(&format!("{}{}", self.server, path))).set("Idempotency-Key", &key).send_json(body.clone())
            });
            match r {
                Err(ureq::Error::Transport(e)) if attempt < max => {
                    eprintln!("cmb: connection problem ({e}); retrying safely ({}/{max})", attempt + 1);
                }
                Err(ureq::Error::Status(409, resp)) if attempt < max && resp.header("retry-after").is_some() => {}
                other => return Self::handle(other),
            }
            attempt += 1;
            std::thread::sleep(std::time::Duration::from_millis(500u64 << attempt.min(4)));
        }
    }
    fn put_blob(&self, data: &[u8]) -> String {
        let v = Self::handle(Self::send(|| self.auth(http().post(&format!("{}/v1/blobs", self.server))).send_bytes(data)));
        v["id"].as_str().unwrap().to_string()
    }
    /// Fetch a blob into the cache, resuming from a partial download if one exists.
    /// Bytes are appended to `<cache>-partial/<id>` as they arrive; a dropped connection
    /// or an interrupted process picks up at the byte it stopped at (HTTP Range).
    /// CAMBIUM_FETCH_CHUNK sets the bytes per request (default: rest of the blob).
    fn get_blob(&self, id: &str) -> Vec<u8> {
        if let Some(d) = cache_get(id) {
            return d;
        }
        if !valid_id(id) {
            die(format!("bad blob id {id}"));
        }
        NET_FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pdir = partial_dir();
        std::fs::create_dir_all(&pdir).unwrap();
        let part = pdir.join(id);
        let chunk: u64 = std::env::var("CAMBIUM_FETCH_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        // test hook: stop after N range requests, as if the process were killed mid-fetch
        let abort_after: Option<u32> = std::env::var("CAMBIUM_FETCH_ABORT_AFTER").ok().and_then(|v| v.parse().ok());
        let mut requests = 0u32;
        let mut net_errors = 0u32;
        let start = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if start > 0 {
            eprintln!("cmb: resuming blob {} at byte {start}", short(id));
        }
        loop {
            let off = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            if abort_after == Some(requests) {
                eprintln!("cmb: fetch interrupted at byte {off} (test hook)");
                exit(3);
            }
            let range = if chunk > 0 { format!("bytes={off}-{}", off + chunk - 1) } else { format!("bytes={off}-") };
            let url = format!("{}/v1/blobs/{}", self.server, id);
            requests += 1;
            let resp = match Self::send(|| self.auth(http().get(&url)).set("Range", &range).call()) {
                Ok(r) => r,
                Err(ureq::Error::Status(416, _)) if off > 0 => {
                    let _ = std::fs::remove_file(&part);
                    continue;
                } // stale partial
                Err(ureq::Error::Status(c, _)) => die(format!("fetch blob {id}: server said {c}")),
                Err(e) => {
                    net_errors += 1;
                    if net_errors > 5 {
                        die(format!("fetch blob {id}: {e} (partial kept; rerun to resume)"));
                    }
                    eprintln!("cmb: fetch of {} dropped at byte {off}: {e}; retrying", short(id));
                    std::thread::sleep(std::time::Duration::from_millis(300 * net_errors as u64));
                    continue;
                }
            };
            let total: Option<u64> = if resp.status() == 206 {
                resp.header("content-range").and_then(|v| v.rsplit('/').next()).and_then(|v| v.parse().ok())
            } else {
                None
            };
            let mut f = if resp.status() == 206 {
                std::fs::OpenOptions::new().create(true).append(true).open(&part).unwrap()
            } else {
                std::fs::File::create(&part).unwrap()
            }; // server sent the whole thing
            let mut rd = resp.into_reader();
            let mut buf = [0u8; 64 * 1024];
            let mut broke = false;
            loop {
                match rd.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => f.write_all(&buf[..n]).unwrap(),
                    Err(e) => {
                        eprintln!("cmb: fetch of {} interrupted: {e}; resuming", short(id));
                        broke = true;
                        break;
                    }
                }
            }
            drop(f);
            if broke {
                net_errors += 1;
                if net_errors > 5 {
                    die("too many interrupted fetches (partial kept; rerun to resume)");
                }
                continue;
            }
            let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            if total.is_some_and(|t| have < t) {
                continue;
            }
            break;
        }
        let buf = std::fs::read(&part).unwrap();
        let _ = std::fs::remove_file(&part);
        if hex::encode(Sha256::digest(&buf)) != id {
            die(format!("blob {id} failed integrity check (partial discarded; rerun)"));
        }
        cache_put(id, &buf);
        buf
    }
}

// ---------- bounded local object cache ----------
/// Per-user cache (CAMBIUM_CACHE, default ~/.cache/cambium).
fn user_cache_dir() -> PathBuf {
    std::env::var("CAMBIUM_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache/cambium"))
}
/// Object cache: CAMBIUM_SHARED_CACHE (one per machine, shared by its users and
/// working copies) when set, else the per-user cache. Entries are content
/// addressed and verified on every read, so a corrupt or tampered entry is
/// discarded and refetched rather than trusted.
fn cache_dir() -> PathBuf {
    std::env::var("CAMBIUM_SHARED_CACHE").map(PathBuf::from).unwrap_or_else(|_| user_cache_dir())
}
/// Partial downloads stay per-user so two users never append to one file.
fn partial_dir() -> PathBuf {
    PathBuf::from(format!("{}-partial", user_cache_dir().display()))
}
static CACHE_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static NET_FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn cache_get(id: &str) -> Option<Vec<u8>> {
    if !valid_id(id) {
        return None;
    }
    let p = cache_dir().join(id);
    let d = std::fs::read(&p).ok()?;
    if hex::encode(Sha256::digest(&d)) != id {
        eprintln!("cmb: cache entry {} failed integrity check; refetching", short(id));
        let _ = std::fs::remove_file(&p);
        return None;
    }
    CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(d)
}
fn cache_put(id: &str, data: &[u8]) {
    let d = cache_dir();
    let shared = std::env::var("CAMBIUM_SHARED_CACHE").is_ok();
    if !d.exists() {
        let _ = std::fs::create_dir_all(&d);
        #[cfg(unix)]
        if shared {
            // group-shared: members of the directory's group can read and add entries
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o2770));
        }
    }
    // write-then-rename so a concurrent reader never sees a half-written entry
    let tmp = d.join(format!(".tmp-{}-{}", std::process::id(), id));
    if std::fs::write(&tmp, data).is_err() {
        return;
    }
    #[cfg(unix)]
    if shared {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o660));
    }
    if std::fs::rename(&tmp, d.join(id)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let max: u64 = std::env::var("CAMBIUM_CACHE_MAX").ok().and_then(|s| s.parse().ok()).unwrap_or(256 << 20);
    let mut ents: Vec<_> = std::fs::read_dir(&d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(valid_id))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = ents.iter().map(|e| e.1).sum();
    ents.sort();
    for (_, len, p) in ents {
        if total <= max {
            break;
        }
        let _ = std::fs::remove_file(p);
        total -= len;
    }
}

// ---------- working copy ----------
fn find_root() -> PathBuf {
    let mut d = std::env::current_dir().unwrap();
    loop {
        if d.join(".cambium/state.json").exists() || mount_meta(&d).join("state.json").exists() {
            return d;
        }
        if !d.pop() {
            die("not inside a Cambium working copy (run `cmb checkout` or `cmb init`)");
        }
    }
}
/// Metadata for a writable mount lives outside the mount, keyed by its path.
pub(crate) fn mount_meta(dir: &Path) -> PathBuf {
    PathBuf::from(format!("{}-mounts", user_cache_dir().display()))
        .join(&cambium_core::blob::hash_bytes(dir.to_string_lossy().as_bytes())[..16])
}
/// Working-copy metadata directory: `<root>/.cambium`, or the mount's metadata dir.
pub(crate) fn meta(root: &Path) -> PathBuf {
    let m = mount_meta(root);
    if !root.join(".cambium/state.json").exists() && m.join("state.json").exists() {
        m
    } else {
        root.join(".cambium")
    }
}
fn load(root: &Path) -> State {
    serde_json::from_slice(&std::fs::read(meta(root).join("state.json")).unwrap()).unwrap()
}
fn save(root: &Path, s: &State) {
    let m = meta(root);
    std::fs::create_dir_all(&m).unwrap();
    let tmp = m.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap()).unwrap();
    std::fs::rename(tmp, m.join("state.json")).unwrap();
}
fn in_sparse(s: &State, p: &str) -> bool {
    s.sparse.is_empty()
        || s.sparse.iter().any(|x| {
            let x = x.trim_end_matches('/');
            p == x || p.starts_with(&format!("{x}/"))
        })
}
/// True if a file or symlink (even a dangling one) is at `p`.
fn present(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}
fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}
/// What gets stored for a path: file bytes, or the target of a symlink.
fn read_content(p: &Path) -> Option<Vec<u8>> {
    if is_link(p) {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return std::fs::read_link(p).ok().map(|t| t.as_os_str().as_bytes().to_vec());
        }
    }
    std::fs::read(p).ok()
}
fn file_mode(p: &Path) -> u32 {
    if is_link(p) {
        return MODE_LINK;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) {
            return MODE_EXEC;
        }
    }
    MODE_FILE
}
/// Entry for a file on disk: content hash plus executable bit.
fn hash_file(p: &Path) -> Option<String> {
    read_content(p).map(|d| entry(&hex::encode(Sha256::digest(d)), file_mode(p)))
}
fn set_mode(p: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(if mode == MODE_EXEC { 0o755 } else { 0o644 }));
    }
}
/// Write one manifest entry (content + mode) into the working copy.
/// Resolve a server-provided path inside the working copy. Refuses paths that are not
/// plain relative paths, and paths whose parent directories are symlinks or files, so
/// nothing the server sends can make us write or delete outside `root`.
fn wc_path(root: &Path, p: &str) -> Result<PathBuf, String> {
    if !cambium_core::engine::valid_path(p) {
        return Err(format!("server sent an unsafe path {p:?}; refusing to touch it"));
    }
    let mut cur = root.to_path_buf();
    let segs: Vec<&str> = p.split('/').collect();
    for seg in &segs[..segs.len() - 1] {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("refusing to touch {p}: {} is a symlink", cur.strip_prefix(root).unwrap_or(&cur).display()))
            }
            Ok(m) if !m.is_dir() => {
                return Err(format!("cannot write {p}: {} is a file", cur.strip_prefix(root).unwrap_or(&cur).display()))
            }
            _ => {}
        }
    }
    Ok(root.join(p))
}
fn write_entry(c: &Client, root: &Path, p: &str, e: &str) {
    let (blob, mode) = split_entry(e);
    let fp = wc_path(root, p).unwrap_or_else(|e| die(e));
    // a folder that became a file: remove it if it is empty (its files were removed first)
    if std::fs::symlink_metadata(&fp).map(|m| m.is_dir()).unwrap_or(false) {
        let _ = std::fs::remove_dir(&fp);
    }
    if let Err(err) = std::fs::create_dir_all(fp.parent().unwrap()) {
        die(format!("cannot create the folder for {p}: {err}"));
    }
    let data = c.get_blob(blob);
    if is_link(&fp) || (mode == MODE_LINK && present(&fp)) {
        let _ = std::fs::remove_file(&fp);
    }
    if mode == MODE_LINK {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), &fp).unwrap();
        }
        return;
    }
    std::fs::write(&fp, data).unwrap();
    set_mode(&fp, mode);
}
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.file_name().map(|n| n == ".cambium").unwrap_or(false) {
            continue;
        }
        if p.is_dir() && !is_link(&p) {
            walk(root, &p, out);
        } else {
            out.push(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}
fn rel(root: &Path, arg: &str) -> String {
    let abs = std::env::current_dir().unwrap().join(arg);
    // canonicalise the parent only, so a symlink argument names the link, not its target
    let abs = match (abs.parent(), abs.file_name()) {
        (Some(d), Some(n)) if n != ".." && n != "." => d.canonicalize().map(|d| d.join(n)).unwrap_or(abs.clone()),
        _ => abs.canonicalize().unwrap_or(abs.clone()),
    };
    let root = root.canonicalize().unwrap();
    abs.strip_prefix(&root).unwrap_or_else(|_| die(format!("{arg} is outside the working copy"))).to_string_lossy().replace('\\', "/")
}

/// path -> (Some(blob) | None for deleted) for every local change vs the checked-out tree.
fn changes(root: &Path, s: &State) -> BTreeMap<String, Option<String>> {
    let mut out = BTreeMap::new();
    let mut seen = Vec::new();
    let stats = std::env::var("CAMBIUM_STATS").is_ok();
    if let Some(c) = watch::candidates(root) {
        // a live `cmb watch`: only paths it saw change (plus what differed at its first scan)
        for p in &c {
            let ap = root.join(p);
            if ap.is_dir() && !is_link(&ap) {
                walk(root, &ap, &mut seen);
            } else if present(&ap) && !p.split('/').any(|x| x == ".cambium") {
                seen.push(p.clone());
            }
            let pre = format!("{p}/");
            for t in s.files.keys() {
                if (t == p || t.starts_with(&pre)) && !present(&root.join(t)) {
                    out.insert(t.clone(), None);
                }
            }
        }
        seen.sort();
        seen.dedup();
        if stats {
            eprintln!("cambium-stats: watch: {} candidate paths", c.len());
        }
        for p in seen.iter().filter(|p| in_sparse(s, p)) {
            let h = hash_file(&root.join(p));
            if s.files.get(p) != h.as_ref() {
                out.insert(p.clone(), h);
            }
        }
        return out;
    }
    walk(root, root, &mut seen);
    if stats {
        eprintln!("cambium-stats: full scan of {} paths", seen.len());
    }
    for p in seen.iter().filter(|p| in_sparse(s, p)) {
        let h = hash_file(&root.join(p));
        if s.files.get(p) != h.as_ref() {
            out.insert(p.clone(), h);
        }
    }
    for p in s.files.keys() {
        if !present(&root.join(p)) {
            out.insert(p.clone(), None);
        }
    }
    watch::record_baseline(root, &out.keys().cloned().collect::<Vec<_>>());
    out
}

/// Bring the working copy to `target`, fetching only files inside the sparse set.
fn materialise(c: &Client, root: &Path, s: &mut State, target: &str) {
    let local = changes(root, s);
    let t = c.post(&format!("/v1/repos/{}/tree", s.repo), json!({"rev": target, "prefixes": s.sparse}));
    let entries: BTreeMap<String, String> = serde_json::from_value(t["entries"].clone()).unwrap();
    let (mut fetched, mut removed) = (0, 0);
    // removals first, so a path can change between file, folder and symlink
    for p in s.files.keys() {
        if !entries.contains_key(p) && !local.contains_key(p) {
            match wc_path(root, p) {
                Ok(fp) => {
                    let _ = std::fs::remove_file(&fp);
                    remove_empty_parents(root, &fp);
                }
                Err(e) => eprintln!("cmb: {e}"),
            }
            removed += 1;
        }
    }
    for (p, b) in &entries {
        if s.files.get(p) == Some(b) {
            continue;
        }
        if local.contains_key(p) {
            die(format!("{p} has local changes that would be overwritten; commit them first"));
        }
        write_entry(c, root, p, b);
        fetched += 1;
    }
    s.files = entries;
    s.head = t["cs"].as_str().unwrap().to_string();
    eprintln!("at {} ({} files fetched, {} removed, {} tracked)", short(&s.head), fetched, removed, s.files.len());
    if std::env::var("CAMBIUM_STATS").is_ok() {
        eprintln!(
            "cambium-stats: {} from cache, {} from server",
            CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed),
            NET_FETCHES.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}
/// Remove now-empty folders between `fp` and the working-copy root.
fn remove_empty_parents(root: &Path, fp: &Path) {
    let mut d = fp.parent();
    while let Some(dir) = d {
        if dir == root || std::fs::remove_dir(dir).is_err() {
            break;
        }
        d = dir.parent();
    }
}
fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    if i < args.len() {
        Some(args.remove(i))
    } else {
        die(format!("{name} needs a value"))
    }
}

const HELP: &str = "cmb - thin client for a Cambium server

  cmb init <repo> [dir]                 create a repo on the server and a working copy
  cmb checkout <repo> <dir> [--branch B] [--sparse a/,b/]
                                       lay out a (sparse) working copy
  cmb status                            local changes vs the checked-out changeset
  cmb add <path>...                     stage paths (deletions too)
  cmb commit -m <msg>                   server creates the changeset and moves the branch
  cmb branch [name]                     list branches, or create one here and switch to it
  cmb switch <branch>                   move the working copy to another branch
  cmb sync                              update to the branch head
  cmb merge <rev>                       server merges <rev> into the current branch
  cmb rebase <onto>                     server rebases the current branch onto <onto>
  cmb log [path] [-n N]                 history (first-parent), optionally for a path
  cmb blame <path>                      per-line origin, computed on the server
  cmb search <text> [--rev R] [-n N]    indexed search of paths and contents (only files you may read)
  cmb watch                             monitor the working copy so status only checks changed paths
  cmb diff [--from R] [--to R] [path]   working copy vs head, or between two revisions (renames/copies detected)
  cmb sparse [add <prefix> | list]      widen or show the sparse set
  cmb mount <repo> <dir> [--rev R] [--sparse a/,b/]
                                       read-only virtual checkout (FUSE); files download on first read
  cmb cat <repo> <rev> <path>           read one file through the API, no working copy
  cmb acl [list]                        show per-directory permission rules (admins)
  cmb acl set <prefix> <principal> <perm>
                                       principal: *, user:<name>, group:<name>; perm: none|read|write|admin
  cmb acl rm <prefix> <principal>       remove a rule
  cmb review create -m <title> [--target main]
                                       ask for review of the current branch
  cmb review [list] | show <n> | comment <n> <text> | approve <n> | merge <n> | close <n>
  cmb protect [<branch> <approvals>]    require N approvals (by others) to change a branch
  cmb whoami                            the user and groups the server sees for your token

Executable bits and symlinks are tracked: `chmod +x` shows as a mode change in status and diff.

env: CAMBIUM_SERVER (default http://127.0.0.1:7878), CAMBIUM_TOKEN, CAMBIUM_CACHE, CAMBIUM_CACHE_MAX,
     CAMBIUM_SHARED_CACHE (one object cache per machine, shared by its users and checkouts),
     CAMBIUM_TOKEN_CMD (command that prints a token, e.g. an SSO login helper),
     CAMBIUM_FETCH_CHUNK (bytes per resumable fetch request), CAMBIUM_MAX_RETRIES (429 retries, default 6)";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "help" || args[0] == "--help" {
        println!("{HELP}");
        return;
    }
    let cmd = args.remove(0);
    match cmd.as_str() {
        "init" => {
            let repo = args.first().cloned().unwrap_or_else(|| die("usage: cmb init <repo> [dir]"));
            let dir = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| repo.clone()));
            let c = Client::from_env(None);
            c.post("/v1/repos", json!({"name": repo}));
            std::fs::create_dir_all(&dir).unwrap();
            let mut s = State { server: c.server.clone(), repo: repo.clone(), branch: "main".into(), ..Default::default() };
            materialise(&c, &dir, &mut s, "main");
            save(&dir, &s);
            println!("created {repo} in {}", dir.display());
        }
        "checkout" => {
            let sparse =
                flag(&mut args, "--sparse").map(|v| v.split(',').map(String::from).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
            let branch = flag(&mut args, "--branch").unwrap_or_else(|| "main".into());
            if args.len() < 2 {
                die("usage: cmb checkout <repo> <dir> [--branch B] [--sparse a/,b/]");
            }
            let dir = PathBuf::from(&args[1]);
            if dir.join(".cambium").exists() {
                die("already a working copy");
            }
            let c = Client::from_env(None);
            std::fs::create_dir_all(&dir).unwrap();
            let mut s = State { server: c.server.clone(), repo: args[0].clone(), branch: branch.clone(), sparse, ..Default::default() };
            materialise(&c, &dir, &mut s, &branch);
            save(&dir, &s);
        }
        "mount" => {
            // cmb mount <repo> <dir> [--rev R] [--sparse a/,b/]: lazy read-only view through FUSE
            // cmb mount <repo> <dir> --write [--branch B]: copy-on-write; `cmb commit` inside it
            let write = match args.iter().position(|a| a == "--write") {
                Some(i) => {
                    args.remove(i);
                    true
                }
                None => false,
            };
            let branch = flag(&mut args, "--branch");
            let rev = flag(&mut args, "--rev").or(branch.clone()).unwrap_or_else(|| "main".into());
            let sparse: Vec<String> =
                flag(&mut args, "--sparse").map(|v| v.split(',').map(String::from).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
            if args.len() < 2 {
                die("usage: cmb mount <repo> <dir> [--rev R] [--sparse a/,b/]");
            }
            let c = Client::from_env(None);
            let t = c.post(&format!("/v1/repos/{}/tree", args[0]), json!({"rev": rev, "prefixes": sparse, "sizes": true}));
            let entries: BTreeMap<String, String> = serde_json::from_value(t["entries"].clone()).unwrap();
            let sizes: BTreeMap<String, u64> = serde_json::from_value(t["sizes"].clone()).unwrap_or_default();
            #[cfg(target_os = "linux")]
            {
                std::fs::create_dir_all(&args[1]).unwrap();
                let dir = Path::new(&args[1]).canonicalize().unwrap();
                let err = |e: std::io::Error| die(format!("mount failed: {e} (is /dev/fuse available and fusermount3 installed?)"));
                if !write {
                    vfs::mount(c, entries, sizes, &dir).unwrap_or_else(err);
                    return;
                }
                let m = mount_meta(&dir);
                if m.join("watch.pid").exists()
                    && std::fs::read_to_string(m.join("watch.pid"))
                        .ok()
                        .and_then(|p| p.trim().parse::<u32>().ok())
                        .is_some_and(|p| Path::new(&format!("/proc/{p}")).exists())
                {
                    die(format!("{} is already mounted", dir.display()));
                }
                let _ = std::fs::remove_dir_all(&m);
                std::fs::create_dir_all(&m).unwrap();
                let s = State {
                    server: c.server.clone(),
                    repo: args[0].clone(),
                    branch: rev.clone(),
                    head: t["cs"].as_str().unwrap_or("").into(),
                    sparse: sparse.clone(),
                    stage: vec![],
                    files: entries.clone(),
                };
                std::fs::write(m.join("state.json"), serde_json::to_vec_pretty(&s).unwrap()).unwrap();
                vfs::mount_rw(c, entries, sizes, &dir, &m).unwrap_or_else(err);
                // unmounted: keep anything not committed, then retire the metadata
                let left = std::fs::read_dir(m.join("salvage")).map(|d| d.count()).unwrap_or(0);
                let _ = std::fs::remove_file(m.join("watch.pid"));
                let _ = std::fs::remove_file(m.join("state.json"));
                if left > 0 {
                    eprintln!("unmounted with uncommitted changes; copies kept in {}", m.join("salvage").display());
                } else {
                    let _ = std::fs::remove_dir_all(&m);
                    eprintln!("unmounted; no uncommitted changes");
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (entries, sizes, write);
                die("cmb mount needs Linux FUSE; use `cmb checkout --lazy` instead");
            }
        }
        "whoami" => {
            let c = Client::from_env(None);
            let v = c.get("/v1/whoami", &[]);
            let g: Vec<&str> = v["groups"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
            println!(
                "{}{}",
                v["user"].as_str().unwrap_or("?"),
                if g.is_empty() { String::new() } else { format!(" (groups: {})", g.join(", ")) }
            );
        }
        "cat" => {
            if args.len() < 3 {
                die("usage: cmb cat <repo> <rev> <path>");
            }
            let c = Client::from_env(None);
            let t = c.post(&format!("/v1/repos/{}/tree", args[0]), json!({"rev": args[1], "prefixes": [args[2]]}));
            let id = t["entries"][&args[2]].as_str().unwrap_or_else(|| die("no such file"));
            std::io::stdout().write_all(&c.get_blob(split_entry(id).0)).unwrap();
        }
        _ => {
            let root = find_root();
            let mut s = load(&root);
            let c = Client::from_env(Some(s.server.clone()));
            run_wc(&cmd, args, &c, &root, &mut s);
            save(&root, &s);
        }
    }
}

fn run_wc(cmd: &str, mut args: Vec<String>, c: &Client, root: &Path, s: &mut State) {
    let repo = s.repo.clone();
    match cmd {
        "status" => {
            println!(
                "on {} at {}{}",
                s.branch,
                short(&s.head),
                if s.sparse.is_empty() { String::new() } else { format!(" (sparse: {})", s.sparse.join(", ")) }
            );
            let ch = changes(root, s);
            if ch.is_empty() && s.stage.is_empty() {
                println!("clean");
            }
            for (p, h) in &ch {
                let st = if s.stage.contains(p) { "staged" } else { "      " };
                let old = s.files.get(p).map(|e| split_entry(e));
                let new = h.as_deref().map(split_entry);
                let (kind, extra) = match (old, new) {
                    (None, Some((_, m))) => (
                        "new     ",
                        match m {
                            MODE_EXEC => " (executable)".to_string(),
                            MODE_LINK => " (symlink)".to_string(),
                            _ => String::new(),
                        },
                    ),
                    (Some(_), None) => ("deleted ", String::new()),
                    (Some((ob, om)), Some((nb, nm))) if om != nm => {
                        (if ob == nb { "mode    " } else { "modified" }, format!(" ({om:o} -> {nm:o})"))
                    }
                    _ => ("modified", String::new()),
                };
                println!("  {st} {kind} {p}{extra}");
            }
        }
        "add" => {
            if args.is_empty() {
                die("usage: cmb add <path>...");
            }
            let ch = changes(root, s);
            for a in &args {
                let r = rel(root, a);
                let pre = format!("{}/", r.trim_end_matches('/'));
                let hits: Vec<_> = ch.keys().filter(|p| r.is_empty() || **p == r || p.starts_with(&pre)).cloned().collect();
                if hits.is_empty() {
                    eprintln!("cmb: nothing changed at {a}");
                }
                for h in hits {
                    if !s.stage.contains(&h) {
                        s.stage.push(h);
                    }
                }
            }
        }
        "commit" => {
            let msg = flag(&mut args, "-m").unwrap_or_else(|| die("usage: cmb commit -m <message>"));
            if s.stage.is_empty() {
                die("nothing staged (use `cmb add`)");
            }
            let mut files = vec![];
            for p in &s.stage {
                let fp = root.join(p);
                let blob = if present(&fp) { Some(c.put_blob(&read_content(&fp).unwrap())) } else { None };
                files.push(json!({"path": p, "blob": blob, "mode": file_mode(&fp)}));
            }
            let v = c.post_idem(
                &format!("/v1/repos/{repo}/commit"),
                json!({"ref": s.branch, "expected": s.head, "message": msg, "files": files}),
            );
            for f in v["files"].as_array().unwrap() {
                let p = f["path"].as_str().unwrap().to_string();
                let mode = f["mode"].as_u64().unwrap_or(MODE_FILE as u64) as u32;
                match f["blob"].as_str() {
                    Some(b) => {
                        s.files.insert(p, entry(b, mode));
                    }
                    None => {
                        s.files.remove(&p);
                    }
                }
            }
            s.head = v["id"].as_str().unwrap().into();
            s.stage.clear();
            println!("[{} {}] {}", s.branch, short(&s.head), msg);
        }
        "branch" => match args.first() {
            None => {
                for r in c.get(&format!("/v1/repos/{repo}/refs"), &[]).as_array().unwrap() {
                    let n = r["name"].as_str().unwrap();
                    println!("{} {} {}", if n == s.branch { "*" } else { " " }, n, short(r["cs"].as_str().unwrap()));
                }
            }
            Some(name) => {
                c.post(&format!("/v1/repos/{repo}/refs/{name}"), json!({"expected": null, "new": s.head}));
                s.branch = name.clone();
                println!("created {name} at {} and switched to it", short(&s.head));
            }
        },
        "switch" => {
            let b = args.first().cloned().unwrap_or_else(|| die("usage: cmb switch <branch>"));
            materialise(c, root, s, &b);
            s.branch = b;
        }
        "sync" => {
            let b = s.branch.clone();
            materialise(c, root, s, &b);
        }
        "merge" => {
            let src = args.first().cloned().unwrap_or_else(|| die("usage: cmb merge <rev>"));
            if !changes(root, s).is_empty() {
                die("commit or discard local changes before merging");
            }
            let v = c.post_idem(&format!("/v1/repos/{repo}/merge"), json!({"target": s.branch, "source": src}));
            println!("merged {src} into {} as {}", s.branch, short(v["id"].as_str().unwrap()));
            let b = s.branch.clone();
            materialise(c, root, s, &b);
        }
        "rebase" => {
            let onto = args.first().cloned().unwrap_or_else(|| die("usage: cmb rebase <onto>"));
            if !changes(root, s).is_empty() {
                die("commit or discard local changes before rebasing");
            }
            let v = c.post(&format!("/v1/repos/{repo}/rebase"), json!({"branch": s.branch, "onto": onto}));
            println!("rebased {} onto {onto}: now {}", s.branch, short(v["cs"].as_str().unwrap()));
            let b = s.branch.clone();
            materialise(c, root, s, &b);
        }
        "log" => {
            let n = flag(&mut args, "-n").unwrap_or_else(|| "20".into());
            let path = args.first().map(|a| rel(root, a));
            let mut q = vec![("rev", s.head.as_str()), ("limit", n.as_str())];
            if let Some(p) = &path {
                q.push(("path", p));
            }
            for cs in c.get(&format!("/v1/repos/{repo}/log"), &q).as_array().unwrap() {
                let merge = if cs["parents"].as_array().unwrap().len() > 1 { " (merge)" } else { "" };
                println!(
                    "{} {:<10} {}{}",
                    short(cs["id"].as_str().unwrap()),
                    cs["author"].as_str().unwrap(),
                    cs["message"].as_str().unwrap(),
                    merge
                );
            }
        }
        "search" => {
            let rev = flag(&mut args, "--rev").unwrap_or_else(|| s.head.clone());
            let n = flag(&mut args, "-n").unwrap_or_else(|| "100".into());
            let q = args.join(" ");
            if q.trim().is_empty() {
                die("usage: cmb search <text> [--rev R] [-n N]");
            }
            let hits = c.get(&format!("/v1/repos/{repo}/search"), &[("q", q.as_str()), ("rev", rev.as_str()), ("limit", n.as_str())]);
            for h in hits.as_array().unwrap() {
                if h["line"] == 0 {
                    println!("{} (path)", h["path"].as_str().unwrap());
                    continue;
                }
                println!("{}:{}: {}", h["path"].as_str().unwrap(), h["line"], h["text"].as_str().unwrap());
            }
        }
        "watch" => watch::run(root),
        "blame" => {
            let p = rel(root, args.first().unwrap_or_else(|| die("usage: cmb blame <path>")));
            for (i, l) in
                c.get(&format!("/v1/repos/{repo}/blame"), &[("rev", &s.head), ("path", &p)]).as_array().unwrap().iter().enumerate()
            {
                println!("{} {:>4}| {}", short(l["cs"].as_str().unwrap()), i + 1, l["line"].as_str().unwrap());
            }
        }
        "diff" => {
            let from = flag(&mut args, "--from");
            let to = flag(&mut args, "--to");
            let prefixes: Vec<String> = args.iter().map(|a| rel(root, a)).collect();
            let v = if let Some(to) = to {
                c.post(&format!("/v1/repos/{repo}/diff"), json!({"from": from.unwrap_or(s.head.clone()), "to": to, "prefixes": prefixes}))
            } else {
                let mut ov = BTreeMap::new();
                for (p, h) in changes(root, s) {
                    if !prefixes.is_empty() && !prefixes.iter().any(|x| p == *x || p.starts_with(&format!("{x}/"))) {
                        continue;
                    }
                    let b = h.map(|e| entry(&c.put_blob(&read_content(&root.join(&p)).unwrap()), split_entry(&e).1));
                    ov.insert(p, b);
                }
                c.post(&format!("/v1/repos/{repo}/diff"), json!({"from": from.unwrap_or(s.head.clone()), "overlay": ov}))
            };
            print!("{}", v["diff"].as_str().unwrap());
        }
        "protect" => {
            // cmb protect <branch> <approvals>   (0 lifts protection); cmb protect = list
            if args.is_empty() {
                for p in c.get(&format!("/v1/repos/{repo}/protected"), &[]).as_array().unwrap() {
                    println!("{:<20} {} approval(s)", p[0].as_str().unwrap(), p[1]);
                }
                return;
            }
            let n: i64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or_else(|| die("usage: cmb protect <branch> <approvals>"));
            c.post(&format!("/v1/repos/{repo}/protected"), json!({"ref": args[0], "approvals": n}));
            println!("{} now needs {n} approval(s) to change", args[0]);
        }
        "review" => {
            let sub = if args.is_empty() { "list".to_string() } else { args.remove(0) };
            let id = |a: &Vec<String>| {
                a.first().and_then(|v| v.trim_start_matches('#').parse::<i64>().ok()).unwrap_or_else(|| die("needs a review number"))
            };
            match sub.as_str() {
                "create" => {
                    let target = flag(&mut args, "--target").unwrap_or_else(|| "main".into());
                    let title = flag(&mut args, "-m").or_else(|| args.first().cloned()).unwrap_or_else(|| die("usage: cmb review create -m <title> [--target main]"));
                    let v = c.post(&format!("/v1/repos/{repo}/reviews"), json!({"source": s.branch, "target": target, "title": title}));
                    println!("review #{} opened: {} -> {} ({}/ui/{repo}/reviews/{})", v["id"], s.branch, target, c.server, v["id"]);
                }
                "list" => for r in c.get(&format!("/v1/repos/{repo}/reviews"), &[]).as_array().unwrap() {
                    println!("#{:<4} {:<7} {}/{} {} -> {}  {}  ({})", r["id"], r["state"].as_str().unwrap(), r["approvals"].as_array().unwrap().len(), r["required"],
                        r["source"].as_str().unwrap(), r["target"].as_str().unwrap(), r["title"].as_str().unwrap(), r["author"].as_str().unwrap());
                },
                "show" => {
                    let v = c.get(&format!("/v1/repos/{repo}/reviews/{}", id(&args)), &[]);
                    let r = &v["review"];
                    println!("#{} {} [{}]\n{} -> {} by {}; approvals {}/{} {:?}", r["id"], r["title"].as_str().unwrap(), r["state"].as_str().unwrap(),
                        r["source"].as_str().unwrap(), r["target"].as_str().unwrap(), r["author"].as_str().unwrap(), r["approvals"].as_array().unwrap().len(), r["required"],
                        r["approvals"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect::<Vec<_>>());
                    for cm in v["comments"].as_array().unwrap() { println!("  {}: {}", cm["author"].as_str().unwrap(), cm["body"].as_str().unwrap()); }
                    print!("{}", v["diff"].as_str().unwrap_or(""));
                }
                "comment" => {
                    let n = id(&args);
                    let body = args.get(1..).map(|a| a.join(" ")).filter(|b| !b.is_empty()).unwrap_or_else(|| die("usage: cmb review comment <n> <text>"));
                    c.post(&format!("/v1/repos/{repo}/reviews/{n}/comments"), json!({"body": body}));
                    println!("commented on #{n}");
                }
                "approve" => { let n = id(&args); let v = c.post(&format!("/v1/repos/{repo}/reviews/{n}/approve"), json!({}));
                    println!("approved #{n} ({}/{} approvals)", v["approvals"].as_array().unwrap().len(), v["required"]); }
                "merge" => { let n = id(&args); let v = c.post(&format!("/v1/repos/{repo}/reviews/{n}/merge"), json!({}));
                    println!("merged #{n} as {}", short(v["id"].as_str().unwrap_or(""))); }
                "close" => { let n = id(&args); c.post(&format!("/v1/repos/{repo}/reviews/{n}/close"), json!({})); println!("closed #{n}"); }
                _ => die("usage: cmb review [list | create -m <title> [--target B] | show <n> | comment <n> <text> | approve <n> | merge <n> | close <n>]"),
            }
        }
        "acl" => match args.first().map(String::as_str) {
            Some("set") | Some("rm") => {
                let rm = args[0] == "rm";
                if args.len() < if rm { 3 } else { 4 } {
                    die("usage: cmb acl set <prefix> <principal> <none|read|write|admin> | cmb acl rm <prefix> <principal>");
                }
                let prefix = if args[1] == "/" { String::new() } else { args[1].clone() };
                let perm = if rm { Value::Null } else { json!(args[3]) };
                c.post(&format!("/v1/repos/{repo}/acl"), json!({"prefix": prefix, "principal": args[2], "perm": perm}));
                println!(
                    "{} {} for {} on /{}",
                    if rm { "removed" } else { "set" },
                    if rm { "rule" } else { args[3].as_str() },
                    args[2],
                    prefix
                );
            }
            _ => {
                let v = c.get(&format!("/v1/repos/{repo}/acl"), &[]);
                let rules = v.as_array().unwrap();
                if rules.is_empty() {
                    println!("(no rules: every signed-in user may read and write)");
                }
                for r in rules {
                    println!(
                        "/{:<30} {:<24} {}",
                        r["prefix"].as_str().unwrap(),
                        r["principal"].as_str().unwrap(),
                        r["perm"].as_str().unwrap()
                    );
                }
            }
        },
        "sparse" => match args.first().map(String::as_str) {
            Some("add") => {
                let p = args.get(1).cloned().unwrap_or_else(|| die("usage: cmb sparse add <prefix>"));
                if s.sparse.is_empty() {
                    die("working copy is already full (not sparse)");
                }
                s.sparse.push(p);
                let h = s.head.clone();
                materialise(c, root, s, &h);
            }
            _ => {
                if s.sparse.is_empty() {
                    println!("(full checkout)");
                }
                for p in &s.sparse {
                    println!("{p}");
                }
            }
        },
        _ => die(format!("unknown command `{cmd}` (try `cmb help`)")),
    }
}

/// One shared HTTP agent. `CAMBIUM_CA=/path/ca.pem` adds an internal CA bundle on top
/// of the public roots (for servers using a private PKI).
fn http() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| {
        let mut b = ureq::AgentBuilder::new();
        if let Ok(ca) = std::env::var("CAMBIUM_CA") {
            let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let pem = std::fs::read(&ca).unwrap_or_else(|e| die(format!("CAMBIUM_CA {ca}: {e}")));
            for c in <rustls_pki_types::CertificateDer as rustls_pki_types::pem::PemObject>::pem_slice_iter(&pem) {
                let c = c.unwrap_or_else(|e| die(format!("CAMBIUM_CA {ca}: {e}")));
                roots.add(c).unwrap_or_else(|e| die(format!("CAMBIUM_CA {ca}: {e}")));
            }
            let cfg = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            b = b.tls_config(std::sync::Arc::new(cfg));
        }
        b.build()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wc_path_refuses_escapes() {
        let d = tempfile_dir();
        let root = d.as_path();
        assert!(wc_path(root, "a/b.txt").is_ok());
        assert!(wc_path(root, "../evil").is_err());
        assert!(wc_path(root, "/etc/passwd").is_err());
        assert!(wc_path(root, ".cambium/state.json").is_err());
        #[cfg(unix)]
        {
            let outside = tempfile_dir();
            std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
            assert!(wc_path(root, "link/authorized_keys").is_err(), "must not write through a symlinked folder");
            std::fs::write(root.join("f"), "x").unwrap();
            assert!(wc_path(root, "f/x").is_err());
            let _ = std::fs::remove_dir_all(outside);
        }
        let _ = std::fs::remove_dir_all(root);
    }
    fn tempfile_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("cmb-test-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
