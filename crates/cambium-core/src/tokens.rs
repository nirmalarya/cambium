//! Static API tokens and review-UI sessions.
//!
//! Only SHA-256 hashes are stored. Tokens may carry an expiry (`expires_at`),
//! can be rotated (new token, old ones kept valid for a grace period) and
//! revoked. UI sessions are random server-side ids with a fixed lifetime, so a
//! browser cookie never holds an API credential.
use crate::blob::hash_bytes;
use crate::engine::{Engine, Error, Principal, Result};
use crate::p;
use serde_json::{json, Value};

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}
fn random(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}{}", hex::encode(b))
}
fn groups_of(s: &str) -> Vec<String> {
    s.split(',').filter(|g| !g.is_empty()).map(String::from).collect()
}

/// "90d", "12h", "30m", "45s", "0" (never) -> seconds (None = never expires).
pub fn parse_ttl(s: &str) -> std::result::Result<Option<i64>, String> {
    let s = s.trim();
    if s == "0" || s.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().map_err(|_| format!("bad duration {s:?} (use e.g. 90d, 12h, 30m)"))?;
    let mul = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("bad duration unit in {s:?}")),
    };
    if n <= 0 {
        return Err("duration must be positive".into());
    }
    Ok(Some(n * mul))
}

impl Engine {
    pub fn create_token(&self, user: &str) -> Result<String> {
        self.create_token_with_groups(user, &[])
    }
    pub fn create_token_with_groups(&self, user: &str, groups: &[String]) -> Result<String> {
        self.create_token_full(user, groups, None, "")
    }
    /// Mint a token. `ttl` in seconds (None = no expiry).
    pub fn create_token_full(&self, user: &str, groups: &[String], ttl: Option<i64>, label: &str) -> Result<String> {
        if user.is_empty() {
            return Err(Error::Bad("user name required".into()));
        }
        let tok = random("cmbt_");
        self.db.exec(
            "INSERT INTO tokens(token_hash,user_name,groups,created_at,expires_at,label) VALUES(?1,?2,?3,?4,?5,?6)",
            p![hash_bytes(tok.as_bytes()), user, groups.join(","), now(), ttl.map(|t| now() + t), label],
        )?;
        Ok(tok)
    }
    pub fn verify_principal(&self, tok: &str) -> Result<Principal> {
        let r = self
            .db
            .one(
                "SELECT user_name,groups FROM tokens WHERE token_hash=?1 AND revoked=0 AND (expires_at IS NULL OR expires_at>?2)",
                p![hash_bytes(tok.as_bytes()), now()],
            )?
            .ok_or(Error::Unauthorized)?;
        Ok(Principal { user: r.s(0), groups: groups_of(&r.s(1)) })
    }
    /// New token for `user` with the groups of their newest live token; every
    /// other live token of theirs now expires after `grace` seconds.
    pub fn rotate_token(&self, user: &str, ttl: Option<i64>, grace: i64) -> Result<String> {
        let t = now();
        let cur = self.db.one("SELECT groups,label FROM tokens WHERE user_name=?1 AND revoked=0 AND (expires_at IS NULL OR expires_at>?2) ORDER BY created_at DESC LIMIT 1", p![user, t])?
            .ok_or_else(|| Error::NotFound(format!("no live token for {user}")))?;
        let cutoff = t + grace.max(0);
        self.db.exec(
            "UPDATE tokens SET expires_at=?1 WHERE user_name=?2 AND revoked=0 AND (expires_at IS NULL OR expires_at>?1)",
            p![cutoff, user],
        )?;
        self.create_token_full(user, &groups_of(&cur.s(0)), ttl, &cur.s(1))
    }
    /// Revoke every token of `user`; returns how many.
    pub fn revoke_tokens(&self, user: &str) -> Result<u64> {
        self.db.exec("UPDATE tokens SET revoked=1 WHERE user_name=?1 AND revoked=0", p![user])
    }
    /// Token metadata (never the secrets).
    pub fn list_tokens(&self) -> Result<Vec<Value>> {
        let t = now();
        Ok(self.db.query("SELECT user_name,groups,label,created_at,expires_at,revoked,substr(token_hash,1,12) FROM tokens ORDER BY created_at", p![])?
            .into_iter().map(|r| {
                let exp = r.oi(4);
                let state = if r.i(5) != 0 { "revoked" } else if exp.is_some_and(|e| e <= t) { "expired" } else { "active" };
                json!({"user": r.s(0), "groups": groups_of(&r.s(1)), "label": r.s(2), "created_at": r.i(3), "expires_at": exp, "state": state, "id": r.s(6)})
            }).collect())
    }

    // ---------- UI sessions ----------
    pub fn create_session(&self, who: &Principal, ttl: i64) -> Result<String> {
        let id = random("bvs_");
        let t = now();
        self.db.exec("DELETE FROM ui_sessions WHERE expires_at<=?1", p![t])?;
        self.db.exec(
            "INSERT INTO ui_sessions(id_hash,user_name,groups,created_at,expires_at) VALUES(?1,?2,?3,?4,?5)",
            p![hash_bytes(id.as_bytes()), &who.user, who.groups.join(","), t, t + ttl],
        )?;
        Ok(id)
    }
    pub fn session_principal(&self, id: &str) -> Result<Principal> {
        let r = self
            .db
            .one("SELECT user_name,groups FROM ui_sessions WHERE id_hash=?1 AND expires_at>?2", p![hash_bytes(id.as_bytes()), now()])?
            .ok_or(Error::Unauthorized)?;
        Ok(Principal { user: r.s(0), groups: groups_of(&r.s(1)) })
    }
    pub fn end_session(&self, id: &str) -> Result<()> {
        self.db.exec("DELETE FROM ui_sessions WHERE id_hash=?1", p![hash_bytes(id.as_bytes())])?;
        Ok(())
    }
}

#[cfg(test)]
mod t {
    #[test]
    fn ttl() {
        assert_eq!(super::parse_ttl("90d"), Ok(Some(90 * 86400)));
        assert_eq!(super::parse_ttl("0"), Ok(None));
        assert!(super::parse_ttl("5y").is_err() && super::parse_ttl("-3h").is_err());
    }
}
