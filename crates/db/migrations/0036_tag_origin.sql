-- Generation-source origin (#162, ADR 0026). Revives ADR 0004 point 3 and the
-- interned shape of 0020_tool_provenance, WITHOUT the trust machinery
-- 0021 removed. origin_id is inert metadata: it must never appear in a read
-- predicate, index, trigger column list, or ON CONFLICT DO UPDATE SET (the
-- 0034/#151 perf rule — the AFTER UPDATE OF completion-count trigger fires on
-- the column LIST, not on value change).

-- 1. Interned origin names. Tiny, effectively static ('hydrus', 'wd14-tagger',
--    'gelbooru', …). NULL origin_id everywhere means "manual / unattested".
CREATE TABLE origins (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

-- 2. Per-service default origin (the ADR 0004 home). NULL = manual. Existing
--    services stay NULL (metadata-only ADD COLUMN, no backfill).
ALTER TABLE services ADD COLUMN origin TEXT;

-- 3. Nullable origin_id on mappings. Populated ONLY for pulled rows, from the
--    wire origin. NULL for local and for any pulled row whose upstream asserted
--    no origin. No index — origin_id is never a query key. ADD COLUMN is
--    metadata-only, so this costs nothing at rest.
ALTER TABLE mappings ADD COLUMN origin_id INTEGER REFERENCES origins(id);
