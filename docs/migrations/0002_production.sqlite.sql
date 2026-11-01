-- v2: production hardening. Applied once, inside a transaction, recorded in schema_migrations.
ALTER TABLE review_approvals ADD COLUMN groups TEXT NOT NULL DEFAULT '';
ALTER TABLE tokens ADD COLUMN expires_at INTEGER;
ALTER TABLE tokens ADD COLUMN label TEXT NOT NULL DEFAULT '';
-- Append-only audit trail; each row carries the hash of the previous row (tamper evidence).
CREATE TABLE audit_log (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  repo TEXT, target TEXT, outcome TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '{}', request_id TEXT,
  prev_hash TEXT NOT NULL, hash TEXT NOT NULL
);
CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit_log BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit_log BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
CREATE TABLE ui_sessions (id_hash TEXT PRIMARY KEY, user_name TEXT NOT NULL, groups TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE TABLE idempotency (user_name TEXT NOT NULL, key TEXT NOT NULL, response TEXT NOT NULL, created_at INTEGER NOT NULL,
  PRIMARY KEY (user_name, key));
