-- What peers are shown when they ask about an account: its own words and
-- picture. The server keeps neither; each peer asks this client directly.
CREATE TABLE profiles (
    account     TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    picture     BLOB
);
