//! Seeded random inputs through every `fuzzcheck` property, on stable Rust and in
//! every `cargo test`. The coverage-guided versions live in `fuzz/` (cargo fuzz).
//! CAMBIUM_FUZZ_ITERS sets the inputs per property (default 3000); CAMBIUM_FUZZ_SEED
//! picks the seed (printed, so a failure can be replayed).
use cambium_core::fuzzcheck::ALL;
use rand::{rngs::StdRng, Rng, SeedableRng};

/// Inputs shaped like the real thing as well as raw bytes: short lines from a small
/// alphabet (so merges overlap and conflict), header-like text, and fragments that
/// parsers care about (`bytes=`, `..`, `parent `, `#`, markers).
fn input(r: &mut StdRng) -> Vec<u8> {
    const BITS: &[&str] = &[
        "bytes=",
        "-",
        ",",
        "0",
        "1",
        "9",
        "18446744073709551615",
        "/",
        "..",
        ".",
        ".cambium",
        "\\",
        "a",
        "b",
        "\n",
        "\n\n",
        " ",
        "#",
        "//",
        "/*",
        "*/",
        "\"",
        "'",
        "parent ",
        "author ",
        "tree ",
        "<x@y> ",
        "1600000000 +0000",
        "user:",
        "group:",
        "<<<<<<< ",
        "=======",
        ">>>>>>> ",
        "\u{1e}",
        "\u{0}",
        "\u{ff}",
        "é",
    ];
    let n = r.gen_range(0..64);
    let mut out = vec![];
    for _ in 0..n {
        match r.gen_range(0..10) {
            0..=5 => out.extend_from_slice(BITS[r.gen_range(0..BITS.len())].as_bytes()),
            6 => out.push(0xFF), // part separator for multi-part properties
            7 => out.extend_from_slice(format!("l{}\n", r.gen_range(0..6)).as_bytes()),
            _ => out.push(r.gen()),
        }
    }
    out
}

#[test]
fn every_property_holds_on_random_input() {
    let iters: usize = std::env::var("CAMBIUM_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let seed: u64 = std::env::var("CAMBIUM_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0xC0FFEE);
    eprintln!("fuzz smoke: seed {seed}, {iters} inputs per property");
    for (name, check) in ALL {
        let mut r = StdRng::seed_from_u64(seed ^ name.len() as u64);
        for i in 0..iters {
            let data = input(&mut r);
            let res = std::panic::catch_unwind(|| check(&data));
            assert!(res.is_ok(), "property `{name}` failed on input #{i} (seed {seed}): {data:?}");
        }
    }
}

/// Inputs that once broke, or nearly did, kept as fixed regression cases.
#[test]
fn known_tricky_inputs() {
    let cases: &[(&str, &[u8])] = &[
        ("range", b"\x00bytes=0-0"),
        ("range", b"\x01bytes=-18446744073709551615"),
        ("path", b"a/../b"),
        ("path", b"./a"),
        ("path", b"a//b"),
        ("merge", b"a\nb\n\xffa\nB\n\xffa\nC"),
        ("merge", b"\xffx\n\xffy"),
        // cargo fuzz (CI): the file content itself contains a marker line
        ("merge", b"\n\xff\n>>>>>>> theirs"),
        // cargo fuzz: the same edit on both sides, split into different hunks
        ("merge", b";1\n\n\n\xdc\n\xdc\n\np\n\x03\n\n\xdc\n\n\xff\n\n\n\xff\n\n\xff\n\n"),
        ("git_commit", b"tree 1\nparent 2\nauthor A <a@b> 1 +0000\n\nmsg \x1e with separator"),
        ("git_commit", b"author \n\n"),
        ("owners", b"user:\ngroup: # x\n@"),
    ];
    for (name, data) in cases {
        let check = ALL.iter().find(|(n, _)| n == name).unwrap().1;
        check(data);
    }
}
