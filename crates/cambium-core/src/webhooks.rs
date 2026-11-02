//! Webhooks: tell CI when a ref moves (post-merge testing needs to hear about every
//! landing on main and release branches). Events are read from the write-ahead log
//! with one cursor per hook, so delivery is ordered, at-least-once, and survives
//! restarts; a short lease lets exactly one cmbd node deliver a given hook.
use crate::engine::{Engine, Error, Perm, Principal, Result};
use crate::p;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Serialize, Clone, Debug)]
pub struct Hook {
    pub id: i64,
    pub repo: String,
    pub url: String,
    pub created_by: String,
    pub created_at: i64,
    /// WAL position of the last delivered event
    pub last_seq: i64,
    pub failures: i64,
    pub last_error: Option<String>,
}

/// HMAC-SHA256 (RFC 2104), hex. Receivers check `X-Cambium-Signature: sha256=<this>`.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut k = if key.len() > 64 { Sha256::digest(key).to_vec() } else { key.to_vec() };
    k.resize(64, 0);
    let (mut ipad, mut opad) = (vec![0x36u8; 64], vec![0x5cu8; 64]);
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new().chain_update(&ipad).chain_update(msg).finalize();
    hex::encode(Sha256::new().chain_update(&opad).chain_update(inner).finalize())
}

/// A hook this node may deliver now: (id, repo, url, secret, last delivered seq).
pub type Claimed = (i64, String, String, String, i64);

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

impl Engine {
    fn require_admin(&self, repo: &str, who: &Principal) -> Result<()> {
        let az = self.authz(repo, who)?;
        let owner = self.db.one("SELECT owner FROM repos WHERE name=?1", p![repo])?.and_then(|r| r.os(0));
        let ok = if az.open() { owner.as_deref() == Some(who.user.as_str()) } else { az.can("", Perm::Admin) };
        if ok {
            Ok(())
        } else {
            Err(Error::Forbidden("admin on / (or the repo owner) to manage webhooks".into()))
        }
    }
    /// Register a hook; it receives events from now on.
    pub fn add_hook(&self, repo: &str, who: &Principal, url: &str, secret: &str) -> Result<i64> {
        self.require_admin(repo, who)?;
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() > 2000 {
            return Err(Error::Bad("webhook url must be http(s)://...".into()));
        }
        if secret.len() < 16 {
            return Err(Error::Bad("webhook secret must be at least 16 characters".into()));
        }
        let head = self.db.one("SELECT max(seq) FROM wal WHERE repo=?1", p![repo])?.and_then(|r| r.oi(0)).unwrap_or(0);
        Ok(self
            .db
            .one(
                "INSERT INTO webhooks(repo,url,secret,created_by,created_at,last_seq) VALUES(?1,?2,?3,?4,?5,?6) RETURNING id",
                p![repo, url, secret, &who.user, now(), head],
            )?
            .unwrap()
            .i(0))
    }
    pub fn list_hooks(&self, repo: &str, who: &Principal) -> Result<Vec<Hook>> {
        self.require_admin(repo, who)?;
        Ok(self
            .db
            .query(
                "SELECT id,repo,url,created_by,created_at,last_seq,failures,last_error FROM webhooks WHERE repo=?1 ORDER BY id",
                p![repo],
            )?
            .into_iter()
            .map(|r| Hook {
                id: r.i(0),
                repo: r.s(1),
                url: r.s(2),
                created_by: r.s(3),
                created_at: r.i(4),
                last_seq: r.i(5),
                failures: r.i(6),
                last_error: r.os(7),
            })
            .collect())
    }
    pub fn remove_hook(&self, repo: &str, who: &Principal, id: i64) -> Result<()> {
        self.require_admin(repo, who)?;
        if self.db.exec("DELETE FROM webhooks WHERE repo=?1 AND id=?2", p![repo, id])? == 0 {
            return Err(Error::NotFound(format!("webhook {id}")));
        }
        Ok(())
    }
    /// Hooks due for delivery that this node now holds the lease for.
    pub fn claim_hooks(&self, node: &str, lease_s: i64) -> Result<Vec<Claimed>> {
        let t = now();
        let due = self.db.query(
            "SELECT id FROM webhooks WHERE next_attempt<=?1 AND (lease_until<?1 OR lease_owner=?2) AND EXISTS (SELECT 1 FROM wal w WHERE w.repo=webhooks.repo AND w.seq>webhooks.last_seq AND w.kind='ref')",
            p![t, node],
        )?;
        let mut out = vec![];
        for r in due {
            let id = r.i(0);
            let won = self.db.exec(
                "UPDATE webhooks SET lease_owner=?1, lease_until=?2 WHERE id=?3 AND (lease_until<?4 OR lease_owner=?1)",
                p![node, t + lease_s, id, t],
            )?;
            if won == 1 {
                if let Some(h) = self.db.one("SELECT id,repo,url,secret,last_seq FROM webhooks WHERE id=?1", p![id])? {
                    out.push((h.i(0), h.s(1), h.s(2), h.s(3), h.i(4)));
                }
            }
        }
        Ok(out)
    }
    /// Next ref event after `after` for `repo`, as (seq, payload).
    pub fn next_ref_event(&self, repo: &str, after: i64) -> Result<Option<(i64, Value)>> {
        let Some(r) = self
            .db
            .one("SELECT seq,payload,created_at FROM wal WHERE repo=?1 AND seq>?2 AND kind='ref' ORDER BY seq LIMIT 1", p![repo, after])?
        else {
            return Ok(None);
        };
        let (seq, ev, ts) = (r.i(0), serde_json::from_str::<Value>(&r.s(1)).unwrap_or(Value::Null), r.i(2));
        let (from, to) = (ev["from"].as_str().map(String::from), ev["to"].as_str().map(String::from));
        // the changesets this move brought in (first-parent, newest first, at most 50)
        let mut changesets = vec![];
        if let Some(to) = &to {
            for c in self.log(repo, to, None, 50)? {
                if from.as_deref() == Some(c.id.as_str())
                    || from.as_ref().is_some_and(|f| self.is_ancestor(repo, &c.id, f).unwrap_or(false))
                {
                    break;
                }
                changesets.push(json!({"id": c.id, "author": c.author, "message": c.message, "created_at": c.created_at}));
            }
        }
        Ok(Some((
            seq,
            json!({"event": "ref", "repo": repo, "ref": ev["ref"], "before": from, "after": to, "seq": seq, "at": ts, "changesets": changesets}),
        )))
    }
    pub fn hook_delivered(&self, id: i64, seq: i64) -> Result<()> {
        self.db.exec(
            "UPDATE webhooks SET last_seq=?1, failures=0, next_attempt=0, last_error=NULL WHERE id=?2 AND last_seq<?1",
            p![seq, id],
        )?;
        Ok(())
    }
    /// Delivery failed: back off exponentially (5 s .. 1 h), keep the event.
    pub fn hook_failed(&self, id: i64, err: &str) -> Result<()> {
        let f = self.db.one("SELECT failures FROM webhooks WHERE id=?1", p![id])?.map_or(0, |r| r.i(0)) + 1;
        let wait = (5i64 << f.min(10)).min(3600);
        let msg: String = err.chars().take(500).collect();
        self.db.exec("UPDATE webhooks SET failures=?1, next_attempt=?2, last_error=?3 WHERE id=?4", p![f, now() + wait, msg, id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            super::hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
