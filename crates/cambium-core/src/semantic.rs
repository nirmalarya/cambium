//! Semantic classification of changes for review rules (Scion's review tool can
//! separate comment-only edits from logic changes). Conservative by design: anything
//! not positively recognised as documentation or a comment/formatting-only edit in a
//! supported language is `Logic`, so a misclassification can only ever ask for *more*
//! review, never less.
use serde::Serialize;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// documentation files only
    Docs,
    /// code where only comments or formatting changed
    Comments,
    /// anything else
    Logic,
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    /// `//` and `/* */` comments, "..." and '...' strings with backslash escapes;
    /// whitespace between tokens is not significant
    CLike,
    /// `#` comments, '...' / "..." / triple-quoted strings (Python, TOML); line
    /// structure and indentation are significant, blank lines are not
    Hash,
}

fn ext(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or("", |(_, e)| e)
}
fn is_doc(path: &str) -> bool {
    matches!(ext(path).to_ascii_lowercase().as_str(), "md" | "markdown" | "rst" | "adoc" | "txt")
}
fn lang(path: &str) -> Option<Lang> {
    // YAML is deliberately absent: block scalars hold unquoted text where `#` is content
    Some(match ext(path) {
        "rs" | "go" | "java" | "c" | "h" | "cc" | "cpp" | "hpp" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "kt" | "swift" | "cs" => {
            Lang::CLike
        }
        "py" | "toml" => Lang::Hash,
        _ => return None,
    })
}

