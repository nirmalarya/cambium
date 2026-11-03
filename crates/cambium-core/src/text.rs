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

/// Three-way merge that always gives an answer: edits that overlap become conflict
/// blocks marked the way git marks them (`<<<<<<<`, `=======`, `>>>>>>>`), so a person
/// can resolve them in an editor. Returns the text and the number of conflict blocks.
pub fn merge3_marked(base: &str, ours: &str, theirs: &str, ours_label: &str, theirs_label: &str) -> (String, usize) {
    if let Ok(m) = merge3(base, ours, theirs) {
        return (m, 0);
    }
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let mut all: Vec<(usize, usize, String, bool)> = hunks(base, ours)
        .into_iter()
        .map(|(s, e, r)| (s, e, r, true))
        .chain(hunks(base, theirs).into_iter().map(|(s, e, r)| (s, e, r, false)))
        .collect();
    all.sort_by_key(|h| (h.0, h.1));
    let (mut out, mut pos, mut n, mut i) = (String::new(), 0, 0, 0);
    while i < all.len() {
        // one cluster: hunks that overlap or touch (the same rule `merge3` uses)
        let (s, mut e) = (all[i].0, all[i].1);
        let mut j = i + 1;
        while j < all.len() && all[j].0 <= e {
            e = e.max(all[j].1);
            j += 1;
        }
        let cluster = &all[i..j];
        // what one side made of base[s..e]
        let side = |ours: bool| -> Option<String> {
            let hs: Vec<_> = cluster.iter().filter(|h| h.3 == ours).collect();
            if hs.is_empty() {
                return None;
            }
            let (mut t, mut p) = (String::new(), s);
            for h in hs {
                b[p..h.0].iter().for_each(|l| t.push_str(l));
                t.push_str(&h.2);
                p = h.1;
            }
            b[p..e].iter().for_each(|l| t.push_str(l));
            Some(t)
        };
        b[pos..s].iter().for_each(|l| out.push_str(l));
        match (side(true), side(false)) {
            (Some(o), Some(t)) if o != t => {
                n += 1;
                let nl = |x: &mut String| {
                    if !x.is_empty() && !x.ends_with('\n') {
                        x.push('\n');
                    }
                };
                out.push_str(&format!("<<<<<<< {ours_label}\n"));
                out.push_str(&o);
                nl(&mut out);
                out.push_str("=======\n");
                out.push_str(&t);
                nl(&mut out);
                out.push_str(&format!(">>>>>>> {theirs_label}\n"));
            }
            (Some(x), _) | (None, Some(x)) => out.push_str(&x),
            (None, None) => {}
        }
        pos = e;
        i = j;
    }
    b[pos..].iter().for_each(|l| out.push_str(l));
    (out, n)
}

/// (base_start, base_end, replacement) hunks, in base order and never overlapping.
///
/// Both texts are split into lines the way the merge indexes them (on `\n` only):
/// the diff library's own line splitting also breaks on a lone `\r`. And the ops it
/// returns are checked before use: on some inputs similar 2.7's Myers and Patience
/// return ops whose positions do not follow each other (found by fuzzing), which
/// made merges read out of bounds. Such a diff is redone with LCS (small inputs), or
/// the whole file becomes one hunk, which is always correct (at worst a conflict).
fn hunks(base: &str, other: &str) -> Vec<(usize, usize, String)> {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let o: Vec<&str> = other.split_inclusive('\n').collect();
    let from_ops = |ops: &[similar::DiffOp]| -> Option<Vec<(usize, usize, String)>> {
        let (mut oi, mut ni, mut v) = (0, 0, Vec::new());
        for op in ops {
            let (or, nr) = (op.old_range(), op.new_range());
            if or.start != oi || nr.start != ni || or.end > b.len() || nr.end > o.len() {
                return None;
            }
            (oi, ni) = (or.end, nr.end);
            if op.tag() != similar::DiffTag::Equal {
                v.push((or.start, or.end, o[nr].concat()));
            }
        }
        (oi == b.len() && ni == o.len()).then_some(v)
    };
    if let Some(v) = from_ops(TextDiff::from_slices(&b, &o).ops()) {
        return v;
    }
    if b.len().saturating_mul(o.len()) <= 4_000_000 {
        let d = TextDiff::configure().algorithm(similar::Algorithm::Lcs).diff_slices(&b, &o);
        if let Some(v) = from_ops(d.ops()) {
            return v;
        }
    }
    vec![(0, b.len(), other.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inconsistent_diff_ops_do_not_break_the_merge() {
        // similar 2.7's Myers diff returns out-of-order ops for this pair
        let base = "*/bytes=\u{e9}\u{c} l1\n..#author #\n\n";
        let theirs = "\n\nauthor \u{b}Ol5\n\nauthor =======x";
        for h in [hunks(base, theirs), hunks(base, "b")] {
            assert!(h.windows(2).all(|w| w[0].1 <= w[1].0), "{h:?}");
        }
        let (_, n) = merge3_marked(base, "b", theirs, "o", "t");
        assert!(n >= 1);
        assert!(merge3(base, "b", theirs).is_err());
    }
    #[test]
    fn carriage_returns_do_not_break_the_merge() {
        // a lone \r used to count as a line break in the diff but not in the merge
        let base = "a\rb\nc\n";
        assert_eq!(merge3(base, "a\rb\nC\n", base).unwrap(), "a\rb\nC\n");
        assert_eq!(merge3(base, "X\rb\nc\n", "a\rb\nc\nd\n").unwrap(), "X\rb\nc\nd\n");
        let (_, n) = merge3_marked("x\r\ny\n", "x\r1\ny\n", "x\r2\ny\n", "o", "t");
        assert_eq!(n, 1);
    }
    #[test]
    fn marked_conflict_keeps_both_sides() {
        let base = "a\nb\nc\n";
        let (m, n) = merge3_marked(base, "a\nB-ours\nc\n", "a\nB-theirs\nc\n", "your drafts", "main");
        assert_eq!(n, 1);
        assert_eq!(m, "a\n<<<<<<< your drafts\nB-ours\n=======\nB-theirs\n>>>>>>> main\nc\n");
    }
    #[test]
    fn marked_merges_disjoint_edits_cleanly() {
        let base = "a\nb\nc\nd\ne\nf\ng\n";
        let (m, n) = merge3_marked(base, "A\nb\nc\nd\ne\nf\ng\n", "a\nb\nc\nd\ne\nf\nG\n", "o", "t");
        assert_eq!((m.as_str(), n), ("A\nb\nc\nd\ne\nf\nG\n", 0));
    }
    #[test]
    fn marked_conflict_next_to_a_clean_edit() {
        let base = "1\n2\n3\n4\n5\n6\n7\n";
        let (m, n) = merge3_marked(base, "one\n2\n3\n4\n5\nsix-o\n7\n", "1\n2\n3\n4\n5\nsix-t\n7", "o", "t");
        assert_eq!(n, 1);
        assert!(m.starts_with("one\n2\n"), "{m}");
        assert!(
            m.contains("<<<<<<< o\nsix-o\n=======\nsix-t\n7\n>>>>>>> t\n")
                || m.contains("<<<<<<< o\nsix-o\n7\n=======\nsix-t\n7\n>>>>>>> t\n"),
            "{m}"
        );
    }
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
