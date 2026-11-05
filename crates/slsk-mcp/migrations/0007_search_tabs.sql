-- Searches open as tabs on the Search page, each with the best of what it
-- found, so a batch of searches can be read one at a time and survives a
-- restart. `searched_at` is NULL while the search runs.
CREATE TABLE search_tabs (
    id INTEGER PRIMARY KEY,
    query TEXT NOT NULL,
    lossless INTEGER NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    searched_at TEXT,
    responses INTEGER NOT NULL DEFAULT 0,
    found INTEGER NOT NULL DEFAULT 0,
    folders TEXT,
    error TEXT
);
