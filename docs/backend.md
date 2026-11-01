# Backend (replica-backend, adapted: no payments/email/OAuth — not part of a VCS server)

- **Auth:** `cmbd token <user>` mints `cmbt_<48 hex>`; only sha256 stored (`tokens.token_hash`), `revoked` flag honoured. Every route except `/v1/health` calls `user()`; 401 JSON otherwise. Tested: no header → 401, bad token → 401.
- **Write-ahead log:** `wal` table; every changeset append and every ref move writes a WAL row in the *same transaction* as its effect. Changeset id = sha256(repo, seq, payload).
- **CAS refs:** `UPDATE refs SET cs_id=new, version=version+1 WHERE repo=? AND name=? AND cs_id=expected`; create = `INSERT OR IGNORE`; delete = conditional DELETE. Row count ≠ 1 → `RefMoved{found}`. Changeset creation never touches refs.
- **Blob storage:** `BlobStore` trait (`put/get/has`); `FsBlobStore` (zlib, atomic tmp+rename, id validated), `MemBlobStore`. S3-compatible: implement the trait over an object-store client — documented gap.
- **Client cache:** `~/.cache/cambium`, integrity-checked on fetch (sha256 must equal id), size-capped by `CAMBIUM_CACHE_MAX`.

Security checklist
- [x] secrets only in env / token store; `.env` git-ignored
- [x] input validated on the server (paths, ref names, repo names, blob ids, missing blobs)
- [~] authorization on every call — authenticated yes; per-repo/per-directory ACLs **not yet**
- [ ] rate limits — not implemented (gap)
- [x] no user data in URLs beyond repo/ref names; tokens never logged
- [x] container runs as non-root uid 10001
- [ ] `cargo audit` — not run (tool not installed)


## Added 2026-10-31
- `GET /v1/blobs/:id` honours `Range: bytes=a-b|a-|-n` -> 206 + Content-Range, 416 if unsatisfiable; always sends Accept-Ranges.
- Rate limit middleware (all routes except /v1/health): token bucket keyed by sha256(Authorization). Env CAMBIUM_RATE (req/s, default 50, 0 = off), CAMBIUM_BURST (default 200). 429 body `{"error":"rate_limited","retry_after":N}` + Retry-After header.
- `changeset_files.mode` (100644 / 100755 / 120000; others rejected). Manifest entries encode mode as a suffix (`:x`, `:l`). Older databases get the column added on open.
