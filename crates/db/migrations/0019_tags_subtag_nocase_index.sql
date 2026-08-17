-- #71: `Db::complete_tags`' unnamespaced branch runs `subtag LIKE ?`, which SQLite
-- can only serve from an index whose collation is NOCASE — LIKE is case-insensitive
-- by default, so it ignores a BINARY index. The 0012 index (`idx_tags_subtag`,
-- BINARY) was therefore never used for its intended prefix scan: the query
-- full-scanned the ~1M-row `tags` table (~86s on a cold page cache), holding the
-- dedicated tag lane and stalling every tag/detail request behind it (#69-like
-- symptom, distinct root cause).
--
-- Replace the BINARY index with a NOCASE one so `subtag LIKE 'x%'` becomes an index
-- range scan (SEARCH, not SCAN). This also speeds the bare `subtag LIKE` paths used
-- by wildcard tag search. Subtags are stored normalized-lowercase (see
-- `naiad_core::tag::normalize`), so NOCASE vs BINARY is behaviorally identical on the
-- stored data — and no `subtag = ?` lookup exists without an accompanying
-- `namespace = ?` (those use `UNIQUE(namespace, subtag)`), so nothing depended on the
-- old BINARY index.
DROP INDEX IF EXISTS idx_tags_subtag;
CREATE INDEX idx_tags_subtag_nocase ON tags (subtag COLLATE NOCASE);
