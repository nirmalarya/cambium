-- Cambium schema, Postgres dialect (multi-node). Same tables as schema.sql.
CREATE TABLE IF NOT EXISTS tokens (token_hash TEXT PRIMARY KEY, user_name TEXT NOT NULL, groups TEXT NOT NULL DEFAULT '',
  revoked BIGINT NOT NULL DEFAULT 0 CHECK (revoked IN (0,1)), created_at BIGINT NOT NULL);
CREATE TABLE IF NOT EXISTS repos (name TEXT PRIMARY KEY, owner TEXT, created_at BIGINT NOT NULL);
CREATE TABLE IF NOT EXISTS acl (repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE, prefix TEXT NOT NULL,
  principal TEXT NOT NULL, perm TEXT NOT NULL CHECK (perm IN ('none','read','write','admin')), PRIMARY KEY (repo, prefix, principal));
CREATE TABLE IF NOT EXISTS wal (seq BIGSERIAL PRIMARY KEY, repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('changeset','ref')), payload TEXT NOT NULL, created_at BIGINT NOT NULL);
CREATE INDEX IF NOT EXISTS wal_repo_seq ON wal(repo, seq);
CREATE TABLE IF NOT EXISTS changesets (id TEXT PRIMARY KEY, repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  seq BIGINT NOT NULL UNIQUE REFERENCES wal(seq), parent1 TEXT REFERENCES changesets(id) ON DELETE RESTRICT,
  parent2 TEXT REFERENCES changesets(id) ON DELETE RESTRICT, author TEXT NOT NULL, message TEXT NOT NULL,
  created_at BIGINT NOT NULL, gen BIGINT NOT NULL DEFAULT 1);
CREATE INDEX IF NOT EXISTS changesets_repo ON changesets(repo, seq);
CREATE INDEX IF NOT EXISTS changesets_p1 ON changesets(parent1);
CREATE TABLE IF NOT EXISTS changeset_files (cs_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE, path TEXT NOT NULL,
  blob TEXT, mode BIGINT NOT NULL DEFAULT 33188, PRIMARY KEY (cs_id, path));
CREATE INDEX IF NOT EXISTS changeset_files_path ON changeset_files(path);
CREATE INDEX IF NOT EXISTS changeset_files_blob ON changeset_files(blob);
CREATE TABLE IF NOT EXISTS refs (repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE, name TEXT NOT NULL,
  cs_id TEXT NOT NULL REFERENCES changesets(id), version BIGINT NOT NULL, PRIMARY KEY (repo, name));
CREATE TABLE IF NOT EXISTS derived (kind TEXT NOT NULL, cs_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
  key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (kind, cs_id, key));
CREATE TABLE IF NOT EXISTS path_history (repo TEXT NOT NULL, dir TEXT NOT NULL, gen BIGINT NOT NULL, cs_id TEXT NOT NULL,
  PRIMARY KEY (repo, dir, cs_id));
CREATE INDEX IF NOT EXISTS path_history_dir ON path_history(repo, dir, gen DESC);
CREATE TABLE IF NOT EXISTS reviews (id BIGSERIAL PRIMARY KEY, repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  source TEXT NOT NULL, target TEXT NOT NULL, title TEXT NOT NULL, author TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'open', created_at BIGINT NOT NULL);
CREATE TABLE IF NOT EXISTS review_comments (id BIGSERIAL PRIMARY KEY, review_id BIGINT NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  author TEXT NOT NULL, path TEXT, body TEXT NOT NULL, created_at BIGINT NOT NULL);
CREATE TABLE IF NOT EXISTS review_approvals (review_id BIGINT NOT NULL REFERENCES reviews(id) ON DELETE CASCADE, user_name TEXT NOT NULL,
  head TEXT NOT NULL, created_at BIGINT NOT NULL, PRIMARY KEY (review_id, user_name));
CREATE TABLE IF NOT EXISTS protected_refs (repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE, name TEXT NOT NULL,
  required_approvals BIGINT NOT NULL, PRIMARY KEY (repo, name));
CREATE TABLE IF NOT EXISTS blob_meta (blob TEXT PRIMARY KEY, size BIGINT NOT NULL);
-- code search: inverted index keyed by content, shared across revisions
CREATE TABLE IF NOT EXISTS search_done (blob TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS search_terms (token TEXT NOT NULL, blob TEXT NOT NULL, PRIMARY KEY (token, blob));
