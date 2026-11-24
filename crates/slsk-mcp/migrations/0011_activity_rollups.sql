-- Per-day rollups of share activity, which the trends read instead of the
-- event log: a week of trends is a sum over seven days of these rows rather
-- than a scan of every matched search. Triggers keep them current as
-- events, uploads and hourly search counts are written.

-- Who browsed, opened a folder or was sent a file, by day. Matched searches
-- are left out: the distributed network relays them from everyone, so they
-- say nothing about who came back.
CREATE TABLE visitor_days (
    day      TEXT NOT NULL,
    -- lower(username), so one person's spellings count once.
    user_key TEXT NOT NULL,
    username TEXT NOT NULL,
    events   INTEGER NOT NULL,
    last     TEXT NOT NULL,
    PRIMARY KEY (day, user_key)
) WITHOUT ROWID;

-- Each folder's opens and completed uploads, by day.
CREATE TABLE folder_days (
    day       TEXT NOT NULL,
    folder    TEXT NOT NULL,
    opens     INTEGER NOT NULL,
    downloads INTEGER NOT NULL,
    PRIMARY KEY (day, folder)
) WITHOUT ROWID;

-- search_terms summed by day, pruned to each day's most searched words as
-- hours end.
CREATE TABLE term_days (
    day      TEXT NOT NULL,
    term     TEXT NOT NULL,
    searches INTEGER NOT NULL,
    matched  INTEGER NOT NULL,
    PRIMARY KEY (day, term)
) WITHOUT ROWID;

INSERT INTO visitor_days (day, user_key, username, events, last)
SELECT substr(at, 1, 10), lower(username), MAX(username), COUNT(*), MAX(at)
FROM (
    SELECT username, at FROM share_events WHERE kind <> 'search'
    UNION ALL SELECT username, finished_at FROM uploads
)
GROUP BY 1, 2;

-- A file's folder is its path up to the last backslash: rtrim strips every
-- character but the backslash from the end.
INSERT INTO folder_days (day, folder, opens, downloads)
SELECT day, folder, SUM(opens), SUM(downloads)
FROM (
    SELECT substr(at, 1, 10) AS day, detail AS folder, 1 AS opens, 0 AS downloads
    FROM share_events WHERE kind = 'folder' AND detail IS NOT NULL
    UNION ALL
    SELECT substr(finished_at, 1, 10),
           CASE WHEN instr(filename, '\') = 0 THEN filename
                ELSE substr(rtrim(filename, replace(filename, '\', '')), 1,
                            length(rtrim(filename, replace(filename, '\', ''))) - 1) END,
           0, 1
    FROM uploads WHERE state = 'completed'
)
GROUP BY 1, 2;

INSERT INTO term_days (day, term, searches, matched)
SELECT substr(hour, 1, 10), term, SUM(searches), SUM(matched)
FROM search_terms GROUP BY 1, 2;

DELETE FROM term_days WHERE (day, term) IN (
    SELECT day, term FROM (
        SELECT day, term,
               row_number() OVER (PARTITION BY day ORDER BY searches DESC, term) AS n
        FROM term_days
    ) WHERE n > 2000
);

CREATE TRIGGER share_events_rollup AFTER INSERT ON share_events
WHEN NEW.kind <> 'search'
BEGIN
    INSERT INTO visitor_days (day, user_key, username, events, last)
    VALUES (substr(NEW.at, 1, 10), lower(NEW.username), NEW.username, 1, NEW.at)
    ON CONFLICT (day, user_key) DO UPDATE SET
        events = events + 1, last = max(last, excluded.last), username = excluded.username;
    INSERT INTO folder_days (day, folder, opens, downloads)
    SELECT substr(NEW.at, 1, 10), NEW.detail, 1, 0
    WHERE NEW.kind = 'folder' AND NEW.detail IS NOT NULL
    ON CONFLICT (day, folder) DO UPDATE SET opens = opens + 1;
END;

CREATE TRIGGER uploads_rollup AFTER INSERT ON uploads
BEGIN
    INSERT INTO visitor_days (day, user_key, username, events, last)
    VALUES (substr(NEW.finished_at, 1, 10), lower(NEW.username), NEW.username, 1, NEW.finished_at)
    ON CONFLICT (day, user_key) DO UPDATE SET
        events = events + 1, last = max(last, excluded.last), username = excluded.username;
    INSERT INTO folder_days (day, folder, opens, downloads)
    SELECT substr(NEW.finished_at, 1, 10),
           CASE WHEN instr(NEW.filename, '\') = 0 THEN NEW.filename
                ELSE substr(rtrim(NEW.filename, replace(NEW.filename, '\', '')), 1,
                            length(rtrim(NEW.filename, replace(NEW.filename, '\', ''))) - 1) END,
           0, 1
    WHERE NEW.state = 'completed'
    ON CONFLICT (day, folder) DO UPDATE SET downloads = downloads + 1;
END;

CREATE TRIGGER search_terms_insert_rollup AFTER INSERT ON search_terms
BEGIN
    INSERT INTO term_days (day, term, searches, matched)
    VALUES (substr(NEW.hour, 1, 10), NEW.term, NEW.searches, NEW.matched)
    ON CONFLICT (day, term) DO UPDATE SET
        searches = searches + excluded.searches, matched = matched + excluded.matched;
END;

CREATE TRIGGER search_terms_update_rollup AFTER UPDATE ON search_terms
BEGIN
    INSERT INTO term_days (day, term, searches, matched)
    VALUES (substr(NEW.hour, 1, 10), NEW.term, NEW.searches - OLD.searches, NEW.matched - OLD.matched)
    ON CONFLICT (day, term) DO UPDATE SET
        searches = searches + excluded.searches, matched = matched + excluded.matched;
END;
