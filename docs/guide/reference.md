# Reference

## Client (`cmb`)
Run `cmb help` for the full list. Settings (environment variables):

| Variable | Meaning |
|---|---|
| `CAMBIUM_SERVER` | server or nearest cache tier (default `http://127.0.0.1:7878`) |
| `CAMBIUM_TOKEN` / `CAMBIUM_TOKEN_CMD` | token, or a command that prints one (SSO helper) |
| `CAMBIUM_CACHE`, `CAMBIUM_CACHE_MAX` | local object cache folder and size limit (bytes) |
| `CAMBIUM_SHARED_CACHE` | one cache per machine, shared by its users |
| `CAMBIUM_FETCH_PARALLEL` | parallel downloads (default 8) |
| `CAMBIUM_MAX_RETRIES` | retries on transient errors (default 6) |
| `CAMBIUM_STATS=1` | print fetch statistics |
| `CAMBIUM_ALLOW_HTTP=1` | silence the warning about tokens over plain http |

## Server (`cmbd`)
| Command | |
|---|---|
| `cmbd` | run the server |
| `cmbd edge` / `cmbd daemon` | run a regional / per-machine cache tier (`CAMBIUM_UPSTREAM`) |
| `cmbd token <name> [--groups a,b] [--ttl 90d\|0] [--label text]` | mint a static token |
| `cmbd token list \| rotate <name> [--grace 24h] \| revoke <name>` | manage tokens (list never shows secrets) |
| `cmbd migrate [--status]` | apply / show schema migrations |
| `cmbd backup <dir>` / `cmbd restore <dir>` | consistent backup; restore into an empty database |
| `cmbd import-git <git-dir> <repo> [owner]` | import git history (incremental on re-run) |
| `cmbd git-export <repo> <git-dir> [--allow-restricted]` | git mirror of main + release/* |
| `cmbd derive` | backfill derived data |
| `cmbd audit export \| verify` | audit log |

All server settings (`CAMBIUM_DB`, `CAMBIUM_BLOB`, `CAMBIUM_S3_*`, OIDC, TLS, rate limits,
`CAMBIUM_TRUST_PROXY`, `CAMBIUM_REPO_CREATORS`, webhooks, …): [`docs/deploy.md`](../deploy.md).

## HTTP API (v1, JSON, `Authorization: Bearer <token>`)
| Endpoint | |
|---|---|
| `POST /v1/repos` | create a repository |
| `GET /v1/repos/:r/refs`, `POST /v1/repos/:r/refs/*name` | list refs; create/move/delete (compare-and-swap) |
| `GET /v1/repos/:r/resolve/*rev`, `GET /v1/repos/:r/root/*rev` | revision → changeset; changeset + root tree |
| `GET /v1/repos/:r/trees/:id?path=`, `POST /v1/repos/:r/trees` | one tree; many trees (one walk level) |
| `POST /v1/blobs`, `GET /v1/blobs/:id` | upload / download file content (ranges supported) |
| `POST /v1/repos/:r/changesets` | create a changeset (draft) |
| `POST /v1/repos/:r/land` | land a changeset on a branch (409 `busy` = retry) |
| `POST /v1/repos/:r/commit` | create + move a branch at an expected head |
| `POST /v1/repos/:r/rebase`, `/merge`, `/revert` | server-side rebase, merge, revert |
| `GET /v1/repos/:r/log`, `/blame`, `/search`, `/merge-base`, `/bisect` | history queries |
| `POST /v1/repos/:r/diff`, `POST /v1/repos/:r/tree` | diffs; flat file list |
| `/v1/repos/:r/reviews…` | create, show, comment, approve, update, merge, close |
| `/v1/repos/:r/acl`, `/protected`, `/hooks` | permissions, protection, webhooks |
| `GET /v1/whoami`, `/healthz`, `/readyz`, `/metrics` | identity, health, Prometheus metrics |

Write requests accept an `Idempotency-Key` header so retries are safe.
