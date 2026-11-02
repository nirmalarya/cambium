-- Content-addressed directory trees (Scion-style fetch): one immutable object per
-- directory, shared by every changeset where that directory is unchanged.
CREATE TABLE IF NOT EXISTS trees (id TEXT PRIMARY KEY, body TEXT NOT NULL);
-- Where each tree occurs, so a tree is only served at a path the caller may see.
CREATE TABLE IF NOT EXISTS tree_paths (tree_id TEXT NOT NULL, repo TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY (tree_id, repo, path));
ALTER TABLE changesets ADD COLUMN root_tree TEXT;
