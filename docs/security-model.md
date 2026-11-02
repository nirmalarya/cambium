# Cambium security model

## Trust boundaries
1. Engineer / CI machine → internal reverse proxy (TLS from the internal CA) → cmbd. cmbd can also terminate TLS itself (`CAMBIUM_TLS_CERT`/`CAMBIUM_TLS_KEY`; HSTS then on, plain HTTP refused).
2. cmbd → metadata DB (SQLite file or Postgres with `CAMBIUM_DB_TLS`/`CAMBIUM_DB_CA`).
3. cmbd → blob store (local dir, shared dir, or S3-compatible with SigV4).
4. cmbd → identity provider JWKS (only outbound call; avoidable with `CAMBIUM_OIDC_JWKS_FILE`).

## Authentication
- People: OIDC JWT, RS256/ES256 only, `iss`/`aud`/`exp` checked, JWKS refetched on unknown `kid` (rate-limited).
- Machines: static `cmbt_` tokens, sha256 at rest, TTL, label, rotate with grace, revoke.
- Web UI: token exchanged for a server-side session; cookie HttpOnly, SameSite=Strict, path `/ui`, `Secure` when TLS is on; CSRF value on every form; session TTL `CAMBIUM_SESSION_TTL`.

## Authorization
- Per-directory ACL (longest prefix wins; `none` hides subtrees, including in log/diff/blame/search/mount).
- Protected branches: N approvals from non-authors on the current head; direct writes refused.
- Code owners: `OWNERS` files on the **target** branch (so a review cannot rewrite its own rules); the author cannot satisfy them.

## Integrity and audit
- Content addressed by sha256; clients verify every blob they read, including from the shared cache.
- Append-only, hash-chained audit log of auth, ACL changes, ref moves and admin actions, each with actor and request id; UPDATE/DELETE blocked in the DB; `cmbd audit verify` detects tampering; concurrent multi-node writers serialised with an advisory lock so the chain stays linear.

## Hardening
- Security headers (CSP, X-Frame-Options DENY, nosniff, Referrer-Policy) on API and UI; body cap (`CAMBIUM_MAX_BODY_MB`) → 413; header and request timeouts; per-token rate limit; idempotency keys for commits.
- Container: distroless, uid 65532, read-only rootfs, all capabilities dropped, no-new-privileges, seccomp RuntimeDefault, no shell; SBOMs (CycloneDX) baked in.
- Supply chain: `cargo audit` 0 findings; `cargo deny` (advisories, licenses, bans incl. openssl/native-tls, sources crates.io only) passes; base images pinned by digest.

## Known limits / residual risk
- Content dedup: a user who can read identical bytes elsewhere can fetch them by hash (reveals nothing new).
- Shared machine cache: members of the cache group can list cached hashes.
- No at-rest encryption in cmbd itself; rely on the volume / object store encryption.
- Not yet pen-tested; threat model review by an independent security review is required before GA.

## Independent review, 2026-11-02
An adversarial review of the web UI, API, cache tiers, webhooks, git import/export and
client found one high and four medium issues, all fixed with regression tests
(`security_review_regressions`, e2e `Z10`):
- cache tiers reused an authorization for a filtered tree and could then serve the
  unfiltered cached copy (now only callers who saw the tree complete are remembered);
- the UI directory listing showed messages of changesets that touched only hidden files;
- write refusals named hidden paths (now only readable paths are named, the rest counted);
- webhook SSRF: DNS rebinding, missing metadata ranges, error text usable as a port-scan
  oracle (now checked at connect time, more ranges, error categories only);
- read-only users could create release branches at unreviewed drafts (now writers only,
  at history already on main, admins exempt).
Lower findings also fixed: log path oracle, reviews visible only to people who can read
every changed path, login CSRF (cross-site sign-in refused), unreferenced blobs readable
only by their uploader, webhook lease vs. delivery time and per-hook delivery threads,
optional `CAMBIUM_REPO_CREATORS`. Known, minor: a git commit message containing the byte
0x1E makes `cmbd import-git` stop with an error (no data corruption).
