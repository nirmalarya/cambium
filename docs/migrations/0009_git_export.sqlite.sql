-- Which git commit each exported changeset became, per mirror (cmbd git-export), so
-- exports are incremental and a mirror's history stays stable across runs.
CREATE TABLE IF NOT EXISTS git_export_map (repo TEXT NOT NULL, mirror TEXT NOT NULL, cs_id TEXT NOT NULL, git_sha TEXT NOT NULL, PRIMARY KEY (repo, mirror, cs_id));
