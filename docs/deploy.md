# Deploy: self-hosting

The recommended setup runs Cambium on a private network behind a TLS reverse proxy, not exposed directly to the public internet.

## Topology (target)
```
engineers / CI / agents ──(private network, VPN)──▶ internal reverse proxy (TLS, internal CA)
                                                      └──▶ cmbd :7878 (loopback / private subnet only)
                                                             ├─ metadata: /data/cambium.db (SQLite) or CAMBIUM_DB=postgres://… (multi-node)
                                                             └─ blobs: /data/blobs, CAMBIUM_BLOB_DIR (shared), or CAMBIUM_BLOB=s3
cmbd ──(outbound, internal only)──▶ identity provider JWKS endpoint (signing keys)
```
- **Ports:** `docker-compose.yml` binds `127.0.0.1:7878`. The reverse proxy is the only thing that reaches it. There is no public DNS record, no public load balancer and no port-forward.
- **TLS:** terminates at the internal proxy, using a certificate from the internal CA.
- **Client addresses:** behind your own reverse proxy, set `CAMBIUM_TRUST_PROXY=1` so the audit log and the per-IP limit for unauthenticated requests use the address your proxy appends to `X-Forwarded-For` (the rightmost entry). Without it, cmbd uses the TCP peer address and ignores `X-Forwarded-For`.
- **Outbound:** cmbd needs no internet egress. It only calls the identity provider's JWKS URL, which should be an internal or allow-listed endpoint. Alternatively, set `CAMBIUM_OIDC_JWKS_FILE` to a key file that is synced in, and cmbd makes no outbound calls at all.

## Authentication: single sign-on plus static tokens
| credential | for | how |
|---|---|---|
| OIDC token (JWT) from your identity provider | people (and agents acting as a person) | `CAMBIUM_TOKEN_CMD` runs your SSO sign-in helper (device-code or browser SSO) and prints an ID or access token. |
| static `cmbt_` token | CI, service accounts, break-glass admin | `docker compose run --rm cmbd token <name> --groups <g1,g2>`. Only the sha256 is stored. Rotate on a schedule. |

**Server settings** (placeholders are in `.env.example`; real values come from the app registration and are never committed):
- `CAMBIUM_OIDC_ISSUER`, `CAMBIUM_OIDC_AUDIENCE`: both must match exactly.
- `CAMBIUM_OIDC_JWKS_URL` or `CAMBIUM_OIDC_JWKS_FILE`: the provider's signing keys.
- `CAMBIUM_OIDC_USER_CLAIM`: default `preferred_username`, then `email` (only if `email_verified` is true), then `sub`. Usernames and emails can change or be reassigned; for stable identities set this to an immutable claim such as `sub` (or `oid` on Microsoft Entra ID) and write ACLs against those values.
- An OIDC login whose name equals a static-token account (`cmbd token <name>`) is refused, so an identity-provider claim can never inherit a service account's permissions. Give service accounts names that cannot collide with people (for example `ci-…`).
- JWKS keys are refetched on an unknown key id at most once every 5 minutes (failed attempts count), so an unreachable provider cannot stall sign-in.
- `CAMBIUM_OIDC_GROUPS_CLAIM`: default `groups`.

**What the server checks:**
- the signature against the JWKS (RS256/ES256 only; symmetric and "none" are refused);
- `iss`, `aud` and `exp`, with 60 s leeway;
- an unknown key id triggers a JWKS refetch, at most every 5 min, to follow key rotation.

**Groups:**
- If the provider emits group object ids rather than names, write ACL rules with those ids, or configure the app registration to emit names.
- For users in very many groups, some providers send an overage pointer instead of the list. Plan app roles or a filtered group claim for Cambium groups.

## Authorization: per-directory permissions
- **Default:** a repo with no rules is open to every signed-in user.
- **First rule:** the repo creator sets it, for example `cmb acl set / user:<owner> admin`. After that, only admins on `/` can change rules.
- **Rule shape:** `(prefix, principal, perm)`, where principal is `*`, `user:<name>` or `group:<name>`, and perm is `none`, `read`, `write` or `admin`. The longest matching prefix wins, so a narrower `none` hides a subtree.
- **Enforced on:**
  - **reads:** tree/checkout (filtered), file downloads (hidden files return 404), log and changeset lists (redacted), diff, and blame;
  - **writes:** commit, merge, rebase and ref moves (write on every changed path); deleting `main` needs admin;
  - **rule management:** the ACL endpoints, admins only.
- **Known limit:** file content is deduplicated by hash. If identical bytes exist at a readable path, a user may fetch them by id. That reveals only content they could already read.

