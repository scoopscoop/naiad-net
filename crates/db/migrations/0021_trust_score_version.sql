-- Trust-score cache invalidation (measured ~425x regression on the auto
-- floor path, #search-scale): a monotonic version bumped by any write that
-- can change `effective_trust_map` (the auto-score baseline overlaid with
-- manual `author_trust` weights). Read-side caches compare against it before
-- reuse. Lives in SQLite so the writer connection's changes are visible to
-- the read-only connections, mirroring `relation_graph_version` (0016).
CREATE TABLE trust_score_version (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);
INSERT INTO trust_score_version (id, version) VALUES (1, 0);

-- author_trust: the manual weights that always override the auto baseline.
CREATE TRIGGER author_trust_trustver_ai AFTER INSERT ON author_trust
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER author_trust_trustver_au AFTER UPDATE ON author_trust
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER author_trust_trustver_ad AFTER DELETE ON author_trust
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;

-- block_rules: suppression counts feed the auto-score's suppression ratio.
CREATE TRIGGER block_rules_trustver_ai AFTER INSERT ON block_rules
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER block_rules_trustver_au AFTER UPDATE ON block_rules
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER block_rules_trustver_ad AFTER DELETE ON block_rules
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;

-- mappings: unlike `relation_graph_version`'s mapping triggers (which ignore
-- author IS NULL rows because local mappings never feed the sibling graph),
-- trust scoring's adoption signal is keyed off *local* mappings too
-- (`local_mapping_keys`, ADR 0019 #1: a pulled mapping is "adopted" when the
-- same (file_id, tag_id) exists locally). So a local mapping write can change
-- an authored mapping's adoption count even though the local row itself has
-- `author IS NULL` — the trigger must fire unconditionally, not just on
-- `author IS NOT NULL` rows.
CREATE TRIGGER mappings_trustver_ai AFTER INSERT ON mappings
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_trustver_au AFTER UPDATE ON mappings
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_trustver_ad AFTER DELETE ON mappings
BEGIN UPDATE trust_score_version SET version = version + 1 WHERE id = 1; END;

-- Note: no triggers on `tags`. Tag rows are insert-only (never UPDATEd or
-- DELETEd — see Db::intern_tag), so an existing mapping's tag_id always
-- resolves to the same immutable (namespace, subtag) text. A newly interned
-- tag cannot change whether any *existing* mapping matches a `tag_pattern`
-- block rule; it can only be referenced by a *future* mapping row, and that
-- future INSERT already bumps this version via the `mappings` triggers
-- above. This mirrors 0016_relation_graph_version.sql, which omits `tags`
-- triggers for the same reason.
