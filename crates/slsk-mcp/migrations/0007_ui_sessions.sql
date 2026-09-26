-- UI sign-ins, so a deploy does not sign everyone out. The cookie carries a
-- random id; only its SHA-256 is stored, so a copy of this table signs no
-- one in.
CREATE TABLE ui_sessions (
    id_hash BYTEA PRIMARY KEY,
    sub TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX ui_sessions_expires_at ON ui_sessions (expires_at);
