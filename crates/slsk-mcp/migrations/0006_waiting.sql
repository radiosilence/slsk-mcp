-- Times the job's import has been put off because a metadata service was
-- unavailable, kept with the job so an outage that spans a restart still
-- ends in `failed` after the limit.
ALTER TABLE jobs ADD COLUMN defer_tries INTEGER NOT NULL DEFAULT 0;
