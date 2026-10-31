-- How the job's latest import was asked to decide the album: a release a
-- person chose, or the files' own tags with their corrections. A restart
-- resumes an import with the same decision instead of matching afresh.
ALTER TABLE jobs ADD COLUMN import_how TEXT;

-- Complete copies the wish's latest search found. A wish that does not grab
-- keeps searching; this is what says one has turned up.
ALTER TABLE wishes ADD COLUMN found INTEGER;
