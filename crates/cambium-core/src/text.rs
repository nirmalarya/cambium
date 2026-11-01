//! Line diff, blame propagation, and three-way file merge.
use similar::{ChangeTag, TextDiff};

pub fn unified(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    TextDiff::from_lines(old, new).unified_diff().context_radius(3).header(&format!("a/{path}"), &format!("b/{path}")).to_string()
}

/// Given blame for `old` (one changeset id per line), produce blame for `new`
/// where inserted lines are attributed to `cs`.
pub fn carry_blame(old: &str, old_blame: &[String], new: &str, cs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let diff = TextDiff::from_lines(old, new);
    for ch in diff.iter_all_changes() {
        match ch.tag() {
            ChangeTag::Equal => {
                let i = ch.old_index().unwrap();
                out.push(old_blame.get(i).cloned().unwrap_or_else(|| cs.to_string()));
            }
            ChangeTag::Insert => out.push(cs.to_string()),
            ChangeTag::Delete => {}
        }
    }
    out
}

/// Line-level three-way merge. Returns Err(()) on overlapping edits.
#[allow(clippy::result_unit_err)] // the only failure is "no answer"; callers map it to 416 / conflict
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Result<String, ()> {
    if ours == theirs || theirs == base {
        return Ok(ours.to_string());
    }
    if ours == base {
        return Ok(theirs.to_string());
    }
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let hunks_o = hunks(base, ours);
    let hunks_t = hunks(base, theirs);
    // overlap check (touching hunks count as conflicts)
    for a in &hunks_o {
        for c in &hunks_t {
            if a.0 <= c.1 && c.0 <= a.1 {
                if a == c {
                    continue;
                }
                return Err(());
            }
        }
    }
    let mut all: Vec<_> = hunks_o.into_iter().chain(hunks_t).collect();
    all.sort_by_key(|h| (h.0, h.1));
    all.dedup();
    let mut out = String::new();
    let mut pos = 0;
    for (s, e, repl) in all {
        for l in &b[pos..s] {
            out.push_str(l);
        }
        out.push_str(&repl);
        pos = e;
    }
    for l in &b[pos..] {
        out.push_str(l);
    }
    Ok(out)
}

/// (base_start, base_end, replacement) hunks.
fn hunks(base: &str, other: &str) -> Vec<(usize, usize, String)> {
    let diff = TextDiff::from_lines(base, other);
    let mut v = Vec::new();
    for op in diff.ops() {
        if op.tag() == similar::DiffTag::Equal {
            continue;
        }
        let o = op.old_range();
        let n = op.new_range();
        let repl: String = other.split_inclusive('\n').skip(n.start).take(n.len()).collect();
        v.push((o.start, o.end, repl));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_disjoint() {
        let base = "a\nb\nc\nd\ne\n";
        let o = "A\nb\nc\nd\ne\n";
        let t = "a\nb\nc\nd\nE\n";
        assert_eq!(merge3(base, o, t).unwrap(), "A\nb\nc\nd\nE\n");
    }
    #[test]
    fn merge_conflict() {
        assert!(merge3("a\n", "b\n", "c\n").is_err());
    }
    #[test]
    fn blame_carry() {
        let b = carry_blame("x\ny\n", &["1".into(), "1".into()], "x\nz\ny\n", "2");
        assert_eq!(b, vec!["1", "2", "1"]);
    }
}
