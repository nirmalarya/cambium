# Build log (2026-10-31)
| ID | status | notes |
|---|---|---|
| S01 init | done | root changeset + `main` via CAS |
| S02 checkout | done | sparse prefixes, fetch only missing blobs, dirty-overwrite refused. Virtualized (on-read) materialisation not built |
| S03 status | done | hashes files; no fs watcher |
| S04 add | done | deletions stageable |
| S05 commit | done | stale head → 409 + hint |
| S06 branch/switch | done | |
| S07 merge | done | line-level 3-way; conflicts listed, no conflict markers written |
| S08 rebase | done | refuses to rebase across merge changesets |
| S09 log | done | path filter walks first parents (no index) |
| S10 blame | done | derived recursively, cached per (changeset, path) |
| S11 diff | done | rev↔rev and working copy overlay (server computes) |
| S12 API-only | done | `cmb cat`, merge-base, changeset endpoints |
| S13 fleet caches | skipped | infra |
| S14/S15 review UI | concept | design/review-ui.md |
| fix: offline outbox | done | `cmb outbox` / `cmb flush` |
Harder than expected: rebase of queued offline work (solved with scratch ref + server rebase + CAS).
Toolchain: distro rustc 1.85 too old for deps; installed stable via rustup (1.99.1). Screenshots step N/A (CLI).

## 2026-10-31: storage, scale, review, could-haves, import
- Removed the committed test RSA key; cmbd tests now generate a 2048-bit key and a JWKS at test time (pure Rust `rsa`). No key material in the tree.
- Added: S3 backend, Postgres metadata + two-node CAS tests, generation numbers + path history index, read-only FUSE mount, review API/UI + protected branches, rename/copy diff, search index, shared machine cache, `cmbd edge`, `cmb watch`, `cmbd import-git`.
- Verified: cargo 51 passed (3 Postgres tests run in multinode.sh), e2e 100/100, multinode 9/9, docker smoke 9/9, parity 96.5, sweep clean.

## 2026-10-31: enterprise / production hardening (approved scope)
- Writable copy-on-write FUSE mount; OWNERS code owners (API + UI).
- Observability (probes, metrics, JSON logs, request ids), audit chain + export/verify, token TTL/rotate/revoke, sessions, TLS, security headers, limits, idempotency, graceful drain, migrations, backup/restore + drill, load test.
- Real bug found by the Postgres audit test: `pg_advisory_xact_lock` returns a void column; decoding it panicked the pool worker ("postgres worker gone"). Fixed with tolerant decoding, `catch_unwind` per job and batch execution; regression test added.
- Supply chain: fuser 0.16 (RUSTSEC-2021-0154), axum-server 0.8 (drops unmaintained rustls-pemfile), removed `rsa` dev-dep (RUSTSEC-2023-0071; test key now via openssl CLI). cargo audit 0 / cargo deny ok. Pinned base images, CycloneDX SBOMs, Helm chart, CI workflow.
- Order note: item 9 (fmt/clippy/audit/deny) was done before item 8 (docs) so the docs could cite results.
- Docs: runbook.md, security-model.md, slos.md, rollout-checklist.md; deploy/architecture/test-plan/features updated.
- Final rerun found two more real issues: (1) a closed log pipe made `eprintln!` panic inside the server, so a dead log shipper could wedge the server and its SIGTERM shutdown — logging now drops lines on write errors, regression test P38 (fails on the old code, passes now); (2) `cmbd --help` started a server — cmbd now prints usage for -h/--help, version for -V, and rejects unknown commands (exit 2). Also added a hard shutdown deadline (grace + 5 s).
- Test fixes: gitimport sibling order is a hash tie-break (test made order-insensitive for same-generation parents); multinode waits for the P02 servers to finish draining before binding its ports; SBOM local paths rewritten to path+file:///src/.
