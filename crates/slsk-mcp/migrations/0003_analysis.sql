-- Per-track spectral analysis, taken before import: where each track's
-- spectrum ends and what that suggests about its source.
ALTER TABLE jobs ADD COLUMN analysis JSONB;
-- Set by a person who has looked at the analysis and wants it imported
-- anyway.
ALTER TABLE jobs ADD COLUMN approved BOOLEAN NOT NULL DEFAULT FALSE;
