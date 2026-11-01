-- Cambium schema (SQLite dialect, single node; schema-pg.sql is the Postgres twin).
-- Owner of all rows is the repo; authorization is one check per request in the server.
CREATE TABLE IF NOT EXISTS tokens (
  token_hash TEXT PRIMARY KEY,           -- sha256 of the bearer token; raw token never stored
  user_name  TEXT NOT NULL,
  groups     TEXT NOT NULL DEFAULT '',   -- comma-separated group names (static tokens only)
  revoked    INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0,1)),
  created_at INTEGER NOT NULL            -- unix seconds, UTC
);
CREATE TABLE IF NOT EXISTS repos (
  name TEXT PRIMARY KEY,
  owner TEXT,                            -- creator; may set the first ACL rule
  created_at INTEGER NOT NULL
);
-- Per-directory permissions. No rows for a repo = open to every authenticated user.
-- principal: '*' | 'user:<name>' | 'group:<name>'. The longest matching prefix wins.
CREATE TABLE IF NOT EXISTS acl (
  repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  prefix TEXT NOT NULL,                  -- '' = whole repo, else a directory or file path
  principal TEXT NOT NULL,
  perm TEXT NOT NULL CHECK (perm IN ('none','read','write','admin')),
  PRIMARY KEY (repo, prefix, principal)
);
-- Write-ahead log: every mutation (changeset append, ref move) is an entry.
CREATE TABLE IF NOT EXISTS wal (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('changeset','ref')),
  payload TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS wal_repo_seq ON wal(repo, seq);
-- Changesets: files changed relative to first parent, pointing at blob ids.
CREATE TABLE IF NOT EXISTS changesets (
  id TEXT PRIMARY KEY,
  repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  seq INTEGER NOT NULL UNIQUE REFERENCES wal(seq),
  parent1 TEXT REFERENCES changesets(id) ON DELETE RESTRICT,
  parent2 TEXT REFERENCES changesets(id) ON DELETE RESTRICT,
  author TEXT NOT NULL,
  message TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  gen INTEGER NOT NULL DEFAULT 1         -- generation number: 1 + max(parent gens)
);
CREATE INDEX IF NOT EXISTS changesets_repo ON changesets(repo, seq);
CREATE INDEX IF NOT EXISTS changesets_p1 ON changesets(parent1);
CREATE TABLE IF NOT EXISTS changeset_files (
  cs_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  blob TEXT,                             -- NULL = deleted
  mode INTEGER NOT NULL DEFAULT 33188,   -- 0o100644 regular, 0o100755 executable
  PRIMARY KEY (cs_id, path)
);
CREATE INDEX IF NOT EXISTS changeset_files_path ON changeset_files(path);
-- Refs: one row per ref, moved only by compare-and-swap on cs_id.
CREATE TABLE IF NOT EXISTS refs (
  repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  name TEXT NOT NULL,
  cs_id TEXT NOT NULL REFERENCES changesets(id),
  version INTEGER NOT NULL,
  PRIMARY KEY (repo, name)
);
-- Derived data: recomputable caches keyed by (kind, changeset, key).
CREATE TABLE IF NOT EXISTS derived (
  kind TEXT NOT NULL,                    -- manifest | blame
  cs_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY (kind, cs_id, key)
);
CREATE INDEX IF NOT EXISTS changeset_files_blob ON changeset_files(blob);
-- Path history index: one row per (directory or file path, changeset that touched it).
CREATE TABLE IF NOT EXISTS path_history (
  repo TEXT NOT NULL, dir TEXT NOT NULL, gen INTEGER NOT NULL, cs_id TEXT NOT NULL,
  PRIMARY KEY (repo, dir, cs_id)
);
CREATE INDEX IF NOT EXISTS path_history_dir ON path_history(repo, dir, gen DESC);
-- Code review.
CREATE TABLE IF NOT EXISTS reviews (
  id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  source TEXT NOT NULL, target TEXT NOT NULL, title TEXT NOT NULL, author TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'open',    -- open | merged | closed
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS review_comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT, review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  author TEXT NOT NULL, path TEXT, body TEXT NOT NULL, created_at INTEGER NOT NULL
);
-- An approval counts only while the source head equals `head` (new pushes reset it).
CREATE TABLE IF NOT EXISTS review_approvals (
  review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE, user_name TEXT NOT NULL,
  head TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (review_id, user_name)
);
-- Protected branches: merges need N approvals from people other than the author.
CREATE TABLE IF NOT EXISTS protected_refs (
  repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE, name TEXT NOT NULL,
  required_approvals INTEGER NOT NULL, PRIMARY KEY (repo, name)
);
-- Blob sizes (bytes, uncompressed) so trees can be listed with sizes without reading blobs.
CREATE TABLE IF NOT EXISTS blob_meta (blob TEXT PRIMARY KEY, size INTEGER NOT NULL);
-- code search: inverted index keyed by content, shared across revisions
CREATE TABLE IF NOT EXISTS search_done (blob TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS search_terms (token TEXT NOT NULL, blob TEXT NOT NULL, PRIMARY KEY (token, blob));
