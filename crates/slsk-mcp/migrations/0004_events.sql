-- Every outcome a job reaches, kept after the job is retried or removed: the
-- record of what went wrong and which version did it, so a cause can be
-- counted, traced to a fix, and the jobs it touched found again afterwards.
CREATE TABLE job_events (
    id       BIGSERIAL PRIMARY KEY,
    job_id   UUID NOT NULL,
    title    TEXT NOT NULL,
    at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    version  TEXT NOT NULL,
    -- imported, review, suspect, failed, fallback, deferred
    outcome  TEXT NOT NULL,
    -- Why, from a fixed set; NULL for a clean import.
    cause    TEXT,
    detail   TEXT
);

CREATE INDEX job_events_cause ON job_events (cause, at DESC);
CREATE INDEX job_events_job ON job_events (job_id, at DESC);
