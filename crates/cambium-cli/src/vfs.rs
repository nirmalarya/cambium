//! `cmb mount`: a lazily-populated view of a revision through FUSE.
//!
//! Read-only (default): the tree (paths, modes, sizes) comes from one server
//! call; file bytes are fetched on first read and kept in the shared cache.
//!
//! Writable (`--write`): a copy-on-write overlay. Unchanged files are still
//! served lazily from the server ("lower"); the first write to a file copies it
//! into a private upper directory, and new files, deletes, renames, chmod and
//! symlinks are all kept in the overlay. Every change is appended to the
//! working copy's `watch.dirty` journal, so `cmb status / add / commit` inside
//! the mount hash only the touched paths and commit through the normal API.
//! Nothing is written to the server until `cmb commit`.
use crate::Client;
use cambium_core::engine::{split_entry, MODE_EXEC, MODE_FILE, MODE_LINK};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, TimeOrNow,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOTEMPTY: i32 = 39;

#[derive(Clone)]
enum Data {
    Lower { blob: String, size: u64 },
    Upper { file: PathBuf, size: u64 },
    Mem(Vec<u8>),
}
enum Kind {
    Dir { children: BTreeMap<String, u64> },
    File { data: Data, mode: u32 },
    Gone,
}
struct Node {
    parent: u64,
    name: String,
    kind: Kind,
    mtime: SystemTime,
}

pub struct Vfs {
    client: Client,
    nodes: Vec<Node>,
    loaded: HashMap<u64, Vec<u8>>,
    pub fetched: usize,
    /// Some(upper dir, journal file) when writable
    overlay: Option<(PathBuf, PathBuf)>,
    uid: u32,
    gid: u32,
}

