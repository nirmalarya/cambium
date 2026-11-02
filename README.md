<p align="center"><img src="docs/brand/logo/cambium.svg" width="96" alt="Cambium logo"></p>

# Cambium

Server-centric source control for repositories where most commits come from agents and CI.
The server creates changesets, merges, rebases, and answers log / blame / diff / merge-base / search.
The `cmb` client is thin: it lays out a (sparse or virtual) working copy, hashes files for `status`,
and keeps a bounded object cache. Clients are always online: commits go straight to the server.

> **Status: pilot, not production-ready.** See [Known issues](#known-issues) before deploying.

**New here? Start with the [guide](docs/guide/README.md)** — concepts for git users, a
15-minute getting-started, everyday workflows, administration and reference.

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
`init, checkout [--sparse], status, add, commit [--draft], land, sync, branch, switch, merge, log,
blame, diff, search, sparse, cat, mount [--write], watch, acl, protect, review, revert, bisect, whoami` — see `cmb help`.

## Workflow (no feature branches)
- **Refs:** only `main` and `release/<name>`. Every ref move is a compare-and-swap on one row.
- **Commit:** `cmb commit -m msg` creates a changeset and lands it on `main`. If someone else landed
  first, the server rebases yours onto the new head (lock-free, no merge queue).
- **Drafts and review:** `cmb commit --draft` stacks changesets without touching any ref.
  `cmb review create -m title` opens a review of the drafts; `cmb review update <n>` points it at a
  revision. Protected refs only change through an approved review; merging it lands the changeset.
- **Stay current:** `cmb sync` updates the working copy; drafts are rebased onto the new head.
- **CI and git tools:** webhooks announce every move of `main`/`release/*` (`cmb hook add`), and
  `cmbd git-export` keeps a git mirror for tools that only speak git.

## Sign-in and permissions
- **People:** sign in through your identity provider (OIDC). Set `CAMBIUM_TOKEN_CMD` to a helper that prints a token.
- **CI and service accounts:** static tokens, `cmbd token <name> --groups a,b`.
- **Per-directory permissions:** `cmb acl set services/pay group:pay-devs write`, `cmb acl set secret group:eng none`. Run `cmb whoami` to see who the server thinks you are.
- **Server settings:** see `docs/deploy.md` and `.env.example` (placeholders only). All settings use the `CAMBIUM_` prefix.

## Design
- **Changesets** list only files changed vs the first parent, with content-addressed blob ids; each is a row in a **write-ahead log** table.
- **Refs** (`main`, `release/*`) move only by compare-and-swap on one row; creating a changeset never touches refs.
- **Derived data** (manifests, per-line blame, path history, generation numbers) is computed from parents and cached.
- **Blobs** go through a `BlobStore` trait: filesystem (zlib) or S3-compatible object storage.
- **Metadata:** SQLite (WAL mode) for a single node, Postgres for multiple nodes. Schemas in `docs/schema.sql` and `docs/schema-pg.sql`.
- **Deployment:** Docker image, Helm chart in `deploy/helm/cambium`.

See `docs/design/roadmap.md` for the design points and how they were built.

## Prior art and attribution
Cambium is an independent implementation inspired by the design Anthropic described in
[Redesigning source control](https://www.anthropic.com/research/redesigning-source-control); it
is not affiliated with or endorsed by Anthropic. Earlier prior art includes Meta's
[Sapling](https://github.com/facebook/sapling) server, Google's Piper, and CVS, Subversion and Perforce.

## Known issues
Cambium is early alpha: the security and data-integrity fixes from the first review have landed,
but it has not had an independent security review, and storage still keeps a full file list per
changeset (being replaced by content-hashed trees). Do not use it for production data yet.

## License
[MIT](LICENSE)
