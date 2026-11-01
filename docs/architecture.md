# Cambium — architecture

Reads recon.md / features.csv. Template adapted: no web app, payments or email; this is a CLI + API server.

## 1. Stack
| layer | choice | why |
|---|---|---|
| server | Rust, axum + tokio (`cmbd`) | brief requires Rust; axum is small and boring |
| client | Rust, blocking ureq (`cmb`) | thin: no async runtime, no VCS logic beyond hashing files |
| metadata store | SQLite (WAL mode) for local dev, schema written to port to Postgres | one file, zero setup; Postgres gives row-level CAS the same way (`UPDATE … WHERE cs_id = $expected`) |
| blob store | `BlobStore` trait: `FsBlobStore` (zlib, content addressed), `MemBlobStore` (tests); S3-compatible backend = same trait (not shipped) | the original keeps file contents in object storage |
| auth | opaque bearer tokens, sha256-hashed at rest, minted by `cmbd token <user>` | simplest thing that is safe for agents and CI |
| deploy | one container (Dockerfile + compose), volume at /data | no hosting account needed |

One database, one server process. The original's fleet tiers (node daemon, regional caches) are not needed for parity of *features*; content addressing keeps the door open.

## 2. Schema
See `schema.sql` (also embedded in the server via `include_str!`). Tables: `tokens`, `repos`, `wal`, `changesets`, `changeset_files`, `refs`, `derived`.
- Times are unix seconds UTC. Ids are sha256 hex.
- `changesets.seq` references the `wal` row that created it: the WAL is the source of truth, the changeset tables are its queryable form, `derived` is a disposable cache.
- `refs` primary key (repo, name): moving a ref is a single-row conditional UPDATE — the CAS.
- `on delete`: repo deletion cascades; changeset parents RESTRICT (history is immutable).
- Authorization: one check per request in the server (`user()`); per-repo/per-directory ACLs are a listed gap.
- Postgres port: `INTEGER PRIMARY KEY AUTOINCREMENT` → `bigserial`, `TEXT` stays, `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING`.

## 3. API (all JSON, all require `Authorization: Bearer`, except health)
| method path | does | flow |
|---|---|---|
| GET /v1/health | liveness | — |
| POST /v1/repos | create repo, root changeset, `main` | F01 |
| GET /v1/repos/:r/refs | list refs | F03 |
| POST /v1/repos/:r/refs/:name `{expected,new}` | CAS create/move/delete | F03 F09 |
| GET /v1/repos/:r/resolve/:rev | ref or id prefix → id | F05 |
| POST /v1/blobs (raw) / GET /v1/blobs/:id | put/get content-addressed object | F02 F06 |
| POST /v1/repos/:r/commit `{ref,expected,message,files}` | append changeset + CAS ref | F02 |
| POST /v1/repos/:r/tree `{rev,prefixes}` | sparse manifest | F06 |
| POST /v1/repos/:r/merge `{target,source}` | server 3-way merge | F03 |
| POST /v1/repos/:r/rebase `{branch,onto}` | server replay + CAS | F04 F09 |
| GET /v1/repos/:r/log?rev&path&limit | first-parent history | F05 |
| GET /v1/repos/:r/blame?rev&path | derived blame | F05 |
| POST /v1/repos/:r/diff `{from,to|overlay,prefixes}` | diff revs, or rev vs working-copy overlay | F05 |
| GET /v1/repos/:r/merge-base?a&b | ancestry | F07 |
| GET /v1/repos/:r/changesets/:id | one changeset | F07 |
No webhooks or background jobs in the slice; derived data is computed lazily on first read and cached.

## 4. Parts that bite
- **Races:** two writers on one branch → exactly one CAS wins (tested with 8 threads); loser gets 409 `ref_moved` and a hint.
- **Idempotency:** blobs are content addressed, so re-uploads are free; changeset ids include the WAL seq so retries create new entries (by design, like the original).
- **Path safety:** server rejects `..`, absolute, empty segments and `.cambium`; blob ids validated as 64-hex before touching disk.
- **Large files:** 64 MiB request cap. **Binary files:** stored fine; text diff/merge treats them lossily (gap).
- **Deep history:** manifest/blame derivation walks back to the nearest cached ancestor, so cost scales with the delta after first use.

