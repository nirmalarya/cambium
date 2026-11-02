# Performance

Measured with `bash tests/load/run.sh` on the reference stack (docker compose:
PostgreSQL 16 + RustFS 1.0.4 + cmbd, release build) on a developer laptop running
Docker Desktop. All three containers were CPU-bound (~330% each), so the numbers are
a floor, not a ceiling. Raw results: `docs/load-results.jsonl`.

Workload: a 20,000-file monorepo in 500 directories; for 2 minutes, 32 agents land
small changes (1-3 files; 30% start from a slightly stale head, so the server rebases)
while 16 CI jobs check out (walk all 511 trees, fetch 200 blobs).

| | 2026-11-02 baseline | after fixes |
|---|---|---|
| lands per second | 2.0 | **11.8** (about 1M merges/day) |
| land request p50 / p99 | 4.4 s / 10.9 s | **0.50 s / 3.0 s** |
| failed lands | 56 of 325 | **0** (17 transient `busy` answers, retried) |
| CI checkout p50 | 135 s | **13.9 s** |
| checkouts in 2 minutes | 16 | **144** |

What changed: PostgreSQL pool 4 -> 16; rebases and the file/folder check work on
trees (cost follows the change, not the repository); landing backs off and answers a
retryable 409 `busy` instead of failing; tree walks fetch one directory level per
request; clients fetch blobs 8 at a time.

Next: measure on real servers, and checkouts through `cmbd edge`/`cmbd daemon` (cache
hits never reach PostgreSQL or the object store).
