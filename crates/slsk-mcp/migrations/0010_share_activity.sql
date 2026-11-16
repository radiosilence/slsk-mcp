-- What other users do with our shares. Browses, opened folders and searches
-- that matched our shares are kept one row each; every search, matched or
-- not, is counted by its normalised words per hour in search_terms, keeping
-- only each hour's most frequent, since the distributed network relays far
-- more searches than are worth a row each.
CREATE TABLE share_events (
    id       INTEGER PRIMARY KEY,
    at       TEXT NOT NULL,
    -- browse, folder or search
    kind     TEXT NOT NULL,
    username TEXT NOT NULL,
    -- The folder opened, or the search's words.
    detail   TEXT,
    -- Files a search matched.
    results  INTEGER,
    -- A search the distributed network relayed, rather than the server.
    distributed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX share_events_at ON share_events (at DESC);
CREATE INDEX share_events_kind_at ON share_events (kind, at DESC);
CREATE INDEX share_events_user ON share_events (username, at);

CREATE TABLE search_terms (
    hour     TEXT NOT NULL,
    term     TEXT NOT NULL,
    searches INTEGER NOT NULL,
    -- Of those, the searches our shares answered.
    matched  INTEGER NOT NULL,
    PRIMARY KEY (hour, term)
) WITHOUT ROWID;

-- Every search counted, per hour, including those whose words were not kept.
CREATE TABLE search_hours (
    hour        TEXT PRIMARY KEY,
    searches    INTEGER NOT NULL,
    matched     INTEGER NOT NULL,
    distributed INTEGER NOT NULL
);
