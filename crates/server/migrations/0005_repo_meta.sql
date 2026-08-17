-- Key/value metadata table for the repo store (#194).
-- Stores opaque, operator-visible fields that do not belong in a typed column
-- (e.g. the store-generation id minted on every from-scratch re-seed).
--
-- WITHOUT ROWID: the table is tiny (O(10) keys at most) and keyed entirely by
-- the TEXT primary key; removing the hidden rowid avoids a second B-tree and
-- keeps all data in the PRIMARY KEY index.
CREATE TABLE repo_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
