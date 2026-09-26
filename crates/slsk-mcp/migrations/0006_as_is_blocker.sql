-- Why an import as-is would be refused, checked when a job reaches review:
-- the UI then offers the action only when it can work, and says why not
-- otherwise. NULL is unchecked, '' passes.
ALTER TABLE jobs ADD COLUMN as_is_blocker TEXT;
