-- Search tabs are named by UUID rather than a row number, like jobs, and
-- ordered by when they were opened. Tabs already open keep their order.
CREATE TABLE search_tabs_new (
    id BLOB PRIMARY KEY,
    query TEXT NOT NULL,
    lossless INTEGER NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    opened_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    searched_at TEXT,
    responses INTEGER NOT NULL DEFAULT 0,
    found INTEGER NOT NULL DEFAULT 0,
    folders TEXT,
    error TEXT
);
INSERT INTO search_tabs_new (id, query, lossless, pinned, opened_at, searched_at, responses, found, folders, error)
SELECT randomblob(16), query, lossless, pinned,
       strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('-%d seconds', (SELECT max(id) FROM search_tabs) - id)),
       searched_at, responses, found, folders, error
FROM search_tabs;
DROP TABLE search_tabs;
ALTER TABLE search_tabs_new RENAME TO search_tabs;
