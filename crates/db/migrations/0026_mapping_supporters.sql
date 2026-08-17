-- Evidence preservation, client side (ADR 0020 §5, #85): mappings' one-author
-- limit made ten supporters indistinguishable from one and let a single
-- blocked/low-trust author hide a mapping a trusted supporter also asserted.
-- mappings becomes the per-(file, tag, service) membership + status row;
-- supporters move to a child relation, N per key, capped by the wire at
-- SUPPORTER_CAP but exact-in-total via supporter_total.
--
-- origin (#84): the supporter row's origin repo genesis key as the wire
-- reported it; NULL when the serving repo had no identity (or for rows
-- backfilled from pre-#85 data, which never carried origin).
CREATE TABLE mapping_supporters (
    file_id    INTEGER NOT NULL REFERENCES files(id),
    tag_id     INTEGER NOT NULL REFERENCES tags(id),
    service_id INTEGER NOT NULL REFERENCES services(id),
    author     TEXT    NOT NULL,
    tool_id    INTEGER REFERENCES tools(id),
    origin     TEXT,
    signature  BLOB,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (file_id, tag_id, service_id, author)
);

ALTER TABLE mappings ADD COLUMN supporter_total INTEGER NOT NULL DEFAULT 0;

INSERT INTO mapping_supporters
    (file_id, tag_id, service_id, author, tool_id, origin, signature, created_at)
SELECT file_id, tag_id, service_id, author, tool_id, NULL, signature, created_at
  FROM mappings WHERE author IS NOT NULL;

UPDATE mappings SET supporter_total = 1 WHERE author IS NOT NULL;
