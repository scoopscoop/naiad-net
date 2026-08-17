-- Namespace-completion performance (#70): materialize the count of
-- completion-eligible tags per namespace so `complete_namespaces` reads a tiny
-- table instead of scanning the full `tags` table (1.07M rows) and GROUP BY-ing
-- on every namespace typeahead. On a cold cache that scan cost ~125s.
--
-- A namespace's count is the number of DISTINCT tags in that namespace that have
-- a `tag_completion_counts` row (i.e. at least one current mapping). Membership
-- therefore changes exactly when a tag_id ENTERS or LEAVES tag_completion_counts,
-- so the triggers hang off that table's INSERT/DELETE. A `current_count` UPDATE
-- on an existing row does not change membership, so it needs no trigger.
CREATE TABLE tag_namespace_counts (
    namespace TEXT PRIMARY KEY,
    tag_count INTEGER NOT NULL CHECK(tag_count > 0)
);

INSERT INTO tag_namespace_counts (namespace, tag_count)
SELECT t.namespace, COUNT(*)
FROM tags t
JOIN tag_completion_counts c ON c.tag_id = t.id
WHERE t.namespace <> ''
GROUP BY t.namespace;

-- A tag became completion-eligible: bump its namespace tally.
CREATE TRIGGER namespace_counts_after_insert
AFTER INSERT ON tag_completion_counts
BEGIN
    INSERT INTO tag_namespace_counts (namespace, tag_count)
    SELECT namespace, 1 FROM tags WHERE id = NEW.tag_id AND namespace <> ''
    ON CONFLICT(namespace) DO UPDATE
    SET tag_count = tag_count + 1;
END;

-- A tag lost its last current mapping: drop it from its namespace tally,
-- removing the namespace row entirely when it hits zero.
--
-- The namespace-resolving subquery assumes the `tags` row still exists when this
-- fires. That holds today because tags are interned and never hard-deleted. If a
-- `DELETE FROM tags` path is ever added, `tag_completion_counts`' ON DELETE
-- CASCADE would fire this trigger after the parent row is already gone, the
-- subquery would return NULL, match nothing, and the tally would leak. Any such
-- path must therefore maintain `tag_namespace_counts` explicitly.
CREATE TRIGGER namespace_counts_after_delete
AFTER DELETE ON tag_completion_counts
BEGIN
    DELETE FROM tag_namespace_counts
    WHERE namespace = (SELECT namespace FROM tags WHERE id = OLD.tag_id)
      AND tag_count = 1;

    UPDATE tag_namespace_counts
    SET tag_count = tag_count - 1
    WHERE namespace = (SELECT namespace FROM tags WHERE id = OLD.tag_id)
      AND tag_count > 1;
END;
