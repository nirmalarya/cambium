# Cambium operations runbook

Placeholders: `<cambium-host>`, `<namespace>`, `<release>`. No real hostnames, tenant ids or credentials belong in this file.

## 1. Health and triage
| signal | where | meaning |
|---|---|---|
| `GET /healthz` | liveness probe | the process is up; 200 even while draining |
| `GET /readyz` | readiness probe | the DB and blob store answer; 503 while draining or when either is down |
| `cambium_up`, `cambium_http_requests_total{route,status}`, `cambium_http_request_duration_seconds` | `/metrics` (protect with `CAMBIUM_METRICS_TOKEN`) | traffic, errors, latency |
| `cambium_auth_failures_total`, `cambium_rate_limited_total` | `/metrics` | credential problems or abuse |
| `cambium_db_pool_*` (incl. `cambium_db_pool_job_panics_total`), `cambium_audit_write_errors_total` | `/metrics` | database health; any audit write error is a page |
| JSON log lines (`CAMBIUM_LOG=info`) | stdout → log shipper | each line carries `request_id` and `actor`; the client sees the same id in `x-request-id` |

Triage order: readyz → error rate by route → the request id from the user's error message → the log line → the audit row with the same request id.

## 2. Tokens
- Issue: `cmbd token <name> --groups g1,g2 --ttl 90d --label <why>` (prints the secret once; only the sha256 is stored).
- List: `cmbd token list` (labels and expiry, never secrets).
- Rotate: `cmbd token rotate <name> --ttl 90d --grace 1h` (old token keeps working for the grace period; `--grace 0` cuts it off immediately).
- Revoke (suspected leak): `cmbd token revoke <name>`, then search the audit export for that actor.

## 3. Audit
- Export for the SIEM: `cmbd audit export --out audit.jsonl` (JSONL, `"product":"cambium"`). Schedule it, or set `CAMBIUM_AUDIT_FILE` to stream.
- Verify the hash chain: `cmbd audit verify`. Non-zero exit = tampering or a gap; treat as a security incident, keep the DB snapshot.
- The DB refuses UPDATE/DELETE on `audit_log` (triggers on SQLite; UPDATE/DELETE/TRUNCATE triggers on Postgres — a DB superuser can still drop them, so also restrict DB roles and ship the export off-box).

## 4. Backup and restore
- Backup: `cmbd backup <dir>` writes the metadata dump, the blobs and `manifest.json` with checksums.
- Restore: `cmbd restore <dir>` into an **empty** DB and blob store (it refuses a non-empty target and a backup whose checksums do not match).
- Drill: `tests/restore-drill.sh` (Postgres → fresh Postgres, plus SQLite → Postgres). Run it quarterly and after schema changes; last run on the build box: backup 676K, 216 rows, 42 blobs, restore 0.14 s.
- RPO/RTO targets are in slos.md.

## 5. Schema migrations
- `cmbd migrate --status` shows applied/pending. `cmbd migrate` applies them.
- In Kubernetes the Helm `pre-upgrade` Job runs `cmbd migrate`; pods run with `CAMBIUM_AUTO_MIGRATE=0` and refuse to start on a pending migration. A binary older than the DB refuses to start ("database is newer").
- Rollback = restore the pre-upgrade backup and redeploy the previous image. There are no down-migrations.

## 6. Deploy / restart
- `helm upgrade --install <release> deploy/helm/cambium -n <namespace> -f values-<env>.yaml`.
- Rolling update with `maxUnavailable: 0`; on SIGTERM a pod flips readyz to 503, drains for `CAMBIUM_SHUTDOWN_DRAIN_S`, then finishes in-flight requests within `CAMBIUM_SHUTDOWN_GRACE_S`.

## 7. Common incidents
| symptom | likely cause | action |
|---|---|---|
| 401 for everyone | IdP signing key rotated and JWKS unreachable | check egress to the JWKS URL or refresh `CAMBIUM_OIDC_JWKS_FILE` |
| 429s | `CAMBIUM_RATE`/`CAMBIUM_BURST` too low for CI fan-out | raise per-deployment, or give CI its own token |
| 409 on commit | someone moved the branch first | expected; the client rebases/retries |
| readyz 503, pool errors | Postgres down or pool exhausted | check DB, raise `CAMBIUM_PG_POOL` |
| merge button disabled | approvals missing or code owners pending | the review page lists the owners still needed |
| server stops logging / log shipper died | stderr pipe closed | cmbd keeps serving and drops log lines (P38); restart the shipper |
| `cmb mount` errors | `fusermount3` missing or no /dev/fuse | install fuse3; use `cmb checkout` instead |

## Derived data
Derived data (skip lists today) is computed when a changeset is created. After an
upgrade that adds or re-versions a derived type, backfill existing history with
`cmbd derive` (idempotent; safe while the server is running). Missing values are also
computed on first use, so the backfill only removes first-request latency.

## Migrating from git (incremental)
`cmbd import-git <git-dir> <repo> [owner]` imports full history. Run it again (e.g. from
cron) while teams still push to git: only new commits are imported and `main` /
`release/*` are fast-forwarded. Once people commit in Cambium, a ref that has diverged
is left alone and reported as skipped; at that point git is no longer the source of
truth for that ref. Only `main` and `release/*` refs are created.

## Git mirror (for git-based tools)
`cmbd git-export <repo> <git-dir>` writes `main` and `release/*` into a git repository
(created bare if missing). Each changeset becomes one commit with exactly its changes;
merges keep both parents. Run it from cron (or on each webhook delivery) and publish with
`git -C <git-dir> push --mirror <remote>`. It is incremental per mirror and existing
commits never change. A mirror contains every file, so repositories with permission
rules are refused unless `--allow-restricted` is passed deliberately. If the mirror
directory is deleted, the next run rebuilds it from scratch.
