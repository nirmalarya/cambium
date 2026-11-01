//! Property tests for the parsers and text algorithms that face user input.
use cambium_core::engine::{path_keys, valid_path};
use cambium_core::limits::parse_range;
use cambium_core::text::{merge3, unified};
use proptest::prelude::*;

fn lines() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b", "c", "d", "", "x y"]), 0..12)
        .prop_map(|v| v.iter().map(|l| format!("{l}\n")).collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]
    #[test] fn merge_identities(b in lines(), o in lines()) {
        prop_assert_eq!(merge3(&b, &b, &o).unwrap(), o.clone());
        prop_assert_eq!(merge3(&b, &o, &b).unwrap(), o.clone());
        prop_assert_eq!(merge3(&b, &o, &o).unwrap(), o);
    }
    #[test] fn merge_never_panics(b in lines(), o in lines(), t in lines()) { let _ = merge3(&b, &o, &t); }
    #[test] fn merge_symmetric_success(b in lines(), o in lines(), t in lines()) {
        prop_assert_eq!(merge3(&b, &o, &t).is_ok(), merge3(&b, &t, &o).is_ok());
    }
    #[test] fn diff_counts_match(a in lines(), b in lines()) {
        let d = unified("f", &a, &b);
        if a == b { prop_assert!(!d.lines().any(|l| l.starts_with('+') && !l.starts_with("+++"))); }
        let plus = d.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
        let minus = d.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
        prop_assert!(plus <= b.lines().count() && minus <= a.lines().count());
        prop_assert!(plus as i64 - minus as i64 == b.lines().count() as i64 - a.lines().count() as i64);
    }
    #[test] fn diff_arbitrary_text(a in ".{0,200}", b in ".{0,200}") { let _ = unified("p", &a, &b); }
    #[test] fn valid_path_rejects_traversal(s in "[a-z./\\\\]{0,20}") {
        if valid_path(&s) {
            prop_assert!(!s.starts_with('/') && !s.contains('\\'));
            prop_assert!(s.split('/').all(|seg| !seg.is_empty() && seg != ".." && seg != "." && seg != ".cambium"));
            prop_assert_eq!(path_keys(&s).last().cloned().unwrap(), s.clone());
        }
    }
    #[test] fn valid_path_any_unicode(s in "\\PC{0,40}") { let _ = valid_path(&s); }
    #[test] fn range_in_bounds(h in "bytes=[0-9]{0,4}-[0-9]{0,4}", len in 0u64..5000) {
        if let Ok((a, b)) = parse_range(&h, len) { prop_assert!(a <= b && b < len); }
    }
    #[test] fn range_garbage(h in "\\PC{0,30}", len in 0u64..u64::MAX) {
        if let Ok((a, b)) = parse_range(&h, len) { prop_assert!(a <= b && b < len); }
    }
}