impl Vfs {
    /// Build the inode table from a tree listing. Inode 1 is the root.
    pub fn new(
        client: Client,
        entries: &BTreeMap<String, String>,
        sizes: &BTreeMap<String, u64>,
        overlay: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        let dir = |p: u64, n: &str| Node { parent: p, name: n.into(), kind: Kind::Dir { children: BTreeMap::new() }, mtime: UNIX_EPOCH };
        let mut nodes = vec![dir(0, ""), dir(1, "")];
        let mut dirs: HashMap<String, u64> = HashMap::from([(String::new(), 1)]);
        for (path, e) in entries {
            let mut parent = 1u64;
            let mut cur = String::new();
            let segs: Vec<&str> = path.split('/').collect();
            for (i, seg) in segs.iter().enumerate() {
                if !cur.is_empty() {
                    cur.push('/');
                }
                cur.push_str(seg);
                let ino = if i + 1 == segs.len() {
                    let (b, m) = split_entry(e);
                    nodes.push(Node {
                        parent,
                        name: seg.to_string(),
                        kind: Kind::File { data: Data::Lower { blob: b.into(), size: sizes.get(path).copied().unwrap_or(0) }, mode: m },
                        mtime: UNIX_EPOCH,
                    });
                    (nodes.len() - 1) as u64
                } else if let Some(&d) = dirs.get(&cur) {
                    d
                } else {
                    nodes.push(dir(parent, seg));
                    let d = (nodes.len() - 1) as u64;
                    dirs.insert(cur.clone(), d);
                    d
                };
                if let Kind::Dir { children } = &mut nodes[parent as usize].kind {
                    children.insert(seg.to_string(), ino);
                }
                parent = ino;
            }
        }
        Vfs {
            client,
            nodes,
            loaded: HashMap::new(),
            fetched: 0,
            overlay,
            uid: env_id("CAMBIUM_MOUNT_UID", "-u"),
            gid: env_id("CAMBIUM_MOUNT_GID", "-g"),
        }
    }
    fn writable(&self) -> bool {
        self.overlay.is_some()
    }
    fn ttl(&self) -> Duration {
        if self.writable() {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(3600)
        }
    }
    fn path_of(&self, ino: u64) -> String {
        let mut parts = vec![];
        let mut i = ino;
        while i > 1 {
            let n = &self.nodes[i as usize];
            parts.push(n.name.clone());
            i = n.parent;
        }
        parts.reverse();
        parts.join("/")
    }
    /// Record a changed path for `cmb status` (same journal as `cmb watch`).
    fn dirty(&self, ino: u64) {
        if let Some((_, j)) = &self.overlay {
            let p = self.path_of(ino);
            if p.is_empty() {
                return;
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(j) {
                let _ = writeln!(f, "{p}");
            }
        }
    }
    fn attr(&self, ino: u64) -> Option<FileAttr> {
        if ino == 0 {
            return None;
        }
        let n = self.nodes.get(ino as usize)?;
        let w = self.writable();
        let (kind, perm, size) = match &n.kind {
            Kind::Gone => return None,
            Kind::Dir { .. } => (FileType::Directory, if w { 0o755 } else { 0o555 }, 0),
            Kind::File { data, mode } => {
                let size = match data {
                    Data::Lower { size, .. } | Data::Upper { size, .. } => *size,
                    Data::Mem(v) => v.len() as u64,
                };
                if *mode == MODE_LINK {
                    (FileType::Symlink, 0o777, size)
                } else if *mode == MODE_EXEC {
                    (FileType::RegularFile, if w { 0o755 } else { 0o555 }, size)
                } else {
                    (FileType::RegularFile, if w { 0o644 } else { 0o444 }, size)
                }
            }
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: n.mtime,
            mtime: n.mtime,
            ctime: n.mtime,
            crtime: n.mtime,
            kind,
            perm,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn content(&mut self, ino: u64) -> Option<Vec<u8>> {
        let Kind::File { data, .. } = &self.nodes.get(ino as usize)?.kind else { return None };
        match data.clone() {
            Data::Mem(v) => Some(v),
            Data::Upper { file, .. } => std::fs::read(file).ok(),
            Data::Lower { blob, .. } => {
                if !self.loaded.contains_key(&ino) {
                    let d = self.client.get_blob(&blob);
                    self.fetched += 1;
                    self.loaded.insert(ino, d);
                }
                self.loaded.get(&ino).cloned()
            }
        }
    }
    /// Copy a lower file into the upper directory before its first change.
    fn copy_up(&mut self, ino: u64) -> Result<PathBuf, i32> {
        let Some((upper, _)) = self.overlay.clone() else { return Err(EROFS) };
        match self.nodes.get(ino as usize).map(|n| &n.kind) {
            Some(Kind::File { data: Data::Upper { file, .. }, .. }) => return Ok(file.clone()),
            Some(Kind::File { .. }) => {}
            Some(Kind::Dir { .. }) => return Err(EISDIR),
            _ => return Err(ENOENT),
        }
        let bytes = self.content(ino).ok_or(EIO)?;
        let file = upper.join(ino.to_string());
        std::fs::write(&file, &bytes).map_err(|_| EIO)?;
        self.loaded.remove(&ino);
        if let Kind::File { data, .. } = &mut self.nodes[ino as usize].kind {
            *data = Data::Upper { file: file.clone(), size: bytes.len() as u64 };
        }
        Ok(file)
    }
    fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        match &self.nodes.get(parent as usize)?.kind {
            Kind::Dir { children } => children.get(&*name.to_string_lossy()).copied(),
            _ => None,
        }
    }
    fn add(&mut self, parent: u64, name: &OsStr, kind: Kind) -> Result<u64, i32> {
        if !self.writable() {
            return Err(EROFS);
        }
        let name = name.to_string_lossy().to_string();
        if name == ".cambium" || name.contains('/') {
            return Err(EINVAL);
        }
        if !matches!(self.nodes.get(parent as usize).map(|n| &n.kind), Some(Kind::Dir { .. })) {
            return Err(ENOTDIR);
        }
        if self.child(parent, OsStr::new(&name)).is_some() {
            return Err(EEXIST);
        }
        self.nodes.push(Node { parent, name: name.clone(), kind, mtime: SystemTime::now() });
        let ino = (self.nodes.len() - 1) as u64;
        if let Kind::Dir { children } = &mut self.nodes[parent as usize].kind {
            children.insert(name, ino);
        }
        self.dirty(ino);
        Ok(ino)
    }
    fn remove(&mut self, parent: u64, name: &OsStr, want_dir: bool) -> Result<(), i32> {
        if !self.writable() {
            return Err(EROFS);
        }
        let ino = self.child(parent, name).ok_or(ENOENT)?;
        match &self.nodes[ino as usize].kind {
            Kind::Dir { children } if want_dir => {
                if !children.is_empty() {
                    return Err(ENOTEMPTY);
                }
            }
            Kind::Dir { .. } => return Err(EISDIR),
            _ if want_dir => return Err(ENOTDIR),
            _ => {}
        }
        self.dirty(ino);
        if let Kind::File { data: Data::Upper { file, .. }, .. } = &self.nodes[ino as usize].kind {
            let _ = std::fs::remove_file(file);
        }
        self.nodes[ino as usize].kind = Kind::Gone;
        if let Kind::Dir { children } = &mut self.nodes[parent as usize].kind {
            children.remove(&*name.to_string_lossy());
        }
        Ok(())
    }
    fn lookup_path(&self, p: &str) -> Option<u64> {
        let mut i = 1u64;
        for seg in p.split('/') {
            i = self.child(i, OsStr::new(seg))?;
        }
        Some(i)
    }
    /// Paths whose content lives in the overlay (changed or new since mount).
    pub fn upper_paths(&self) -> Vec<String> {
        (2..self.nodes.len() as u64)
            .filter(|i| matches!(&self.nodes[*i as usize].kind, Kind::File { data: Data::Upper { .. } | Data::Mem(_), .. }))
            .map(|i| self.path_of(i))
            .collect()
    }
}
fn env_id(var: &str, flag: &str) -> u32 {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        std::process::Command::new("id")
            .arg(flag)
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    })
}

