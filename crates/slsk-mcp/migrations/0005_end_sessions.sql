-- The session cookie was renamed. Sessions minted under the old name would
-- otherwise stay valid until they expire if their id were presented under
-- the new one, beyond the reach of logout.
DELETE FROM ui_sessions;
