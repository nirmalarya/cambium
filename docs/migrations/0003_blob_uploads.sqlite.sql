-- Who uploaded which blob. Uploading the bytes proves you have them, so you may
-- reference that blob in a commit or diff even if a hidden path also uses it.
CREATE TABLE IF NOT EXISTS blob_uploads (
  blob TEXT NOT NULL,
  principal TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (blob, principal)
);
