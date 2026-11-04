//! Properties checked against arbitrary input, for the parsers that read data from
//! outside: HTTP headers, paths sent by clients and servers, file contents (merge,
//! review classifier, owners files) and git objects. Each function takes raw bytes,
//! must never panic on its own, and panics only if a property is broken.
//!
//! Used by the coverage-guided targets in `fuzz/` (cargo fuzz, nightly) and by the
//! seeded random test `tests/fuzz_smoke.rs` that runs with every `cargo test`.
use std::path::{Component, Path};

/// Split `data` into up to `n` strings at 0xFF bytes (which never occur in UTF-8).
fn parts(data: &[u8], n: usize) -> Vec<String> {
    let mut v: Vec<String> = data.splitn(n, |b| *b == 0xFF).map(|x| String::from_utf8_lossy(x).into_owned()).collect();
    v.resize(n, String::new());
    v
}

/// `Range` header: an accepted range always lies inside the body.
pub fn range(data: &[u8]) {
    let len = data.first().map_or(0, |b| u64::from(*b) * 37);
    let h = String::from_utf8_lossy(data.get(1..).unwrap_or_default());
    if let Ok((a, b)) = crate::limits::parse_range(&h, len) {
        assert!(a <= b && b < len, "range {h:?} on {len} bytes gave {a}-{b}");
    }
}

/// Paths: a path the server accepts is relative, has only normal components and
/// never names the `.cambium` metadata folder, so it cannot escape a working copy.
pub fn path(data: &[u8]) {
    let p = String::from_utf8_lossy(data);
    if crate::engine::valid_path(&p) {
        assert!(Path::new(p.as_ref()).components().all(|c| matches!(c, Component::Normal(_))), "valid_path accepted {p:?}");
        assert!(!p.split('/').any(|s| s == ".cambium"), "valid_path accepted {p:?}");
    }
}

/// Three-way merge: the clean merge and the marked merge agree, trivial merges take
/// the changed side, and a marked merge has one opening marker per conflict.
pub fn merge(data: &[u8]) {
    let v = parts(data, 3);
    let (b, o, t) = (&v[0], &v[1], &v[2]);
    use crate::text::{merge3, merge3_marked};
    assert_eq!(merge3(b, o, b).as_deref(), Ok(o.as_str()));
    assert_eq!(merge3(b, b, t).as_deref(), Ok(t.as_str()));
    assert_eq!(merge3(b, o, o).as_deref(), Ok(o.as_str()));
    let (m, n) = merge3_marked(b, o, t, "ours", "theirs");
    match merge3(b, o, t) {
        Ok(clean) => assert_eq!((m.as_str(), n), (clean.as_str(), 0)),
        Err(()) => {
            assert!(n > 0, "merge3 refused but the marked merge found no conflict");
            // exact marker counts only when the files do not already contain marker
            // lines themselves (found by cargo fuzz: content may be anything)
            let has = |x: &str| x.lines().any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l == "=======");
            if !has(b) && !has(o) && !has(t) {
                assert_eq!(m.lines().filter(|l| *l == "<<<<<<< ours").count(), n);
                assert_eq!(m.lines().filter(|l| *l == ">>>>>>> theirs").count(), n);
            }
        }
    }
}

/// Review classifier: any input gives an answer, and a real code change can never
/// pass as a comment-only one (that would let it land with fewer approvals): if
/// old -> new is "comments only", adding a line of code to `new` must make it logic.
pub fn semantic(data: &[u8]) {
    use crate::semantic::{classify, classify_file, ChangeKind};
    let v = parts(data, 3);
    let (path, old, new) = (&v[0], &v[1], &v[2]);
    let k = classify_file(path, Some(old), Some(new));
    let _ = classify([k]);
    let _ = classify_file(path, None, Some(new));
    let _ = classify_file(path, Some(old), None);
    if k == ChangeKind::Comments {
        for code in ["\nzz_fuzz_call();\n", "\nzz_fuzz = 1\n"] {
            let changed = format!("{new}{code}");
            assert_eq!(
                classify_file(path, Some(old), Some(&changed)),
                ChangeKind::Logic,
                "code added to {path:?} was classified as comments only"
            );
        }
    }
}

/// Owners files: every name parsed is non-empty and has no spaces or comment text.
pub fn owners(data: &[u8]) {
    let o = crate::review::parse_owners(&String::from_utf8_lossy(data));
    for n in o.users.iter().chain(&o.groups) {
        assert!(!n.is_empty() && !n.chars().any(char::is_whitespace) && !n.contains('#'), "owner name {n:?}");
    }
}

/// Git commit objects (from a repository being imported): parsing never panics, and
/// a parsed author never contains a newline.
pub fn git_commit(data: &[u8]) {
    if let Some((_, author, _, _)) = crate::gitimport::fuzz_parse_commit(data) {
        assert!(!author.contains('\n'), "author {author:?}");
    }
}

/// A named property check.
pub type Check = (&'static str, fn(&[u8]));
/// Every check, by name (the fuzz targets and the smoke test share this list).
pub const ALL: &[Check] =
    &[("range", range), ("path", path), ("merge", merge), ("semantic", semantic), ("owners", owners), ("git_commit", git_commit)];
