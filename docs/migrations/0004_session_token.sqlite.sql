-- Link a UI session to the static token it was started with, so revoking or
-- expiring the token ends the session too. NULL for sessions started with an
-- OIDC JWT (those are capped at the JWT's own expiry instead).
ALTER TABLE ui_sessions ADD COLUMN token_hash TEXT;
