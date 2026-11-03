# Bugs
Found and fixed during build/test:
| ID | sev | steps | expected | actual | fix |
|---|---|---|---|---|---|
| B1 | S3 | offline commit with CAMBIUM_SERVER set to unreachable host while state.json held the real server | env override used | state's server always won, so outbox path never triggered | env CAMBIUM_SERVER now takes priority (crates/cambium-cli/src/main.rs `from_env`); covered by e2e F09 |
| B2 | S4 | e2e "diff between revisions" used unsupported `main~` syntax | — | passed only via fallback | test rewritten to use explicit id |
| B3 | S2 | three-way merge of a file containing a lone `\r` (or certain inputs where similar 2.7's Myers diff returns out-of-order ops) | merged text or a conflict | server panicked (slice out of range) while merging or landing | both sides split into lines the same way; diff ops validated, redone with LCS or treated as one hunk (`text.rs` `hunks`); found by fuzzing, regression tests in `text.rs` |
| B4 | S3 | OWNERS line with `user:` or `group:` and no name, or names separated by other whitespace | ignored / split | empty owner name; whitespace inside names | split on any whitespace, empty names dropped (`review.rs` `parse_owners`); found by fuzzing |
| B5 | S3 | `cmbd import-git` of a commit whose message contains byte 0x1E | imported intact | import stopped with "unexpected git log output" | commits are read as raw objects (`git cat-file`), not parsed from `git log` output; test `messages_with_any_bytes_import_intact` |
| B6 | S3 | `cmb` given an unexpected server reply (proxy page, wrong server) or a disk error | readable error | Rust panic from `unwrap()` | every reply field and file operation reports a `cmb:` error instead |
| B7 | S3 | `cmbd backup` of a large database | constant memory | each table loaded fully into memory | rows streamed to disk (`Conn::each`); blob ids de-duplicated on disk, not in memory |

Open: none. Known gaps: none tracked here; see the README's Known issues for what production
use still needs.
