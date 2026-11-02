//! Code search over a revision, backed by an inverted token index.
//!
//! The index is keyed by blob, so it is shared by every revision and branch
//! that contains the same content and only new blobs are ever tokenized.
//! A query's tokens narrow the candidate blobs; candidates are then scanned
//! for the literal (case-insensitive) query so results are exact.
use crate::db::Val;
use crate::engine::{split_entry, Engine, Manifest, Result};
use crate::p;
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

const MAX_INDEXED: usize = 4 << 20;

pub fn tokens(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| t.len() >= 2 && t.len() <= 64).map(|t| t.to_lowercase()).collect()
}
/// What a blob containing `q` must contain: interior words exactly, and the
/// first/last runs (which may be partial words) as a substring of some token.
fn required(q: &str) -> Vec<(String, bool)> {
    let parts: Vec<&str> = q.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
    let n = parts.len();
    parts.iter().enumerate().filter(|(_, t)| t.len() >= 2 && t.len() <= 64).map(|(i, t)| (t.to_lowercase(), i == 0 || i + 1 == n)).collect()
}
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

impl Engine {
    /// Make sure every blob in `m` is indexed; returns how many were new.
    pub fn index_blobs(&self, m: &Manifest) -> Result<usize> {
        let blobs: Vec<String> = m.values().map(|e| split_entry(e).0.to_string()).collect::<BTreeSet<_>>().into_iter().collect();
        let mut new = 0;
        for chunk in blobs.chunks(500) {
            let ph: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let done: HashSet<String> = self
                .db
                .query(
                    &format!("SELECT blob FROM search_done WHERE blob IN ({})", ph.join(",")),
                    chunk.iter().map(|b| Val::Text(b.clone())).collect(),
                )?
                .into_iter()
                .map(|r| r.s(0))
                .collect();
            for b in chunk.iter().filter(|b| !done.contains(*b)) {
                let data = self.blobs.get(b)?.unwrap_or_default();
                let toks: Vec<String> = if data.len() > MAX_INDEXED || data.contains(&0) {
                    vec![]
                } else {
                    std::str::from_utf8(&data).map(|s| tokens(s).into_iter().collect()).unwrap_or_default()
                };
                let b = b.clone();
                self.db.tx(move |t| {
                    for tok in &toks {
                        t.exec("INSERT INTO search_terms(token,blob) VALUES(?1,?2) ON CONFLICT DO NOTHING", &p![tok, &b])?;
                    }
                    t.exec("INSERT INTO search_done(blob) VALUES(?1) ON CONFLICT DO NOTHING", &p![&b])?;
                    Ok(())
                })?;
                new += 1;
            }
        }
        Ok(new)
    }
    /// Search the files of `m` (already permission-filtered by the caller).
    pub fn search(&self, m: &Manifest, q: &str, limit: usize) -> Result<Vec<Hit>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        self.index_blobs(m)?;
        let mut cand: Option<HashSet<String>> = None;
        for (tok, partial) in required(q) {
            let rows = if partial {
                self.db.query(
                    "SELECT DISTINCT blob FROM search_terms WHERE token LIKE ?1 ESCAPE '\\'",
                    p![format!("%{}%", like_escape(&tok))],
                )?
            } else {
                self.db.query("SELECT blob FROM search_terms WHERE token=?1", p![tok])?
            };
            let s: HashSet<String> = rows.into_iter().map(|r| r.s(0)).collect();
            cand = Some(match cand {
                None => s,
                Some(c) => c.intersection(&s).cloned().collect(),
            });
        }
        let ql = q.to_lowercase();
        let mut hits = vec![];
        // path matches first (line 0), then content matches
        for path in m.keys() {
            if path.to_lowercase().contains(&ql) {
                hits.push(Hit { path: path.clone(), line: 0, text: String::new() });
                if hits.len() >= limit {
                    return Ok(hits);
                }
            }
        }
        let mut paths: Vec<(&String, &str)> =
            m.iter().map(|(p, e)| (p, split_entry(e).0)).filter(|(_, b)| cand.as_ref().is_none_or(|c| c.contains(*b))).collect();
        paths.sort();
        // No indexed token narrowed the candidates: very short queries match paths
        // only, others scan a bounded number of files instead of every blob.
        let cap = match &cand {
            Some(_) => usize::MAX,
            None if ql.chars().count() < 3 => 0,
            None => 5000,
        };
        for (path, b) in paths.into_iter().take(cap) {
            let data = self.blobs.get(b)?.unwrap_or_default();
            if data.contains(&0) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&data) else { continue };
            for (i, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&ql) {
                    hits.push(Hit { path: path.clone(), line: i + 1, text: line.chars().take(300).collect() });
                    if hits.len() >= limit {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }
}
