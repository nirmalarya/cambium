# Test plan (replica-test, adapted: Rust unit/integration tests + a bash e2e driving real `cmbd` + `cmb`; Playwright/axe N/A — no GUI)

Run: `cargo test` and `bash tests/e2e.sh` (after `cargo build`).

| case | flow | type | automated in |
|---|---|---|---|
| F01-H1 init creates repo + main | F01 | happy | engine.rs init_creates_main, e2e "F01 init" |
| F01-N1 duplicate / bad repo name | F01 | negative | init_creates_main |
| F01-N2 no token / bad token → 401 | F01 | negative | e2e |
| F02-H1 status → add → commit → clean | F02 | happy | e2e |
| F02-E1 nothing staged | F02 | edge | e2e |
| F02-E2 branch moved meanwhile (stale head) → rejected + hint | F02 | edge | stale_commit_rejected, e2e |
| F02-E3 8 concurrent commits on one head → exactly 1 wins | F02 | edge (two tabs) | concurrent_cas_one_winner |
| F02-N1 `..`, absolute, `//`, `.cambium` paths; missing blob | F02 | negative/security | rejects_bad_paths_and_missing_blobs, e2e traversal |
| F03-H1 branch, commit, switch, merge | F03 | happy | merge_clean_and_conflict, e2e |
| F03-E1 conflicting edits reported | F03 | edge | merge_clean_and_conflict, e2e |
| F04-H1 rebase replays in order with merged content | F04 | happy | rebase_replays, e2e |
| F05-H1 log / path filter / blame / diff | F05 | happy | blame_tracks_lines, log_path_filter_and_diff, e2e |
| F05-E1 blame second read served from derived cache | F05 | edge | blame_tracks_lines |
| F06-H1 sparse checkout only fetches prefix; widen | F06 | happy | sparse_tree, e2e |
| F07-H1 read file with no working copy | F07 | happy | e2e |
| F08-H1 cache cap enforced | F08 | edge | e2e |
| F09-H1 offline commit queued; flush lands after main moved | F09 | happy | e2e |
| WAL rows for each changeset and ref move | backend | invariant | wal_records_everything |
| CAS create/move/delete semantics | backend | invariant | cas_semantics |
| tokens hashed and verified | backend | invariant | tokens |
| blob store content-addressed, compressed, rejects traversal ids | backend | invariant | fs_blob_store_roundtrip, fs_blobs_are_compressed |
| merge3 disjoint/conflict, blame carry | core | unit | text.rs tests |

Not covered (to check): very large files, binary merge, Unicode paths on other OSes, server restart mid-request.

## Results (2026-10-31)
- `cargo test`: **18 passed, 0 failed** (3 unit in text.rs + 15 integration in tests/engine.rs)
- `tests/e2e.sh`: **30 passed, 0 failed**
- Total **48 / 48**.


## Update 2026-10-31
cargo test: 29 passed, 0 failed (cambium-core unit 6, cambium-core integration 21, cambium-server 2).
e2e (tests/e2e.sh): 47 passed, 0 failed. New: M01-M05 modes/symlinks, R01-R04 resumable fetch, L01-L04 rate limiting.
Total 76 passed, 0 failed.

## Update 2026-10-31 (identity + permissions)
cargo test: 35 passed, 0 failed (cambium-core unit 6, cambium-core integration 25, cambium-server 4).
- **cambium-core:** 4 new ACL tests (open repo until the owner locks it, longest prefix wins with groups, filtering and hidden blobs, merge blocked by its check).
- **cambium-server:** 2 new tests:
  - OIDC: accepts a valid token; rejects a wrong audience, wrong issuer, expired token, unknown key id, or HS256 forgery.
  - HTTP ACL enforcement for OIDC and static identities.
- **Test key:** a throwaway RSA key in crates/cambium-server/tests/fixtures, used only by these tests.

e2e: 57 passed, 0 failed. The new checks are A01–A10 (whoami, the CAMBIUM_TOKEN_CMD helper, open by default, owner-only first rule, hidden directory on checkout, 403 with hint, group-scoped writes, hidden file via API, non-admin cannot manage rules).

Total: 92 passed, 0 failed.

## Update 2026-10-31 (storage, scale, review, could-haves, import)
| suite | checks | covers |
|---|---|---|
| cambium-core lib | 8 | S3 SigV4 (AWS test vector), amz date, limits |
| cambium-core tests/engine.rs | 34 | + ancestry/generation numbers, path history + backfill, review rules (no self-approval, stale approvals, N required, merge once), rename/copy diff, search index |
| cambium-core tests/s3.rs | 4 | S3 store against an in-process fake that checks signatures |
| cambium-core tests/gitimport.rs | 1 | real git repo: branches, merge, tag, exec bit, symlink, rename, authors, timestamps |
| cambium-core tests/pg.rs | 3 (ignored unless CAMBIUM_TEST_PG) | two engines on one Postgres, CAS races |
| cmbd unit | 4 | OIDC with an RSA key generated at test time (no key in the tree) |
| tests/e2e.sh | 100 | + W01-W16 review/protect/UI (cookie, CSRF, escaping), X01-X04 search + rename, Y01-Y04 shared cache, U01-U05 cmb watch, I01-I04 import-git, Z01-Z06 edge cache, V01-V04 FUSE mount |
| tests/multinode.sh | 9 | Postgres 16 container, full e2e on Postgres, two nodes, commit race, 20 concurrent ref creates |
| tests/docker-smoke.sh | 9 | built image: API, search, UI, protection, import-git, non-root |

## Update 2026-10-31 (enterprise / production hardening) — final rerun
| suite | result | covers |
|---|---|---|
| cargo test --workspace | 69 passed, 0 failed, 5 ignored (the Postgres tests, run below) | + tests/prod.rs (8: migrations, audit chain, tokens, sessions, code owners, backup/restore), tests/props.rs (9 proptests) |
| cambium-core tests/pg.rs (CAMBIUM_TEST_PG) | 5 passed | + audit chain from two nodes stays linear, tokens/sessions/migrations on Postgres |
| tests/e2e.sh | 109 passed | + V05-V10 writable copy-on-write mount |
| tests/prod.sh | 38 passed | P01-P38: probes, request ids, metrics, JSON logs, headers, 413, idempotency, token lifecycle, audit, migrations, code owners (API + UI), SIGTERM drain, TLS, broken log pipe |
| tests/multinode.sh | 9 passed | unchanged (full e2e on Postgres + two nodes) |
| tests/restore-drill.sh | 12 passed | Postgres → fresh Postgres, corrupt backup refused, SQLite → Postgres |
| tests/docker-smoke.sh | 13 passed | + uid 65532, read-only rootfs, no shell, probes, SBOM, graceful stop |
| tests/k8s-check.sh | 17 passed | helm lint --strict, kubeconform 1.28-1.31, policy asserts |
| tests/load/run.sh | 2000/2000 commits ok on SQLite and Postgres | see load-results.jsonl |
Gaps: no real S3, IdP, cluster, or production-grade load hardware on the build box (see rollout-checklist.md).
