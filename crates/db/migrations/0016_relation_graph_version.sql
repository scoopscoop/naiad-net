-- Relation-graph cache invalidation (#64): a monotonic version bumped by any
-- write that can change the merged sibling/parent graph. Read-side caches
-- compare against it before reuse. Lives in SQLite so the writer connection's
-- changes are visible to the read-only connections.
CREATE TABLE relation_graph_version (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);
INSERT INTO relation_graph_version (id, version) VALUES (1, 0);

-- tag_siblings / tag_parents: the graph's edges.
CREATE TRIGGER tag_siblings_relver_ai AFTER INSERT ON tag_siblings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER tag_siblings_relver_au AFTER UPDATE ON tag_siblings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER tag_siblings_relver_ad AFTER DELETE ON tag_siblings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

CREATE TRIGGER tag_parents_relver_ai AFTER INSERT ON tag_parents
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER tag_parents_relver_au AFTER UPDATE ON tag_parents
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER tag_parents_relver_ad AFTER DELETE ON tag_parents
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

-- services: priority ordering feeds sibling merge precedence.
CREATE TRIGGER services_relver_ai AFTER INSERT ON services
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER services_relver_au AFTER UPDATE ON services
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER services_relver_ad AFTER DELETE ON services
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

-- author_trust + block_rules: feed effective_trust_map / auto-scores.
CREATE TRIGGER author_trust_relver_ai AFTER INSERT ON author_trust
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER author_trust_relver_au AFTER UPDATE ON author_trust
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER author_trust_relver_ad AFTER DELETE ON author_trust
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

CREATE TRIGGER block_rules_relver_ai AFTER INSERT ON block_rules
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER block_rules_relver_au AFTER UPDATE ON block_rules
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER block_rules_relver_ad AFTER DELETE ON block_rules
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

-- mappings: auto-scores are derived from AUTHORED mappings only. Local and
-- Hydrus-import rows (author IS NULL) never affect the merged graph, so they
-- must not invalidate it — bulk local imports stay trigger-cheap.
CREATE TRIGGER mappings_relver_ai AFTER INSERT ON mappings
WHEN NEW.author IS NOT NULL
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_relver_au AFTER UPDATE ON mappings
WHEN OLD.author IS NOT NULL OR NEW.author IS NOT NULL
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_relver_ad AFTER DELETE ON mappings
WHEN OLD.author IS NOT NULL
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
