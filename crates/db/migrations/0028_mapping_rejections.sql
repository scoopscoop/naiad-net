-- ADR 0006's fourth, narrowest suppression kind: reject exactly one mapping --
-- this tag, on this file, from this repo (service). Reversible (row delete =
-- undo), purely local (never federated, no wire form, no status column: present
-- or absent, like block_rules), never applied to raw views. Keys on file_id, the
-- one thing block_rules cannot express. Aggregated by (service, tool) it is the
-- #90 per-tool scoring signal (ADR 0020 §6/§9).
CREATE TABLE mapping_rejections (
    service_id INTEGER NOT NULL REFERENCES services(id),
    file_id    INTEGER NOT NULL REFERENCES files(id),
    tag_id     INTEGER NOT NULL REFERENCES tags(id),
    kind       TEXT    NOT NULL DEFAULT 'mapping',  -- #87 reserves 'sibling'/'parent'
    note       TEXT,                                -- optional human reason
    created_at INTEGER NOT NULL,
    PRIMARY KEY (service_id, file_id, tag_id)
);

-- Per-#90: snapshot the supporting tool(s) of the rejected mapping AT rejection
-- time, so tool aggregation survives the mapping later vanishing from the pull.
-- Post-#85 a mapping may have several supporters -- one row per distinct tool.
-- tool_id NULL = manual/unknown. Deleted together with the parent on undo.
CREATE TABLE rejection_tools (
    service_id INTEGER NOT NULL REFERENCES services(id),
    file_id    INTEGER NOT NULL REFERENCES files(id),
    tag_id     INTEGER NOT NULL REFERENCES tags(id),
    tool_id    INTEGER REFERENCES tools(id),        -- NULL = manual/unknown
    PRIMARY KEY (service_id, file_id, tag_id, tool_id)
);

CREATE INDEX idx_reject_service ON mapping_rejections (service_id);
CREATE INDEX idx_rejection_tools_tool ON rejection_tools (service_id, tool_id);
