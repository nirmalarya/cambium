# Cambium

Server-centric source control for repositories where most commits come from agents and CI.
The server creates changesets, merges, rebases, and answers log / blame / diff / merge-base / search.
The `cmb` client is thin: it lays out a (sparse or virtual) working copy, hashes files for `status`,
and keeps a bounded object cache.

> **Status: pilot, not production-ready.** See [Known issues](#known-issues) before deploying.

## Quick start (local)
```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
export CAMBIUM_DATA=./cambium-data
export CAMBIUM_TOKEN=$(cmbd token alice)   # mints a token (only its hash is stored)
cmbd &                                     # listens on 127.0.0.1:7878
cmb init demo && cd demo
echo hi > hello.txt && cmb add hello.txt && cmb commit -m "first"
cmb log; cmb blame hello.txt
```
Docker: `docker compose up --build -d`, then `docker compose exec cmbd cmbd token alice`.

## Commands
`init, checkout [--sparse], status, add, commit, branch, switch, sync, merge, rebase, log, blame,
diff, search, sparse, cat, mount [--write], watch, acl, protect, review, whoami, outbox, flush` — see `cmb help`.

## Sign-in and permissions
- **People:** sign in through your identity provider (OIDC). Set `CAMBIUM_TOKEN_CMD` to a helper that prints a token.
- **CI and service accounts:** static tokens, `cmbd token <name> --groups a,b`.
- **Per-directory permissions:** `cmb acl set services/pay group:pay-devs write`, `cmb acl set secret group:eng none`. Run `cmb whoami` to see who the server thinks you are.
- **Server settings:** see `docs/deploy.md` and `.env.example` (placeholders only). All settings use the `CAMBIUM_` prefix.

## Design
- **Changesets** list only files changed vs the first parent, with content-addressed blob ids; each is a row in a **write-ahead log** table.
- **Refs** move only by compare-and-swap on one row; creating a changeset never touches refs.
- **Derived data** (manifests, per-line blame, path history, generation numbers) is computed from parents and cached.
- **Blobs** go through a `BlobStore` trait: filesystem (zlib) or S3-compatible object storage.
- **Metadata:** SQLite (WAL mode) for a single node, Postgres for multiple nodes. Schemas in `docs/schema.sql` and `docs/schema-pg.sql`.
- **Deployment:** Docker image, Helm chart in `deploy/helm/cambium`.

## Prior art and attribution
The design follows the ideas Anthropic published about **Scion**, its in-house source control system
([Redesigning source control](https://www.anthropic.com/research/redesigning-source-control)):
always-online clients, server-side history and merges, changesets as write-ahead-log entries,
compare-and-swap refs, derived data computed from parents, and optional working copies.
Scion itself is not open source; Cambium is an independent implementation from the published
description and is not affiliated with or endorsed by Anthropic.
Scion in turn credits Meta's [Sapling](https://github.com/facebook/sapling) server (Mononoke) and its Bonsai data model.
Earlier centralized systems (CVS, Subversion, Perforce) and Google's Piper are also prior art.

## Known issues
Cambium is early alpha. Security and data-integrity hardening is in progress; open items include
rate-limit keying, filesystem blob-store write durability, three-way merges of binary files,
session revocation, and removal of the offline commit outbox (`outbox`/`flush`).
Do not use it for production data yet.

## License
[MIT](LICENSE)
