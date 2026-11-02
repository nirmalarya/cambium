# Administration

## Deploy
- **Try it / small teams:** `docker compose up -d` (PostgreSQL + RustFS + cmbd), see
  [getting started](getting-started.md).
- **Production:** the Helm chart in `deploy/helm/cambium` (PostgreSQL, an S3-compatible object
  store such as RustFS, several stateless `cmbd` replicas behind your TLS ingress). Settings,
  TLS and networking: [`docs/deploy.md`](../deploy.md). Day-2 operations:
  [`docs/runbook.md`](../runbook.md).
- **Single machine without Docker:** `cmbd` alone uses SQLite and the local disk.

## Sign-in
- **People:** single sign-on (OIDC). Set `CAMBIUM_OIDC_ISSUER`, `CAMBIUM_OIDC_AUDIENCE`,
  `CAMBIUM_OIDC_JWKS_URL`; users set `CAMBIUM_TOKEN_CMD` to a helper that prints their token.
  Key identities on a stable claim (`CAMBIUM_OIDC_USER_CLAIM=sub`, or `oid` on Entra ID).
- **CI and service accounts:** static tokens, `cmbd token <name> [--groups a,b] [--ttl 90d]`;
  `cmbd token list | rotate <name> | revoke <name>`. Use names that cannot collide with
  people (e.g. `ci-…`).
- **Web UI:** users paste a token once; the session ends when the token is revoked.

## Repositories
Any signed-in user can create a repository and becomes its admin, unless you limit this with
`CAMBIUM_REPO_CREATORS=group:platform,user:alice`. Import existing git history with
`cmbd import-git <git-dir> <repo> [owner]` (run it again to pick up new git commits while
teams move over; only `main` and `release/*` become branches).

## Permissions
Rules are `prefix + principal + permission`; the longest matching prefix wins. A repository
with no rules is open (everyone may write).
```sh
cmb acl set / user:alice admin            # do this first: an admin rule for yourself
cmb acl set / group:eng read
cmb acl set services/pay group:pay-devs write
cmb acl set secrets group:eng none
cmb acl                                    # list
```
Or use **Settings** in the web UI. Hidden paths never appear anywhere for people without read.

## Protected branches and review rules
```sh
cmb protect main 2                 # main changes only through reviews with 2 approvals
cmb protect main 2 --trivial 1     # docs-only / comment-only reviews need 1
cmb protect                        # list
```
Code owners: an `OWNERS` file in a folder (`user:alice`, `group:payments`, one per line)
makes an approval from those owners required for changes under that folder.

## Webhooks for CI
```sh
cmb hook add https://ci.example.com/hooks/cambium --secret "$(openssl rand -hex 24)"
cmb hook list                      # delivery status and last error
```
Every move of `main` or `release/*` is POSTed (`repo`, `ref`, `before`, `after`,
`changesets`), signed in `X-Cambium-Signature: sha256=<HMAC>`; verify it with the secret and
de-duplicate on `X-Cambium-Delivery`. Delivery is ordered, at least once, retried with backoff.

## Git mirror
For tools that only speak git:
```sh
cmbd git-export <repo> /srv/mirrors/<repo>.git
git -C /srv/mirrors/<repo>.git push --mirror git@github.com:org/<repo>.git
```
Run it from cron or a webhook. Repositories with permission rules need
`--allow-restricted` (a mirror contains every file).

## Cache tiers
For many build machines or regions, put `cmbd edge` (per region) and `cmbd daemon` (per
machine) in front of the server: `CAMBIUM_UPSTREAM=<next tier>`; clients point
`CAMBIUM_SERVER` at the nearest tier. They serve immutable files and folders locally and still
check permissions upstream.

## Backups and upgrades
`cmbd backup <dir>` / `cmbd restore <dir>` (consistent snapshot, does not block the server);
`cmbd migrate` applies schema migrations (the Helm chart runs it before each upgrade);
`cmbd derive` backfills derived data after upgrades; `cmbd audit verify` checks the audit chain.
See [`docs/runbook.md`](../runbook.md).
