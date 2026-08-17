-- 0004_repo_hashes_explicit_index.sql — applied by rusqlite_migration to_latest (store.rs)
-- Replace repo_hashes' inline UNIQUE (an undroppable sqlite_autoindex) with an
-- explicit, droppable UNIQUE INDEX, so a fresh bridge seed can defer the
-- uniqueness B-tree build to the end of phase 1 (#187). Additive over 0003;
-- id values are preserved verbatim (repo_mappings.hash_id references them).

-- 1. New table: same columns, same rowid semantics (id INTEGER PRIMARY KEY),
--    WITHOUT the inline UNIQUE.
CREATE TABLE repo_hashes_new (id INTEGER PRIMARY KEY, hash BLOB NOT NULL);

-- 2. Copy every row, id explicit so first-seen ids are preserved exactly.
--    On a FRESH store this table is empty -> instant. On an existing populated
--    store this is a one-time bulk copy (operator-docs note §7).
INSERT INTO repo_hashes_new (id, hash) SELECT id, hash FROM repo_hashes;

-- 3. Swap.
DROP TABLE repo_hashes;
ALTER TABLE repo_hashes_new RENAME TO repo_hashes;

-- 4. Recreate uniqueness as a NAMED, droppable index.
CREATE UNIQUE INDEX repo_hashes_hash_unique ON repo_hashes(hash);
