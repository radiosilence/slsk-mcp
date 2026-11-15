-- When a job put off for a metadata service is tried again, set once when
-- it is put off, so the schedule and what the job shows agree. Jobs already
-- waiting keep the five minutes they were promised.
ALTER TABLE jobs ADD COLUMN retry_at TEXT;
UPDATE jobs SET retry_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', updated_at, '+300 seconds') WHERE status = 'waiting';
CREATE INDEX jobs_retry_at ON jobs (retry_at) WHERE status = 'waiting';
