//! Server-side building blocks for automated pipelines built on the
//! history APIs: revert a changeset on top of the current
//! head, and stateless bisection along first-parent history using skip lists.
use crate::derived::Skip;
use crate::engine::{Engine, Error, FileChange, Result};
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub struct BisectStep {
    /// set when the search is finished: the first changeset that is bad
    pub first_bad: Option<String>,
    /// otherwise the changeset to test next (about halfway between good and bad)
    pub next: Option<String>,
    /// generation span still to search (an upper bound on the steps left)
    pub span: i64,
}

impl Engine {
    /// Furthest ancestor of `cs` along the first-parent chain whose generation is
    /// still >= `min_gen` (greedy skip-list jumps: O(log^2 n) lookups).
    pub fn first_parent_floor(&self, repo: &str, cs: &str, min_gen: i64) -> Result<String> {
        let mut cur = cs.to_string();
        loop {
            let skip = self.derive::<Skip>(repo, &cur)?;
            let mut jumped = false;
            for x in skip.iter().rev() {
                if self.gen(repo, x)? >= min_gen {
                    cur = x.clone();
                    jumped = true;
                    break;
                }
            }
            if !jumped {
                return Ok(cur);
            }
        }
    }

    /// One bisection step between `good` (an earlier, working changeset) and `bad`
    /// (a later, broken one) on bad's first-parent history. Stateless: call again with
    /// the result of testing `next` as the new good or bad.
    pub fn bisect(&self, repo: &str, good: &str, bad: &str) -> Result<BisectStep> {
        let (good, bad) = (self.resolve(repo, good)?, self.resolve(repo, bad)?);
        let (gg, gb) = (self.gen(repo, &good)?, self.gen(repo, &bad)?);
        if good == bad || gg >= gb || self.first_parent_floor(repo, &bad, gg)? != good {
            return Err(Error::Bad("good must be an earlier changeset on bad's first-parent history".into()));
        }
        let p1 = self.get_changeset(repo, &bad)?.parents.first().cloned();
        if p1.as_deref() == Some(good.as_str()) {
            return Ok(BisectStep { first_bad: Some(bad), next: None, span: 0 });
        }
        let mid = self.first_parent_floor(repo, &bad, gg + (gb - gg + 1) / 2)?;
        let next = if mid == bad { p1.expect("bad has a first parent") } else { mid };
        Ok(BisectStep { first_bad: None, next: Some(next), span: gb - gg })
    }

    /// The file changes that undo changeset `cs` on top of `onto` (three-way merge of
    /// cs's inverse). Err(Conflict) if later work touched the same lines.
    pub fn revert_changes(&self, repo: &str, cs: &str, onto: &str) -> Result<Vec<FileChange>> {
        let ch = self.get_changeset(repo, cs)?;
        let parent = ch.parents.first().ok_or_else(|| Error::Bad("cannot revert the root changeset".into()))?;
        if ch.parents.len() > 1 {
            return Err(Error::Bad("reverting a merge is not supported".into()));
        }
        let (base, theirs, ours) = (self.manifest(repo, cs)?, self.manifest(repo, parent)?, self.manifest(repo, onto)?);
        let merged = self.merge3_manifests(&base, &ours, &theirs)?.map_err(Error::Conflict)?;
        Ok(Self::changes_between(&ours, &merged))
    }

    /// Revert `cs` and land the result on `target` (lock-free, like any landing).
    pub fn revert(&self, repo: &str, cs: &str, target: &str, author: &str, check: &dyn Fn(&[FileChange]) -> Result<()>) -> Result<String> {
        let cs = self.resolve(repo, cs)?;
        for _ in 0..16 {
            let head = self.get_ref(repo, target)?.ok_or_else(|| Error::NotFound(format!("ref {target}")))?;
            let files = self.revert_changes(repo, &cs, &head)?;
            if files.is_empty() {
                return Err(Error::Bad(format!("{} has no changes left to revert on {target}", &cs[..12])));
            }
            check(&files)?;
            let msg = format!("Revert {}", &cs[..12]);
            let r = self.create_changeset(repo, vec![head.clone()], author, &msg, files)?;
            match self.cas_ref(repo, target, Some(&head), Some(&r.id)) {
                Ok(()) => return Ok(r.id),
                Err(Error::RefMoved { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::Busy(format!("{target} kept moving; retry")))
    }
}
