-- ADR 0019 §4a follow-through for the supporters split (#85): authored
-- mapping evidence now lives in mapping_supporters, so the caches keyed to it
-- must be invalidated by writes THERE.
--
--  * trust_score_version: every supporter row is authored evidence feeding
--    auto-scores → bump on any supporters write. The 0021 unconditional
--    `mappings` triggers are LEFT IN PLACE: a membership write can still
--    change adoption via local_mapping_keys (local rows), and the extra bump
--    on pulled membership writes (which always accompany supporter writes) is
--    harmless over-invalidation, never a missed one. (The spec's "re-narrow to
--    local writes" is not expressible post-#85: mappings.author is dead-NULL,
--    so there is no per-row local/pulled signal for a trigger — plan decision.)
--  * relation_graph_version: every supporter row is authored → bump
--    unconditionally here. The 0024 `mappings` triggers (WHEN author IS NOT
--    NULL) go naturally inert once author is always NULL — correct: authored
--    graph-relevant writes now land in this table.
CREATE TRIGGER supporters_trustver_ai AFTER INSERT ON mapping_supporters
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER supporters_trustver_ad AFTER DELETE ON mapping_supporters
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER supporters_trustver_au AFTER UPDATE ON mapping_supporters
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;

CREATE TRIGGER supporters_relver_ai AFTER INSERT ON mapping_supporters
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER supporters_relver_ad AFTER DELETE ON mapping_supporters
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER supporters_relver_au AFTER UPDATE ON mapping_supporters
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

-- Drop the now-redundant index from 0025: the adoption scan
-- (author_mapping_counts) reads mapping_supporters in the SEARCH direction
-- (joined from mappings), where the PRIMARY KEY
-- (file_id, tag_id, service_id, author) is already a covering index for the
-- column set (file_id, service_id, author, tag_id). No replacement index is
-- needed — the PK autoindex serves the query. A (service_id, author)-leading
-- scan index could be added later if a different access pattern appears, but
-- only after measurement (maxim #12).
DROP INDEX IF EXISTS idx_mappings_authored_covering_current;
