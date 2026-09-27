-- What was uploaded, to whom, and how fast, kept past the engine's in-memory
-- list of recent transfers so the uploads page can show a history.
CREATE TABLE uploads (
    id BIGSERIAL PRIMARY KEY,
    username TEXT NOT NULL,
    filename TEXT NOT NULL,
    size BIGINT NOT NULL,
    bytes BIGINT NOT NULL,
    state TEXT NOT NULL,
    error TEXT,
    seconds DOUBLE PRECISION,
    finished_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX uploads_finished_at ON uploads (finished_at DESC);
