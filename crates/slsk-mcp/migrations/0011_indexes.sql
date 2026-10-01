-- Indexes for the queries the service runs on every UI tick and job loop.

-- Job lists: newest first, all or one status.
DROP INDEX IF EXISTS jobs_status;
CREATE INDEX jobs_status_created ON jobs (status, created_at DESC);
CREATE INDEX jobs_created ON jobs (created_at DESC);
-- A repeated grab finds the job it already started by title.
CREATE INDEX jobs_title ON jobs (lower(title), created_at DESC);

-- Unread counts, per account and across all of them.
CREATE INDEX messages_unread ON messages (account, peer) WHERE NOT read AND NOT outgoing;

-- Triage reads every event with a cause since a date, newest first.
CREATE INDEX job_events_caused_at ON job_events (at DESC) WHERE cause IS NOT NULL;

-- Deleting a job clears its wish (ON DELETE SET NULL), and an import
-- deletes the wish it satisfied.
CREATE INDEX wishes_job ON wishes (job_id) WHERE job_id IS NOT NULL;
CREATE INDEX wishes_account ON wishes (account, created_at);
