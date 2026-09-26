-- Private messages, both directions. Kept: a conversation is worth having
-- after a restart, and the server only redelivers what was never acked.
CREATE TABLE messages (
    id         BIGSERIAL PRIMARY KEY,
    account    TEXT NOT NULL,
    peer       TEXT NOT NULL,
    outgoing   BOOLEAN NOT NULL,
    body       TEXT NOT NULL,
    at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    read       BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX messages_peer ON messages (account, peer, at DESC);

-- Rooms to rejoin after every login; the server forgets them with the
-- session.
CREATE TABLE rooms (
    account TEXT NOT NULL,
    name    TEXT NOT NULL,
    PRIMARY KEY (account, name)
);

CREATE TABLE buddies (
    account  TEXT NOT NULL,
    username TEXT NOT NULL,
    note     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (account, username)
);

-- Searches repeated on the server's wishlist interval until something turns
-- up. With `grab`, the first relevant folder found becomes a job.
CREATE TABLE wishes (
    id          UUID PRIMARY KEY,
    account     TEXT NOT NULL,
    query       TEXT NOT NULL,
    lossless    BOOLEAN NOT NULL DEFAULT TRUE,
    grab        BOOLEAN NOT NULL DEFAULT FALSE,
    searched_at TIMESTAMPTZ,
    job_id      UUID REFERENCES jobs (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Interests feed the server's recommendations; like rooms, the server holds
-- them only for the session.
CREATE TABLE interests (
    account TEXT NOT NULL,
    item    TEXT NOT NULL,
    liked   BOOLEAN NOT NULL,
    PRIMARY KEY (account, item)
);
