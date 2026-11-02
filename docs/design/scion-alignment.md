# Aligning Cambium with Scion's published design

Source: Anthropic, "Redesigning source control"
(https://www.anthropic.com/research/redesigning-source-control). Scion's code is not
public; this document maps each published design point to Cambium. Where the article
is silent, the choice made here is marked **(our choice)**.

## Principles (from the article)
1. Clients are always online.
2. The server does the heavy lifting.
3. Everything scales horizontally.
4. Commits are cheap and lock-free.
5. The working copy is optional.

## Mapping

| Scion (published) | Cambium today | Plan |
|---|---|---|
| Changeset = WAL entry listing files changed vs first parent, content-addressed compressed blobs | Same | Keep |
| Creating and fetching changesets never touch refs | Commits go through a branch ref | Changesets are created without a ref; landing on `main` is a separate server-side rebase + CAS |
| Refs: only `main` and release branches; ref move = CAS on one row | Arbitrary branches | Only `main` and `release/<name>`; CAS on one row (kept) |
| Fetch: client starts at a root tree, requests missing trees then blobs; immutable objects, resumable, no packfiles | Flat per-changeset file list (`/tree`) | Content-hashed trees per directory; `GET /v1/trees/:id`, `GET /v1/blobs/:id`; client walks from the root tree |
| Derived data: one generic mechanism, each type computed from its parents' values, eager or deferred (blame, path history, commit-graph skip lists) | Ad hoc (manifest cache, blame cache, generation numbers, path history) | A `Derived` trait with versioned types, eager/deferred scheduling, backfill; add skip lists |
| Server-side blame, log, merge-base APIs used by CI, bisect and revert pipelines | Blame, log, diff, merge-base | Keep; add bisect and revert endpoints **(our choice: as APIs)** |
| Sparse and virtualized checkouts; API-only writes | Sparse checkout, FUSE mount (Linux), API commits | Keep; mount reads trees lazily |
| Cache tiers: per-node daemon → regional cache → authoritative server, same RPCs, strict tiering | Shared cache directory, `cmbd edge` | `cmbd daemon` (per node) → `cmbd edge` (regional) → `cmbd` (authoritative), same API, each tier talks only to the next |
| Storage: Spanner + cloud object storage | SQLite or Postgres; filesystem or S3 | **PostgreSQL + RustFS** (S3-compatible) as the reference deployment; SQLite + filesystem for single node |
| Per-directory permissions | Same | Keep |
| Review (Antique): semantic approval rules, e.g. comment-only edits separated from logic | N approvals + path owners | Reviews point at a changeset, not a branch; add rules by change kind |
| No global merge queue; post-merge and pre-deploy testing | No queue | Keep; landing is lock-free rebase + CAS with retry |
| Git ingestion / migration | One-shot import | Import `main` and release branches; incremental re-import |
| Hermetic simulations, fuzzing, rate limiting | Property tests, rate limiting | Add a deterministic simulation harness and fuzz targets |
| No offline support | Removed the outbox | Done |

## Order of work
1. **Ref model:** only `main` and `release/*`. Changesets are created without refs, then land on `main` (server rebase + CAS). Reviews point at changesets.
2. **Trees and fetch protocol:** content-hashed directory trees, a recursive fetch from the root tree, and the client and mount moved onto it.
3. **Derived-data framework:** move manifests, blame and path history onto it; add skip lists.
4. **Cache tiers:** per-node daemon and regional edge with strict tiering.
5. **Review rules, bisect/revert APIs, git ingestion and the simulation harness.**
