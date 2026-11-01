# Cambium SLOs (proposed, to confirm with pilot data)

| SLI | objective | measured by |
|---|---|---|
| Availability of the API (non-5xx / all, excl. 429) | 99.9 % per 30 days | `cambium_http_requests_total` |
| Commit latency p95 | < 500 ms | `cambium_http_request_duration_seconds{route="commit"}` |
| Read (tree/blob) latency p95 | < 200 ms | same histogram, read routes |
| Audit write errors | 0 | `cambium_audit_write_errors_total` |
| RPO | ≤ 24 h (daily backup) — ≤ 15 min with Postgres PITR | backup job + DB WAL archiving |
| RTO | ≤ 1 h | restore drill timing |

Build-box baseline (not production hardware; client on the same 8-core box): 200 concurrent committers × 10 commits, 0 errors, p95 434.9 ms (SQLite) / 381.1 ms (Postgres), ~350–363 commits/s. See load-results.jsonl.

Alerting suggestions: error-budget burn rate (2 % in 1 h, 5 % in 6 h), readyz failing > 2 min, any audit write error, `cambium_db_pool_job_panics_total` increase, auth failures 10× baseline.
