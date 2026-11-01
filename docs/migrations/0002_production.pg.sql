-- v2: production hardening (Postgres). Applied once, inside a transaction, recorded in schema_migrations.
ALTER TABLE review_approvals ADD COLUMN groups TEXT NOT NULL DEFAULT '';
ALTER TABLE tokens ADD COLUMN expires_at BIGINT;
ALTER TABLE tokens ADD COLUMN label TEXT NOT NULL DEFAULT '';
CREATE TABLE audit_log (
  seq BIGSERIAL PRIMARY KEY, ts BIGINT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  repo TEXT, target TEXT, outcome TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '{}', request_id TEXT,
  prev_hash TEXT NOT NULL, hash TEXT NOT NULL
);
CREATE FUNCTION cambium_audit_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'audit_log is append-only'; END $$;
CREATE TRIGGER audit_no_change BEFORE UPDATE OR DELETE ON audit_log FOR EACH ROW EXECUTE FUNCTION cambium_audit_append_only();
CREATE TRIGGER audit_no_truncate BEFORE TRUNCATE ON audit_log FOR EACH STATEMENT EXECUTE FUNCTION cambium_audit_append_only();
CREATE TABLE ui_sessions (id_hash TEXT PRIMARY KEY, user_name TEXT NOT NULL, groups TEXT NOT NULL DEFAULT '',
  created_at BIGINT NOT NULL, expires_at BIGINT NOT NULL);
CREATE TABLE idempotency (user_name TEXT NOT NULL, key TEXT NOT NULL, response TEXT NOT NULL, created_at BIGINT NOT NULL,
  PRIMARY KEY (user_name, key));
