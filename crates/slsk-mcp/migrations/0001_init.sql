-- Everything the service keeps across restarts, in one SQLite file beside
-- the library index.
--
-- Timestamps are UTC as ISO-8601 text ('2026-10-01T17:00:00.123+00:00'),
-- which sorts as it reads, so ranges over them use their indexes. Ids are
-- UUIDs as 16-byte blobs.

-- Credentials are sealed with SEAL_KEY (XChaCha20-Poly1305). Losing the key
-- loses only the ability to log back in without re-entering them.
CREATE TABLE accounts (
    username        TEXT PRIMARY KEY,
    sealed_password TEXT NOT NULL,
    -- The account the UI and header-less requests act as.
    active          INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
);
CREATE UNIQUE INDEX accounts_one_active ON accounts (active) WHERE active;

-- An album (or any set of files) on its way into the library.
CREATE TABLE jobs (
    id            BLOB PRIMARY KEY,
    account       TEXT NOT NULL,
    title         TEXT NOT NULL,
    -- {"kind":"soulseek","username":..,"folder":..} | {"kind":"files",..}
    source        TEXT NOT NULL,
    -- Other folders that matched the same request, tried in order if this
    -- one fails.
    alternates    TEXT NOT NULL DEFAULT '[]',
    status        TEXT NOT NULL,
    error         TEXT,
    import_log    TEXT,
    -- Releases the tagger offered when it could not decide.
    candidates    TEXT,
    library_path  TEXT,
    -- Per-track spectral analysis, taken before import.
    analysis      TEXT,
    -- Set by a person who looked at the analysis and wants it imported anyway.
    approved      INTEGER NOT NULL DEFAULT 0,
    -- Why an import as-is would be refused: NULL unchecked, '' passes.
    as_is_blocker TEXT,
    -- Fetched to replace a copy already filed (grab with refetch).
    replaces      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
);
CREATE INDEX jobs_status_created ON jobs (status, created_at DESC);
CREATE INDEX jobs_created ON jobs (created_at DESC);
-- A repeated grab finds the job it already started by title.
CREATE INDEX jobs_title ON jobs (lower(title), created_at DESC);

CREATE TABLE job_files (
    job_id BLOB NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    peer   TEXT NOT NULL,
    -- The peer's full virtual path, byte for byte: peers may send Latin-1,
    -- and a path re-encoded as UTF-8 names nothing on their side.
    remote BLOB NOT NULL,
    size   INTEGER NOT NULL,
    -- Directory under the job's staging dir the file lands in.
    subdir TEXT NOT NULL DEFAULT '',
    state  TEXT NOT NULL DEFAULT 'queued',
    error  TEXT,
    PRIMARY KEY (job_id, peer, remote)
) WITHOUT ROWID;

CREATE TABLE bans (
    account    TEXT NOT NULL,
    username   TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (account, username)
) WITHOUT ROWID;

-- Private messages, both directions. Kept: a conversation is worth having
-- after a restart, and the server only redelivers what was never acked.
CREATE TABLE messages (
    id       INTEGER PRIMARY KEY,
    account  TEXT NOT NULL,
    peer     TEXT NOT NULL,
    outgoing INTEGER NOT NULL,
    body     TEXT NOT NULL,
    at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    read     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX messages_peer ON messages (account, peer, at DESC);
CREATE INDEX messages_unread ON messages (account, peer) WHERE NOT read AND NOT outgoing;

-- Rooms to rejoin after every login; the server forgets them with the
-- session.
CREATE TABLE rooms (
    account TEXT NOT NULL,
    name    TEXT NOT NULL,
    PRIMARY KEY (account, name)
) WITHOUT ROWID;

CREATE TABLE buddies (
    account  TEXT NOT NULL,
    username TEXT NOT NULL,
    note     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (account, username)
) WITHOUT ROWID;

-- Searches repeated on the server's wishlist interval until something turns
-- up. With `grab`, the first relevant folder found becomes a job.
CREATE TABLE wishes (
    id          BLOB PRIMARY KEY,
    account     TEXT NOT NULL,
    query       TEXT NOT NULL,
    lossless    INTEGER NOT NULL DEFAULT 1,
    grab        INTEGER NOT NULL DEFAULT 0,
    searched_at TEXT,
    job_id      BLOB REFERENCES jobs (id) ON DELETE SET NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
);
CREATE INDEX wishes_account ON wishes (account, created_at);
CREATE INDEX wishes_job ON wishes (job_id) WHERE job_id IS NOT NULL;
-- Wishes still looking, least recently searched first.
CREATE INDEX wishes_open ON wishes (account, searched_at) WHERE job_id IS NULL;

-- Interests feed the server's recommendations; like rooms, the server holds
-- them only for the session.
CREATE TABLE interests (
    account TEXT NOT NULL,
    item    TEXT NOT NULL,
    liked   INTEGER NOT NULL,
    PRIMARY KEY (account, item)
) WITHOUT ROWID;

-- Every outcome a job reaches, kept after the job is retried or removed: the
-- record of what went wrong and which version did it.
CREATE TABLE job_events (
    id      INTEGER PRIMARY KEY,
    job_id  BLOB NOT NULL,
    title   TEXT NOT NULL,
    at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    version TEXT NOT NULL,
    -- imported, review, suspect, failed, fallback, deferred
    outcome TEXT NOT NULL,
    -- Why, from a fixed set; NULL for a clean import.
    cause   TEXT,
    detail  TEXT,
    -- The peer the job was using when the outcome happened.
    peer    TEXT
);
CREATE INDEX job_events_cause ON job_events (cause, at DESC);
CREATE INDEX job_events_job ON job_events (job_id, at DESC);
CREATE INDEX job_events_caused_at ON job_events (at DESC) WHERE cause IS NOT NULL;

-- UI sign-ins, so a deploy does not sign everyone out. The cookie carries a
-- random id; only its SHA-256 is stored.
CREATE TABLE ui_sessions (
    id_hash    BLOB PRIMARY KEY,
    sub        TEXT NOT NULL,
    expires_at TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX ui_sessions_expires_at ON ui_sessions (expires_at);

-- What was uploaded, to whom, and how fast, kept past the engine's in-memory
-- list of recent transfers.
CREATE TABLE uploads (
    id          INTEGER PRIMARY KEY,
    username    TEXT NOT NULL,
    filename    TEXT NOT NULL,
    size        INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    state       TEXT NOT NULL,
    error       TEXT,
    seconds     REAL,
    finished_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
);
CREATE INDEX uploads_finished_at ON uploads (finished_at DESC);

-- Lifetime totals and everyone an upload has reached. The rows they are
-- counted from are pruned or replaced; these only grow.
CREATE TABLE totals (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE served_users (
    username     TEXT PRIMARY KEY,
    first_served TEXT NOT NULL,
    last_served  TEXT NOT NULL,
    uploads      INTEGER NOT NULL,
    bytes        INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX served_users_last_served ON served_users (last_served DESC);
