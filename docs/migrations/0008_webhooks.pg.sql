-- Webhooks: deliveries are read from the write-ahead log with a cursor per hook,
-- so they are ordered, at-least-once and survive restarts. A short lease makes one
-- cmbd node the deliverer of each hook at a time.
CREATE TABLE IF NOT EXISTS webhooks (
  id BIGSERIAL PRIMARY KEY,
  repo TEXT NOT NULL REFERENCES repos(name) ON DELETE CASCADE,
  url TEXT NOT NULL,
  secret TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  last_seq BIGINT NOT NULL,
  failures BIGINT NOT NULL DEFAULT 0,
  next_attempt BIGINT NOT NULL DEFAULT 0,
  last_error TEXT,
  lease_owner TEXT,
  lease_until BIGINT NOT NULL DEFAULT 0
);
