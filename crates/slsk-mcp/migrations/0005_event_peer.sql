-- The peer a job was using when the outcome happened: the one that stalled,
-- failed or was left. What lets a peer that wasted a download be remembered
-- across restarts, not only by the process it wasted it for.
ALTER TABLE job_events ADD COLUMN peer TEXT;
