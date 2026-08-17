-- 0003_intern_mappings.sql — applied by rusqlite_migration to_latest (store.rs)
-- Intern hashes/tags, compact repo_mappings to WITHOUT ROWID + INTEGER status,
-- drop redundant hash index. See spec §7.2 and design doc for peak-disk note.

-- 1. New interned tables (repo_mappings_new built WITHOUT its seq index; see §4.3).
CREATE TABLE repo_hashes (id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE);
CREATE TABLE repo_tags   (id INTEGER PRIMARY KEY, tag  TEXT NOT NULL UNIQUE);
CREATE TABLE repo_mappings_new (
  hash_id INTEGER NOT NULL REFERENCES repo_hashes(id),
  tag_id  INTEGER NOT NULL REFERENCES repo_tags(id),
  status  INTEGER NOT NULL CHECK (status IN (0,1)),
  seq     INTEGER NOT NULL,
  origin  TEXT,
  PRIMARY KEY (hash_id, tag_id)
) WITHOUT ROWID;

-- 2. Intern distinct hashes (hex TEXT -> 32-byte BLOB) and tags.
--    Plain INSERT (not OR IGNORE): DISTINCT already dedups, and every writer
--    emits lowercase 64-hex, so no UNIQUE collision is possible. Critically, a
--    corrupt (non-64-hex) legacy hash makes unhex() return NULL; a plain INSERT
--    then trips `repo_hashes.hash NOT NULL` and aborts the migration LOUDLY
--    (spec §7.2, R3). OR IGNORE would silently swallow that NULL and drop the
--    mapping in step 3's join — silent corruption, the opposite of the intent.
INSERT INTO repo_hashes(hash) SELECT DISTINCT unhex(hash) FROM repo_mappings;
INSERT INTO repo_tags(tag)    SELECT DISTINCT tag         FROM repo_mappings;

-- 3. Rewrite the mappings, decoding status TEXT -> INTEGER.
INSERT INTO repo_mappings_new(hash_id, tag_id, status, seq, origin)
SELECT h.id, t.id,
       CASE m.status WHEN 'deleted' THEN 1 ELSE 0 END,
       m.seq, m.origin
FROM   repo_mappings m
JOIN   repo_hashes h ON h.hash = unhex(m.hash)
JOIN   repo_tags   t ON t.tag  = m.tag;

-- 4. Swap. Dropping the old table releases idx_repo_mappings_hash AND the old
--    idx_repo_mappings_seq name, so the new index can reuse it (§4.3).
DROP TABLE repo_mappings;
ALTER TABLE repo_mappings_new RENAME TO repo_mappings;
CREATE INDEX idx_repo_mappings_seq ON repo_mappings(seq);
