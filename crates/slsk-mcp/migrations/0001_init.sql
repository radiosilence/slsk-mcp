-- Credentials are sealed with SEAL_KEY (XChaCha20-Poly1305). Losing the key
-- loses only the ability to log back in without re-entering them.
CREATE TABLE accounts (
    username        TEXT PRIMARY KEY,
    sealed_password TEXT NOT NULL,
    -- The account the UI and header-less requests act as.
    active          BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX accounts_one_active ON accounts (active) WHERE active;

-- An album (or any set of files) on its way into the library.
CREATE TABLE jobs (
    id          UUID PRIMARY KEY,
    account     TEXT NOT NULL,
    title       TEXT NOT NULL,
    -- {"kind":"soulseek","peer":..,"folder":..} | {"kind":"bandcamp","url":..}
    source      JSONB NOT NULL,
    -- Other folders that matched the same request, tried in order if this
    -- one fails.
    alternates  JSONB NOT NULL DEFAULT '[]',
    status      TEXT NOT NULL,
    error       TEXT,
    import_log  TEXT,
    -- Releases the tagger offered when it could not decide; importing with
    -- one of their ids resolves the job.
    candidates  JSONB,
    library_path TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX jobs_status ON jobs (status);

CREATE TABLE job_files (
    job_id  UUID NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    peer    TEXT NOT NULL,
    -- The peer's full virtual path, byte for byte: peers may send Latin-1,
    -- and a path re-encoded as UTF-8 names nothing on their side.
    remote  BYTEA NOT NULL,
    size    BIGINT NOT NULL,
    -- Directory under the job's staging dir the file lands in.
    subdir  TEXT NOT NULL DEFAULT '',
    state   TEXT NOT NULL DEFAULT 'queued',
    error   TEXT,
    PRIMARY KEY (job_id, peer, remote)
);

CREATE TABLE bans (
    account    TEXT NOT NULL,
    username   TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account, username)
);
