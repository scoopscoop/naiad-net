-- Covering partial index for the authored-mapping scan in
-- `author_mapping_counts` (`author IS NOT NULL`): only indexes rows that are
-- actually authored (pulled from a repo), so a library with many local
-- (`author IS NULL`) mappings pays no penalty when computing auto-scores.
-- Also covers all columns the query selects, eliminating table lookups for
-- each result row.
--
-- Without this index, `author IS NOT NULL` on a 1M-row table where all rows
-- have `author IS NULL` forces SQLite to do a full table scan (~30ms on the
-- search_scale fixture). The partial index makes it O(authored_count), which
-- is O(1) when no authored rows exist and O(authored_count) in the normal
-- use case — always correct and fast regardless of ANALYZE state.
--
-- The existing `idx_mappings_service_author (service_id, author)` from
-- migration 0010 covers a `GROUP BY (service_id, author)` keyed scan; this
-- index supersedes it for the `author_mapping_counts` query path. Both
-- indexes are kept: the 0010 one covers INSERT-path deduplication and the
-- old `WHERE service_id = ? AND author IS NOT NULL` slice; this one covers
-- the full-authored-scan + adoption-count path.
CREATE INDEX IF NOT EXISTS idx_mappings_authored_covering
ON mappings (service_id, author, file_id, tag_id)
WHERE author IS NOT NULL;
