//! Derived data: one generic mechanism for everything computed from the
//! commit graph. A derived type says how to compute its value for a changeset from the
//! changeset itself and the values already derived for its parents; the engine stores
//! each value once (keyed by type name and version, so a new version recomputes
//! cleanly), computes missing ancestors first without recursion, and runs eager types
//! when a changeset is created and deferred types on first use.
//!
//! Types today: `Skip` (commit-graph skip lists). Root trees, generation numbers and
//! the path-history index are also derived per changeset at creation and stored in
//! their own columns/tables for query speed; blame is derived per (changeset, path).
use crate::engine::{Changeset, Engine, Result};
use serde::{de::DeserializeOwned, Serialize};

pub trait Derive {
    const NAME: &'static str;
    /// Bump to recompute every value (old values are simply no longer read).
    const VERSION: u32;
    /// Compute when a changeset is created (true) or on first use (false).
    const EAGER: bool;
    type Value: Serialize + DeserializeOwned + Clone;
    /// Value for `cs`, given the values already derived for each of its parents.
    fn compute(e: &Engine, repo: &str, cs: &Changeset, parents: &[Self::Value]) -> Result<Self::Value>;
}

/// Commit-graph skip list: `ancestors[k]` is the changeset 2^k steps back along the
/// first-parent chain (as far as it goes). Lets ancestry checks jump through long
/// linear histories in O(log n) steps.
pub struct Skip;
impl Derive for Skip {
    const NAME: &'static str = "skip";
    const VERSION: u32 = 1;
    const EAGER: bool = true;
    type Value = Vec<String>;
    fn compute(e: &Engine, repo: &str, cs: &Changeset, parents: &[Vec<String>]) -> Result<Vec<String>> {
        let Some(p1) = cs.parents.first() else { return Ok(vec![]) };
        let _ = parents;
        let mut out = vec![p1.clone()];
        // out[k] = out[k-1]'s own (k-1)-th entry
        loop {
            let k = out.len();
            let prev = e.derive::<Skip>(repo, &out[k - 1])?;
            match prev.get(k - 1) {
                Some(x) => out.push(x.clone()),
                None => break,
            }
        }
        Ok(out)
    }
}

impl Engine {
    fn derived_key<D: Derive>() -> String {
        format!("{}@{}", D::NAME, D::VERSION)
    }
    fn derived_load<D: Derive>(&self, cs: &str) -> Result<Option<D::Value>> {
        let r = self.db.one("SELECT value FROM derived WHERE kind=?1 AND cs_id=?2 AND key=''", crate::p![Self::derived_key::<D>(), cs])?;
        Ok(r.and_then(|r| serde_json::from_str(&r.s(0)).ok()))
    }
    /// Value of derived type `D` for changeset `cs`, computing (and storing) it and any
    /// missing ancestors first. Iterative: deep histories cannot overflow the stack.
    pub fn derive<D: Derive>(&self, repo: &str, cs: &str) -> Result<D::Value> {
        if let Some(v) = self.derived_load::<D>(cs)? {
            return Ok(v);
        }
        let mut stack = vec![cs.to_string()];
        let mut done: std::collections::HashMap<String, D::Value> = Default::default();
        while let Some(top) = stack.last().cloned() {
            if done.contains_key(&top) {
                stack.pop();
                continue;
            }
            let ch = self.get_changeset(repo, &top)?;
            let mut missing = vec![];
            let mut vals = vec![];
            for p in &ch.parents {
                match done.get(p).cloned().map(Some).map_or_else(|| self.derived_load::<D>(p), Ok)? {
                    Some(v) => vals.push(v),
                    None => missing.push(p.clone()),
                }
            }
            if !missing.is_empty() {
                stack.extend(missing);
                continue;
            }
            let v = D::compute(self, repo, &ch, &vals)?;
            self.db.exec(
                "INSERT INTO derived(kind,cs_id,key,value) VALUES(?1,?2,'',?3) ON CONFLICT(kind,cs_id,key) DO NOTHING",
                crate::p![Self::derived_key::<D>(), &top, serde_json::to_string(&v).unwrap()],
            )?;
            done.insert(top, v);
            stack.pop();
        }
        Ok(done.remove(cs).expect("derived"))
    }
    /// Run every eager derived type for a new changeset.
    pub fn derive_eager(&self, repo: &str, cs: &str) -> Result<()> {
        if Skip::EAGER {
            self.derive::<Skip>(repo, cs)?;
        }
        Ok(())
    }
    /// Backfill eager derived types for every changeset (oldest first). Returns how
    /// many changesets were visited.
    pub fn backfill_derived(&self) -> Result<usize> {
        let rows = self.db.query("SELECT repo,id FROM changesets ORDER BY seq", crate::p![])?;
        for r in &rows {
            self.derive_eager(&r.s(0), &r.s(1))?;
        }
        Ok(rows.len())
    }
    /// Fast path for ancestry along the first-parent chain: jump down from `d` with the
    /// skip list while staying at or above gen(a). Some(true) if `a` is reached; None
    /// if the chain cannot decide (merges), so the caller falls back to a full walk.
    pub(crate) fn first_parent_reaches(&self, repo: &str, a: &str, d: &str, ga: i64) -> Result<Option<bool>> {
        let mut cur = d.to_string();
        loop {
            if cur == a {
                return Ok(Some(true));
            }
            if self.gen(repo, &cur)? <= ga {
                return Ok(None);
            }
            let skip = self.derive::<Skip>(repo, &cur)?;
            // furthest jump that does not go below a's generation
            let mut next = None;
            for x in skip.iter().rev() {
                if self.gen(repo, x)? >= ga {
                    next = Some(x.clone());
                    break;
                }
            }
            match next {
                Some(n) => cur = n,
                None => return Ok(None),
            }
        }
    }
}
