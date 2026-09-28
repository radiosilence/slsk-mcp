-- Lifetime totals and everyone an upload has reached. The uploads and
-- job_files rows they are counted from are pruned or replaced, and the
-- engine's own counters start again with each process; these only grow.
CREATE TABLE totals (
    name TEXT PRIMARY KEY,
    value BIGINT NOT NULL
);

CREATE TABLE served_users (
    username TEXT PRIMARY KEY,
    first_served TIMESTAMPTZ NOT NULL,
    last_served TIMESTAMPTZ NOT NULL,
    uploads BIGINT NOT NULL,
    bytes BIGINT NOT NULL
);
CREATE INDEX served_users_last_served ON served_users (last_served DESC);

-- What the history still holds, as the starting point.
INSERT INTO totals (name, value)
SELECT 'uploaded_bytes', COALESCE(SUM(bytes), 0) FROM uploads
UNION ALL
SELECT 'uploads_' || state, COUNT(*) FROM uploads GROUP BY state
UNION ALL
SELECT 'downloaded_bytes', COALESCE(SUM(size), 0) FROM job_files WHERE state = 'completed';

INSERT INTO served_users (username, first_served, last_served, uploads, bytes)
SELECT lower(username), MIN(finished_at), MAX(finished_at), COUNT(*), SUM(bytes)
FROM uploads WHERE state = 'completed'
GROUP BY lower(username);
