-- Replace `0023_mappings_authored_covering_index`'s partial index: it claimed
-- to be "covering" for `author_mapping_counts`'s query —
--   SELECT file_id, service_id, author, tag_id FROM mappings
--   WHERE author IS NOT NULL AND status = 'current'
-- — but `status` is neither an index column nor part of the partial
-- predicate, so SQLite must fetch the full table row for every entry that
-- matches `author IS NOT NULL` to re-check `status`. Not covering; the 0023
-- header's claim was false (caught in round-3 review). We do not edit `0023`
-- in place (migrations are an ordered, append-only list) — this drops it and
-- creates the corrected index under a new name.
--
-- Fix: extend the partial predicate with `AND status = 'current'`, rather
-- than adding `status` as a fifth index column. `status` is only ever
-- *filtered*, never *selected*, by this query (and by
-- `local_mapping_keys`, which the header already notes must not drift from
-- this one) — so putting it in the predicate instead of the column list
-- means the index holds only the rows this query (and its sibling) ever
-- wants, keeping it smaller and letting SQLite recognize the index as an
-- exact match for the query's `WHERE` clause without needing to store a
-- column that never appears in the SELECT list.
DROP INDEX IF EXISTS idx_mappings_authored_covering;

CREATE INDEX IF NOT EXISTS idx_mappings_authored_covering_current
ON mappings (service_id, author, file_id, tag_id)
WHERE author IS NOT NULL AND status = 'current';