/// Code with comments removed, in a form where formatting-only differences vanish.
/// Strings are copied verbatim. None if the text cannot be lexed confidently.
fn normalize(text: &str, l: Lang) -> Option<String> {
    let b: Vec<char> = text.chars().collect();
    let n = b.len();
    let mut out = String::new();
    // CLike: a pending separator for a run of whitespace/comments between tokens
    let mut sep = false;
    let line_empty =
        |o: &String| o.is_empty() || o.ends_with('\n') || o.chars().rev().take_while(|c| *c != '\n').all(|c| c == ' ' || c == '\t');
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == '"' || c == '\'' {
            if sep {
                out.push(' ');
                sep = false;
            }
            let triple = l == Lang::Hash && i + 2 < n && b[i + 1] == c && b[i + 2] == c;
            let w = if triple { 3 } else { 1 };
            out.extend(&b[i..i + w]);
            i += w;
            loop {
                if i >= n {
                    return None; // unterminated: do not guess
                }
                if b[i] == '\\' {
                    out.extend(&b[i..(i + 2).min(n)]);
                    i += 2;
                    continue;
                }
                if b[i] == c && (!triple || (i + 2 < n && b[i + 1] == c && b[i + 2] == c)) {
                    out.extend(&b[i..i + w]);
                    i += w;
                    break;
                }
                if b[i] == '\n' && !triple && l == Lang::Hash {
                    return None;
                }
                out.push(b[i]);
                i += 1;
            }
            continue;
        }
        match l {
            Lang::CLike => {
                if c == '/' && i + 1 < n && b[i + 1] == '/' {
                    while i < n && b[i] != '\n' {
                        i += 1;
                    }
                    sep = true;
                } else if c == '/' && i + 1 < n && b[i + 1] == '*' {
                    let mut j = i + 2;
                    while j + 1 < n && !(b[j] == '*' && b[j + 1] == '/') {
                        j += 1;
                    }
                    if j + 1 >= n {
                        return None;
                    }
                    i = j + 2;
                    sep = true;
                } else if c.is_whitespace() {
                    sep = true;
                    i += 1;
                } else {
                    if sep && !out.is_empty() {
                        out.push(' ');
                    }
                    sep = false;
                    out.push(c);
                    i += 1;
                }
            }
            Lang::Hash => {
                if c == '#' {
                    while i < n && b[i] != '\n' {
                        i += 1;
                    }
                } else if c == '\n' {
                    // end of a code line: drop trailing blanks; drop the line if empty
                    while out.ends_with(' ') || out.ends_with('\t') {
                        out.pop();
                    }
                    if !line_empty(&out) {
                        out.push('\n');
                    } else {
                        while out.ends_with(' ') || out.ends_with('\t') {
                            out.pop();
                        }
                    }
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }
    if l == Lang::Hash {
        while out.ends_with(' ') || out.ends_with('\t') || out.ends_with('\n') {
            out.pop();
        }
    }
    Some(out)
}

/// Kind of one file change. `old`/`new` are the contents (None = absent or binary).
pub fn classify_file(path: &str, old: Option<&str>, new: Option<&str>) -> ChangeKind {
    if is_doc(path) {
        return ChangeKind::Docs;
    }
    let (Some(l), Some(o), Some(n)) = (lang(path), old, new) else { return ChangeKind::Logic };
    match (normalize(o, l), normalize(n, l)) {
        (Some(a), Some(b)) if a == b => ChangeKind::Comments,
        _ => ChangeKind::Logic,
    }
}

/// Kind of a whole change: the most significant of its files (Logic > Comments > Docs).
pub fn classify(files: impl IntoIterator<Item = ChangeKind>) -> ChangeKind {
    files.into_iter().max().unwrap_or(ChangeKind::Docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChangeKind::*;
    #[test]
    fn comment_and_format_only_edits() {
        assert_eq!(classify_file("a.rs", Some("fn f() { 1 } // old"), Some("fn f() {\n    1\n} // new note")), Comments);
        assert_eq!(classify_file("a.rs", Some("/* a */ let x = 1;"), Some("let x = 1; /* b\n c */")), Comments);
        assert_eq!(classify_file("a.py", Some("x = 1  # one\n"), Some("x = 1  # uno\n\n")), Comments);
        assert_eq!(classify_file("c.toml", Some("a = 1 # x\n"), Some("# heading\na = 1\n")), Comments);
        assert_eq!(classify_file("c.yaml", Some("a: 1 # x\n"), Some("a: 1\n")), Logic, "YAML is never judged");
        assert_eq!(classify_file("README.md", Some("a"), Some("b")), Docs);
    }
    #[test]
    fn logic_is_never_mistaken_for_comments() {
        // comment markers inside strings are code
        assert_eq!(classify_file("a.rs", Some(r#"let u = "http://a";"#), Some(r#"let u = "http://b";"#)), Logic);
        assert_eq!(classify_file("a.py", Some("s = '# a'\n"), Some("s = '# b'\n")), Logic);
        assert_eq!(classify_file("a.rs", Some(r#"let s = "/* x */";"#), Some(r#"let s = "/* y */";"#)), Logic);
        // whitespace inside strings matters
        assert_eq!(classify_file("a.rs", Some(r#"let s = "a b";"#), Some(r#"let s = "a  b";"#)), Logic);
        // Python indentation is logic
        assert_eq!(classify_file("a.py", Some("if x:\n    a()\nb()\n"), Some("if x:\n    a()\n    b()\n")), Logic);
        // code after a comment marker on a new line
        assert_eq!(classify_file("a.rs", Some("// c\nf();"), Some("// c\ng();")), Logic);
        // unknown languages, binaries, adds and deletes are logic
        assert_eq!(classify_file("a.sh", Some("# a"), Some("# b")), Logic);
        // Python: text inside triple-quoted strings (including '#' and blank lines) is code
        assert_eq!(classify_file("a.py", Some("d = \"\"\"\n# a\n\"\"\"\n"), Some("d = \"\"\"\n# b\n\"\"\"\n")), Logic);
        assert_eq!(classify_file("a.py", Some("d = \"\"\"\nx\n\"\"\"\n"), Some("d = \"\"\"\nx\n\n\"\"\"\n")), Logic);
        // C-like: whitespace that separates tokens still matters as a separator
        assert_eq!(classify_file("a.c", Some("int a;"), Some("inta;")), Logic);
        assert_eq!(classify_file("a.rs", None, Some("// new file")), Logic);
        assert_eq!(classify_file("a.rs", Some("x"), None), Logic);
        // an unterminated string cannot be judged
        assert_eq!(classify_file("a.rs", Some("let s = \"abc // x"), Some("let s = \"abc // y")), Logic);
        assert_eq!(classify([Docs, Comments, Logic]), Logic);
        assert_eq!(classify([Docs, Comments]), Comments);
    }
}