## Preflight (2026-10-31, on the build box)
| check | result |
|---|---|
| cargo test --workspace | all pass; 5 Postgres tests run only with CAMBIUM_TEST_PG |
| tests/e2e.sh | 109 / 109 |
| tests/prod.sh | 38 / 38 |
| tests/multinode.sh (Postgres 16, two cmbd nodes) | 9 / 9 |
| tests/restore-drill.sh | 12 / 12 |
| tests/docker-smoke.sh | 13 / 13 |
| tests/k8s-check.sh | 17 / 17 |
| load (200 committers × 10) | 0 errors; p95 461.7 ms SQLite / 395.6 ms Postgres |
| clippy -D warnings, fmt --check, cargo audit, cargo deny | clean |

## Storage and scale settings
| setting | effect |
|---|---|
| `CAMBIUM_DB=postgres://user:pass@host/db` | Metadata in Postgres; any number of cmbd nodes share it. Ref moves are a single-row compare-and-swap, so concurrent writers never both win. `CAMBIUM_PG_POOL` (default 4) connections per node. Unset = SQLite in `CAMBIUM_DATA`. |
| `CAMBIUM_BLOB=s3` + `CAMBIUM_S3_ENDPOINT`, `CAMBIUM_S3_BUCKET`, `CAMBIUM_S3_REGION`, `CAMBIUM_S3_ACCESS_KEY`, `CAMBIUM_S3_SECRET_KEY`, `CAMBIUM_S3_PREFIX` | Blobs in an S3-compatible object store (path-style, SigV4). The reference setup is **RustFS** (see `docker-compose.yml`); CI runs the blob store and engine against a real RustFS (`tests/s3_live.rs`). cmbd creates the bucket at startup if missing. Credentials come from the secret store, never the repo. |
| `CAMBIUM_BLOB_DIR=/mnt/shared/blobs` | Blobs in a shared directory (e.g. an NFS mount) for multi-node without an object store. |
| `cmbd edge` + `CAMBIUM_UPSTREAM=https://<primary>` | Regional read-through cache with the same API. Blobs are kept locally after the first fetch; every cached read is re-authorised by the primary with the caller's own credentials. |
| client `CAMBIUM_SHARED_CACHE=/var/cache/cambium` | One object cache per build machine. Created 2770 (setgid group); add the machine's users to the directory's group. Entries are hash-verified on every read. Only share among users with similar read rights: a member of the group can list the directory. |

## Review UI, protected branches, virtual checkout, migration
- **Review UI:** `https://<cambium-host>/ui`. Users paste a token (or the output of the sign-in helper) once; it is kept in an HttpOnly, SameSite=Strict cookie scoped to `/ui`. Every form carries a per-session CSRF value. Put the UI behind the same internal proxy as the API.
- **Protected branches:** `cmb protect main 2`. After that, `main` changes only through a review with 2 approvals from people other than the author, given on the current head. Direct commit, merge, rebase and ref moves are refused (403).
- **Virtual checkout:** `cmb mount <repo> <dir>` (Linux, needs `fusermount3`) is a lazy view; files download on first read. Edits go to a local copy-on-write overlay; `cmb status`/`cmb commit` work inside the mount; unmounting keeps uncommitted work as a salvage copy. `--read-only` gives the old behaviour.
- **Code owners:** an `OWNERS` file (lines `user:<name>` / `group:<name>`) in a directory makes those principals required approvers for changes under it on protected branches. Rules are read from the target branch.
- **Git migration:** `cmbd import-git <git-dir> <repo> [owner]` imports every commit on branches and tags. In the container the mounted repo is owned by another uid, so pass `-e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0=<path>` (see tests/docker-smoke.sh).

## Kubernetes (preferred for production)
- Chart: `deploy/helm/cambium`. Values use placeholders (`*.example.internal`) and `existingSecret`; real values live in an environment-specific values file outside this repo.
- Migrations run in a `pre-upgrade` Job; pods set `CAMBIUM_AUTO_MIGRATE=0`.
- Pod: non-root 65532, read-only rootfs, drop ALL, seccomp RuntimeDefault, no service-account token, probes, resources, topology spread, PDB, NetworkPolicy, optional Ingress (TLS required) and ServiceMonitor.
- `import-git` uses the `tools` image target (has git) as a one-off Job; the runtime image is distroless.
- Verified only by `helm lint --strict` and kubeconform (1.28–1.31); not installed on a real cluster.
- Ops procedures: runbook.md. Security: security-model.md. SLOs: slos.md. Go-live: rollout-checklist.md.

## Run
```sh
cp .env.example .env        # fill in from the identity-provider app registration
docker compose up --build -d
docker compose run --rm cmbd token ci-bot --groups ci     # static token for CI
export CAMBIUM_SERVER=https://<cambium-host> CAMBIUM_TOKEN_CMD='<sso-helper> --print-token'
cmb whoami
```
On the build box, dockerd has no bridge network, so builds use `--network host` and compose uses a throwaway override in /tmp.

## Production checklist
See rollout-checklist.md. Items that could not be verified on the build box are listed there and in the final report.