## 5. Build order
1. Vertical slice: init → add → commit → log (S01, S03–S05, S09).
2. Must-haves: branch, merge, rebase, blame, diff, sparse, API-only, cache (S06–S12).
3. Should-haves: derived caching, widen sparse; then the entrepreneur fix (offline outbox).
4. Gaps (parity.md): S3 backend, ACLs, path-history index, skip lists, virtual FS.

## Identity and permissions (added 2026-10-31)
- **Credential kinds:** `auth::authenticate` takes either a static `cmbt_` token (looked up by hash; groups stored with the token) or an OIDC JWT (verified against the provider's JWKS for issuer, audience and expiry; user and groups read from claims). Both produce one `Principal {user, groups}`.
- **Authorization:** `Engine::authz(repo, principal)` returns an `Authz` built from the `acl` table, using longest-prefix rules over `*`, `user:` and `group:` principals.
- **Where it is enforced:** the server filters reads (tree, log, diff, changeset), hides blobs, and checks writes before anything is written. Merge and rebase take a `check` callback that runs before the changeset is created.
- **Pluggability:** any OIDC-compliant IdP works; nothing is provider-specific. SAML-only setups would need an OIDC bridge in front.

## Storage, scale and the later features (added 2026-10-31)
- **Db layer (`cambium-core/src/db.rs`):** one SQL dialect (`?N` params, ON CONFLICT, RETURNING) on SQLite or Postgres. Postgres runs on worker threads, each owning a sync client; schema setup is serialised with an advisory lock.
- **Blobs:** `BlobStore` = filesystem (optionally shared dir), memory, or S3 (`s3.rs`: SigV4 over ureq, zlib, hash-checked).
- **Ancestry:** `changesets.gen` = 1 + max parent gen. Walks pop the highest gen first and prune below the target's gen. `path_history(repo, dir, gen, cs)` has one row per changed path prefix, so `log <path>` never walks unrelated history.
- **Reviews (`review.rs`):** reviews, comments, approvals (with the head they were given on), protected_refs. The server guards protected refs on commit/merge/rebase/CAS. The UI is server-rendered HTML in `cambium-server/src/review.rs`.
- **Search (`search.rs`):** `search_terms(token, blob)` + `search_done(blob)`; indexed lazily per blob, so it is shared across revisions. Candidates come from the index; results are verified line by line.
- **Edge (`cambium-server/src/edge.rs`):** transparent proxy; local blob copies; 1-byte upstream probe to re-check permission on each cached read.
- **Client:** `vfs.rs` (FUSE, read-only), `watch.rs` (inotify dirty list + baseline), shared cache (atomic rename, verify on read).
- **Import (`gitimport.rs`):** `git log --topo-order --reverse`, `diff-tree` vs first parent, `cat-file --batch`; changesets get the original timestamps.

## Production hardening (added 2026-10-31)
- **Writable mount (`cambium-cli/src/vfs.rs`):** FUSE (fuser 0.16) over a copy-on-write overlay in `<cache>-mounts/<hash>`: reads fall through to the lazy blob fetch; writes, creates, deletes (whiteouts), chmod, mkdir and rename stay in the overlay; `cmb status`/`commit` inside the mount diff the overlay against the base tree; unmount moves uncommitted overlay content to a salvage copy.
- **Code owners (`review.rs`):** `OWNERS` files parsed from the target branch; `owners_pending` = rules covering changed paths with no approval from a matching user/group (author excluded); merge blocked until empty; shown in API and UI.
- **Edge of the server:** request-id middleware, JSON access log, Prometheus registry (`obs.rs`), security headers, body cap, timeouts, idempotency cache, graceful drain, optional native TLS (axum-server 0.8 + rustls/ring).
- **DB layer:** versioned migrations (`docs/migrations`, `cmbd migrate`), pool workers wrapped in `catch_unwind` (panic → ROLLBACK + `cambium_db_pool_job_panics_total`), tolerant row decoding, statement timeout, Postgres TLS.
- **Audit (`audit.rs`):** hash-chained rows; Postgres appends under `pg_advisory_xact_lock` so multi-node writers keep one linear chain; append-only triggers.
- **Backup (`backup.rs`):** logical dump + blobs + checksummed manifest; restore only into an empty target; SQLite→Postgres supported.
- **Deploy:** distroless runtime image + `tools` image; Helm chart; CI workflow (not run; nothing pushed).
