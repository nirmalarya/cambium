//! Logical backup and restore of the metadata store plus every referenced blob.
//!
//! The backup is a directory: one JSON-lines file per table (read inside a single
//! snapshot transaction), `blobs/` with the raw content of every blob the metadata
//! points at (each verified against its content hash on the way out), and
//! `manifest.json` with row counts and SHA-256 sums. The format is the same for
//! SQLite and Postgres, so a backup taken from one restores into the other.
//! Caches (derived, search index, sessions, idempotency keys) are not copied;
//! they are rebuilt on demand.
use crate::blob::hash_bytes;
use crate::db::Val;
use crate::engine::{Engine, Error, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

/// (table, columns, order) in an order that satisfies foreign keys on restore.
pub const TABLES: &[(&str, &str, &str)] = &[
    ("tokens", "token_hash,user_name,groups,revoked,created_at,expires_at,label", "created_at,token_hash"),
    ("repos", "name,owner,created_at", "name"),
    ("acl", "repo,prefix,principal,perm", "repo,prefix,principal"),
    ("wal", "seq,repo,kind,payload,created_at", "seq"),
    ("changesets", "id,repo,seq,parent1,parent2,author,message,created_at,gen,root_tree", "seq"),
    ("changeset_files", "cs_id,path,blob,mode", "cs_id,path"),
    ("refs", "repo,name,cs_id,version", "repo,name"),
    ("path_history", "repo,dir,gen,cs_id", "repo,dir,cs_id"),
    ("reviews", "id,repo,source,target,title,author,state,created_at", "id"),
    ("review_comments", "id,review_id,author,path,body,created_at", "id"),
    ("review_approvals", "review_id,user_name,head,created_at,groups", "review_id,user_name"),
    ("protected_refs", "repo,name,required_approvals", "repo,name"),
    ("blob_meta", "blob,size", "blob"),
    ("blob_uploads", "blob,principal,created_at", "blob,principal"),
    ("trees", "id,body", "id"),
    ("tree_paths", "tree_id,repo,path", "tree_id,repo,path"),
    ("audit_log", "seq,ts,actor,action,repo,target,outcome,detail,request_id,prev_hash,hash", "seq"),
];
const SERIALS: &[(&str, &str)] = &[("wal", "seq"), ("reviews", "id"), ("review_comments", "id"), ("audit_log", "seq")];

fn to_json(v: &Val) -> Value {
    match v {
        Val::Null => Value::Null,
        Val::Int(i) => json!(i),
        Val::Text(s) => json!(s),
    }
}
fn from_json(v: &Value) -> Val {
    match v {
        Value::Null => Val::Null,
        Value::Number(n) => Val::Int(n.as_i64().unwrap_or(0)),
        Value::String(s) => Val::Text(s.clone()),
        o => Val::Text(o.to_string()),
    }
}
fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[derive(Debug, Default)]
pub struct Report {
    pub rows: i64,
    pub blobs: i64,
    pub bytes: i64,
    pub schema_version: i64,
}

impl Engine {
    pub fn backup(&self, dir: &Path) -> Result<Report> {
        if dir.exists() && std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(false) {
            return Err(Error::Bad(format!("{} is not empty", dir.display())));
        }
        std::fs::create_dir_all(dir.join("blobs"))?;
        let d = dir.to_path_buf();
        let pg = self.db.is_pg();
        // one snapshot for every table, so the copy is consistent while writers continue
        // (on SQLite this runs on its own read-only connection and does not block requests)
        let (tables, version) = self.db.snapshot_tx(move |c| {
            if pg {
                c.batch("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")?;
            }
            let version = c.one("SELECT max(version) FROM schema_migrations", &[])?.map_or(1, |r| r.i(0));
            let mut out = serde_json::Map::new();
            for (t, cols, order) in TABLES {
                let rows = c.query(&format!("SELECT {cols} FROM {t} ORDER BY {order}"), &[])?;
                let mut f = std::io::BufWriter::new(std::fs::File::create(d.join(format!("{t}.jsonl")))?);
                for r in &rows {
                    writeln!(f, "{}", Value::Array(r.0.iter().map(to_json).collect()))?;
                }
                f.flush()?;
                out.insert(t.to_string(), json!({"rows": rows.len()}));
            }
            Ok((out, version))
        })?;
        let mut rep = Report { schema_version: version, ..Default::default() };
        let mut tables = tables;
        for (t, _, _) in TABLES {
            let data = std::fs::read(dir.join(format!("{t}.jsonl")))?;
            let e = tables.get_mut(*t).unwrap();
            rep.rows += e["rows"].as_i64().unwrap_or(0);
            e["sha256"] = json!(hash_bytes(&data));
        }
        // blobs referenced by the snapshot (content-addressed and never deleted, so
        // anything the snapshot points at is still present)
        let mut ids = std::collections::BTreeSet::new();
        for t in ["changeset_files", "blob_meta"] {
            let col = if t == "blob_meta" { 0 } else { 2 };
            for line in std::io::BufReader::new(std::fs::File::open(dir.join(format!("{t}.jsonl")))?).lines() {
                let v: Value = serde_json::from_str(&line?).map_err(|e| Error::Db(e.to_string()))?;
                if let Some(s) = v[col].as_str() {
                    ids.insert(s.to_string());
                }
            }
        }
        for id in &ids {
            let data = self.blobs.get(id)?.ok_or_else(|| Error::Db(format!("blob {id} is referenced but missing from the blob store")))?;
            if hash_bytes(&data) != *id {
                return Err(Error::Db(format!("blob {id} is corrupt (hash mismatch)")));
            }
            let p = dir.join("blobs").join(&id[..2]);
            std::fs::create_dir_all(&p)?;
            std::fs::write(p.join(id), &data)?;
            rep.blobs += 1;
            rep.bytes += data.len() as i64;
        }
        let manifest = json!({"format": 1, "product": "cambium", "created_at": now(), "schema_version": version,
            "source": if pg { "postgres" } else { "sqlite" }, "tables": tables, "blobs": {"count": rep.blobs, "bytes": rep.bytes}});
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap())?;
        Ok(rep)
    }

    /// Restore into an empty metadata store and blob store. Checks every file
    /// sum and blob hash first; nothing is written if any check fails.
    pub fn restore(&self, dir: &Path) -> Result<Report> {
        let m: Value =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?).map_err(|e| Error::Bad(format!("manifest: {e}")))?;
        if m["format"] != json!(1) {
            return Err(Error::Bad("unknown backup format".into()));
        }
        let have = self.db.one("SELECT max(version) FROM schema_migrations", vec![])?.map_or(1, |r| r.i(0));
        if m["schema_version"].as_i64().unwrap_or(0) > have {
            return Err(Error::Bad(format!(
                "backup is schema v{} but this cmbd is v{have}; restore with a newer cmbd",
                m["schema_version"]
            )));
        }
        if self.db.one("SELECT count(*) FROM repos", vec![])?.map_or(0, |r| r.i(0)) > 0
            || self.db.one("SELECT count(*) FROM audit_log", vec![])?.map_or(0, |r| r.i(0)) > 0
        {
            return Err(Error::Bad("target database is not empty; restore only into a fresh database".into()));
        }
        for (t, _, _) in TABLES {
            let data = std::fs::read(dir.join(format!("{t}.jsonl")))?;
            if m["tables"][t]["sha256"].as_str() != Some(hash_bytes(&data).as_str()) {
                return Err(Error::Bad(format!("{t}.jsonl checksum mismatch")));
            }
        }
        let mut rep = Report { schema_version: have, ..Default::default() };
        // blobs first: metadata must never point at content that is not there
        for sub in std::fs::read_dir(dir.join("blobs"))? {
            for f in std::fs::read_dir(sub?.path())? {
                let f = f?;
                let data = std::fs::read(f.path())?;
                let id = f.file_name().to_string_lossy().to_string();
                if hash_bytes(&data) != id {
                    return Err(Error::Bad(format!("backup blob {id} is corrupt")));
                }
                if self.blobs.put(&data)? != id {
                    return Err(Error::Db(format!("blob store returned a different id for {id}")));
                }
                rep.blobs += 1;
                rep.bytes += data.len() as i64;
            }
        }
        if m["blobs"]["count"].as_i64() != Some(rep.blobs) {
            return Err(Error::Bad(format!("expected {} blobs, found {}", m["blobs"]["count"], rep.blobs)));
        }
        let d = dir.to_path_buf();
        let pg = self.db.is_pg();
        rep.rows = self.db.tx(move |c| {
            let mut n = 0;
            for (t, cols, _) in TABLES {
                let k = cols.split(',').count();
                let ph: Vec<String> = (1..=k).map(|i| format!("?{i}")).collect();
                let sql = format!("INSERT INTO {t}({cols}) VALUES({})", ph.join(","));
                for line in std::io::BufReader::new(std::fs::File::open(d.join(format!("{t}.jsonl")))?).lines() {
                    let v: Value = serde_json::from_str(&line?).map_err(|e| Error::Db(e.to_string()))?;
                    let vals: Vec<Val> = v.as_array().ok_or_else(|| Error::Bad(format!("bad row in {t}")))?.iter().map(from_json).collect();
                    if vals.len() != k { return Err(Error::Bad(format!("{t}: expected {k} columns"))); }
                    c.exec(&sql, &vals)?;
                    n += 1;
                }
            }
            if pg {
                for (t, col) in SERIALS {
                    c.query(&format!("SELECT setval(pg_get_serial_sequence('{t}','{col}'), GREATEST((SELECT COALESCE(max({col}),0) FROM {t}),1), (SELECT count(*) FROM {t})>0)"), &[])?;
                }
            }
            Ok(n)
        })?;
        Ok(rep)
    }
}
