-- Which changeset each imported git commit became, so `cmbd import-git` can run
-- again during a migration and import only what is new.
CREATE TABLE IF NOT EXISTS git_import_map (repo TEXT NOT NULL, git_sha TEXT NOT NULL, cs_id TEXT NOT NULL, PRIMARY KEY (repo, git_sha));
