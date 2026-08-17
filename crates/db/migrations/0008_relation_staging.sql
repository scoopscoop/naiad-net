-- The cursor of the last incremental relation pull per shared service.
-- NULL = never incrementally pulled (fresh, or an old repo that can't do deltas,
-- which keeps using the full-graph fallback).
ALTER TABLE services ADD COLUMN relation_cursor INTEGER;

-- Raw, per-service, multi-author mirror of the repo's authoritative relation
-- rows — tombstones and all. The collapse winner written into tag_siblings /
-- tag_parents is derived from this; this is the source of truth for recompute.
-- Tag text is stored (not interned ids) so staging is a faithful wire mirror and
-- tombstone-only edges never intern a tag.
CREATE TABLE service_relation_edges (
    service_id INTEGER NOT NULL,
    kind       TEXT    NOT NULL,   -- 'sibling' | 'parent'
    from_tag   TEXT    NOT NULL,
    to_tag     TEXT    NOT NULL,
    author     TEXT    NOT NULL,
    status     TEXT    NOT NULL,   -- 'current' | 'deleted'
    seq        INTEGER NOT NULL,
    PRIMARY KEY (service_id, kind, from_tag, to_tag, author)
);
CREATE INDEX service_relation_edges_from
    ON service_relation_edges(service_id, kind, from_tag);
