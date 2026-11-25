-- Library actions asked for from the web UI, in the order they run, so a
-- restart picks them up where they stopped. `paths` and `attached` are as
-- the action was asked for (JSON); each album finished is a row of
-- `library_queue_done`, with where it moved, and is not run again.
CREATE TABLE library_queue (
    ticket INTEGER PRIMARY KEY,
    action TEXT NOT NULL,
    paths TEXT NOT NULL,
    attached TEXT NOT NULL,
    split TEXT NOT NULL
);
CREATE TABLE library_queue_done (
    ticket INTEGER NOT NULL,
    path TEXT NOT NULL,
    moved_to TEXT,
    PRIMARY KEY (ticket, path)
);
