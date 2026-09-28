-- A job fetched to replace a copy already filed (grab with refetch): its
-- import sets the filed copy aside rather than taking the new one for a
-- repeat of it.
ALTER TABLE jobs ADD COLUMN replaces BOOLEAN NOT NULL DEFAULT false;