impl Filesystem for Vfs {
    fn lookup(&mut self, _r: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let ttl = self.ttl();
        match self.child(parent, name).and_then(|i| self.attr(i)) {
            Some(a) => reply.entry(&ttl, &a, 0),
            None => reply.error(ENOENT),
        }
    }
    fn getattr(&mut self, _r: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let ttl = self.ttl();
        match self.attr(ino) {
            Some(a) => reply.attr(&ttl, &a),
            None => reply.error(ENOENT),
        }
    }
    fn setattr(
        &mut self,
        _r: &Request,
        ino: u64,
        mode: Option<u32>,
        _u: Option<u32>,
        _g: Option<u32>,
        size: Option<u64>,
        _a: Option<TimeOrNow>,
        _m: Option<TimeOrNow>,
        _c: Option<SystemTime>,
        _fh: Option<u64>,
        _cr: Option<SystemTime>,
        _ch: Option<SystemTime>,
        _b: Option<SystemTime>,
        _f: Option<u32>,
        reply: ReplyAttr,
    ) {
        if (mode.is_some() || size.is_some()) && !self.writable() {
            return reply.error(EROFS);
        }
        if let Some(sz) = size {
            let file = match self.copy_up(ino) {
                Ok(f) => f,
                Err(e) => return reply.error(e),
            };
            if std::fs::OpenOptions::new().write(true).open(&file).and_then(|f| f.set_len(sz)).is_err() {
                return reply.error(EIO);
            }
            if let Kind::File { data: Data::Upper { size, .. }, .. } = &mut self.nodes[ino as usize].kind {
                *size = sz;
            }
            self.nodes[ino as usize].mtime = SystemTime::now();
            self.dirty(ino);
        }
        if let Some(m) = mode {
            let mut changed = false;
            if let Kind::File { mode: fm, .. } = &mut self.nodes[ino as usize].kind {
                if *fm != MODE_LINK {
                    let new = if m & 0o111 != 0 { MODE_EXEC } else { MODE_FILE };
                    changed = *fm != new;
                    *fm = new;
                }
            }
            if changed {
                self.nodes[ino as usize].mtime = SystemTime::now();
                self.dirty(ino);
            }
        }
        let ttl = self.ttl();
        match self.attr(ino) {
            Some(a) => reply.attr(&ttl, &a),
            None => reply.error(ENOENT),
        }
    }
    fn readlink(&mut self, _r: &Request, ino: u64, reply: ReplyData) {
        match self.content(ino) {
            Some(d) => reply.data(&d),
            None => reply.error(ENOENT),
        }
    }
    fn open(&mut self, _r: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        if self.attr(ino).is_none() {
            return reply.error(ENOENT);
        }
        if flags & 3 != 0 {
            if !self.writable() {
                return reply.error(EROFS);
            }
            if let Err(e) = self.copy_up(ino) {
                return reply.error(e);
            }
            if flags & 0o1000 != 0 {
                // O_TRUNC
                if let Kind::File { data: Data::Upper { file, size }, .. } = &mut self.nodes[ino as usize].kind {
                    let _ = std::fs::write(file, b"");
                    *size = 0;
                }
                self.dirty(ino);
            }
        }
        reply.opened(0, 0)
    }
    fn read(&mut self, _r: &Request, ino: u64, _fh: u64, offset: i64, size: u32, _f: i32, _l: Option<u64>, reply: ReplyData) {
        if let Some(Node { kind: Kind::File { data: Data::Upper { file, .. }, .. }, .. }) = self.nodes.get(ino as usize) {
            let mut buf = vec![0u8; size as usize];
            let n = std::fs::File::open(file).and_then(|mut f| {
                f.seek(SeekFrom::Start(offset as u64))?;
                f.read(&mut buf)
            });
            return match n {
                Ok(n) => reply.data(&buf[..n]),
                Err(_) => reply.error(EIO),
            };
        }
        match self.content(ino) {
            Some(d) => {
                let s = (offset as usize).min(d.len());
                let e = (s + size as usize).min(d.len());
                reply.data(&d[s..e])
            }
            None => reply.error(ENOENT),
        }
    }
    fn write(&mut self, _r: &Request, ino: u64, _fh: u64, offset: i64, data: &[u8], _wf: u32, _f: i32, _l: Option<u64>, reply: ReplyWrite) {
        let file = match self.copy_up(ino) {
            Ok(f) => f,
            Err(e) => return reply.error(e),
        };
        let res = std::fs::OpenOptions::new().write(true).open(&file).and_then(|mut f| {
            f.seek(SeekFrom::Start(offset as u64))?;
            f.write_all(data)?;
            f.metadata()
        });
        match res {
            Ok(m) => {
                if let Kind::File { data: Data::Upper { size, .. }, .. } = &mut self.nodes[ino as usize].kind {
                    *size = m.len();
                }
                self.nodes[ino as usize].mtime = SystemTime::now();
                self.dirty(ino);
                reply.written(data.len() as u32)
            }
            Err(_) => reply.error(EIO),
        }
    }
    fn create(&mut self, _r: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some((upper, _)) = self.overlay.clone() else { return reply.error(EROFS) };
        let fm = if mode & 0o111 != 0 { MODE_EXEC } else { MODE_FILE };
        let ino = match self.add(parent, name, Kind::File { data: Data::Mem(vec![]), mode: fm }) {
            Ok(i) => i,
            Err(e) => return reply.error(e),
        };
        let file = upper.join(ino.to_string());
        if std::fs::write(&file, b"").is_err() {
            return reply.error(EIO);
        }
        self.nodes[ino as usize].kind = Kind::File { data: Data::Upper { file, size: 0 }, mode: fm };
        let ttl = self.ttl();
        reply.created(&ttl, &self.attr(ino).unwrap(), 0, 0, 0)
    }
    fn mkdir(&mut self, _r: &Request, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        match self.add(parent, name, Kind::Dir { children: BTreeMap::new() }) {
            Ok(i) => {
                let ttl = self.ttl();
                reply.entry(&ttl, &self.attr(i).unwrap(), 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn symlink(&mut self, _r: &Request, parent: u64, name: &OsStr, target: &Path, reply: ReplyEntry) {
        let t = target.to_string_lossy().as_bytes().to_vec();
        match self.add(parent, name, Kind::File { data: Data::Mem(t), mode: MODE_LINK }) {
            Ok(i) => {
                let ttl = self.ttl();
                reply.entry(&ttl, &self.attr(i).unwrap(), 0)
            }
            Err(e) => reply.error(e),
        }
    }
    fn unlink(&mut self, _r: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rmdir(&mut self, _r: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rename(&mut self, _r: &Request, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
        if !self.writable() {
            return reply.error(EROFS);
        }
        let Some(ino) = self.child(parent, name) else { return reply.error(ENOENT) };
        if newname.to_string_lossy() == ".cambium" {
            return reply.error(EINVAL);
        }
        if !matches!(self.nodes.get(newparent as usize).map(|n| &n.kind), Some(Kind::Dir { .. })) {
            return reply.error(ENOTDIR);
        }
        // refuse to move a directory into itself
        let mut p = newparent;
        while p > 1 {
            if p == ino {
                return reply.error(EINVAL);
            }
            p = self.nodes[p as usize].parent;
        }
        if let Some(old) = self.child(newparent, newname) {
            if old == ino {
                return reply.ok();
            }
            let is_dir = matches!(self.nodes[old as usize].kind, Kind::Dir { .. });
            if let Err(e) = self.remove(newparent, newname, is_dir) {
                return reply.error(e);
            }
        }
        self.dirty(ino); // old path
        if let Kind::Dir { children } = &mut self.nodes[parent as usize].kind {
            children.remove(&*name.to_string_lossy());
        }
        let nn = newname.to_string_lossy().to_string();
        if let Kind::Dir { children } = &mut self.nodes[newparent as usize].kind {
            children.insert(nn.clone(), ino);
        }
        let n = &mut self.nodes[ino as usize];
        n.parent = newparent;
        n.name = nn;
        n.mtime = SystemTime::now();
        self.dirty(ino); // new path
        reply.ok()
    }
    /// On unmount, copy out every overlay file that differs from the last commit.
    fn destroy(&mut self) {
        let Some((upper, _)) = self.overlay.clone() else { return };
        let meta = upper.parent().unwrap().to_path_buf();
        let committed: BTreeMap<String, String> = std::fs::read(meta.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| serde_json::from_value(v["files"].clone()).ok())
            .unwrap_or_default();
        for p in self.upper_paths() {
            let Some(ino) = self.lookup_path(&p) else { continue };
            let Some(data) = self.content(ino) else { continue };
            let same = committed.get(&p).map(|e| split_entry(e).0 == cambium_core::blob::hash_bytes(&data)).unwrap_or(false);
            if !same {
                let out = meta.join("salvage").join(&p);
                if let Some(d) = out.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(out, data);
            }
        }
        let _ = std::fs::remove_dir_all(upper);
    }
    fn flush(&mut self, _r: &Request, _ino: u64, _fh: u64, _l: u64, reply: ReplyEmpty) {
        reply.ok()
    }
    fn fsync(&mut self, _r: &Request, _ino: u64, _fh: u64, _d: bool, reply: ReplyEmpty) {
        reply.ok()
    }
    fn readdir(&mut self, _r: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(Node { parent, kind: Kind::Dir { children }, .. }) = self.nodes.get(ino as usize) else { return reply.error(ENOTDIR) };
        let mut list: Vec<(u64, FileType, String)> =
            vec![(ino, FileType::Directory, ".".into()), (if ino == 1 { 1 } else { *parent }, FileType::Directory, "..".into())];
        for (n, &c) in children {
            let k = match &self.nodes[c as usize].kind {
                Kind::Dir { .. } => FileType::Directory,
                Kind::File { mode, .. } if *mode == MODE_LINK => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            list.push((c, k, n.clone()));
        }
        for (i, (c, k, n)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(c, (i + 1) as i64, k, n) {
                break;
            }
        }
        reply.ok()
    }
}

/// Mount read-only and serve until unmounted (`fusermount3 -u <dir>`).
pub fn mount(client: Client, entries: BTreeMap<String, String>, sizes: BTreeMap<String, u64>, dir: &Path) -> std::io::Result<()> {
    let fs = Vfs::new(client, &entries, &sizes, None);
    eprintln!("mounted {} files read-only at {} (contents fetched on first read)", entries.len(), dir.display());
    fuser::mount2(fs, dir, &[MountOption::RO, MountOption::FSName("cambium".into()), MountOption::DefaultPermissions])
}
/// Mount writable (copy-on-write). `meta` is the working-copy metadata dir
/// (state.json, watch journal); `upper` holds changed file contents.
pub fn mount_rw(
    client: Client,
    entries: BTreeMap<String, String>,
    sizes: BTreeMap<String, u64>,
    dir: &Path,
    meta: &Path,
) -> std::io::Result<()> {
    let upper = meta.join("upper");
    std::fs::create_dir_all(&upper)?;
    let journal = meta.join("watch.dirty");
    let _ = std::fs::remove_file(&journal);
    // the overlay is its own change monitor: status trusts the journal while we run
    std::fs::write(meta.join("watch.baseline"), "")?;
    std::fs::write(meta.join("watch.pid"), std::process::id().to_string())?;
    let fs = Vfs::new(client, &entries, &sizes, Some((upper, journal)));
    eprintln!(
        "mounted {} files read-write (copy-on-write) at {}; run `cmb commit` inside it to save changes to the server",
        entries.len(),
        dir.display()
    );
    fuser::mount2(fs, dir, &[MountOption::RW, MountOption::FSName("cambium".into()), MountOption::DefaultPermissions])
}
