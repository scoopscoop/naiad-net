-- ADR 0021 pivot: drop federated-trust client state. Plain (hash,tag,status,seq)
-- model from here on; no supporters, no trust scores, no petitions.

-- 1. Drop trust-version triggers that fire on surviving tables (trust_score_version
--    is being dropped, so any remaining trustver trigger would reference a dead table).

DROP TRIGGER IF EXISTS mappings_trustver_ai;
DROP TRIGGER IF EXISTS mappings_trustver_au;
DROP TRIGGER IF EXISTS mappings_trustver_ad;

DROP TRIGGER IF EXISTS block_rules_trustver_ai;
DROP TRIGGER IF EXISTS block_rules_trustver_au;
DROP TRIGGER IF EXISTS block_rules_trustver_ad;

-- 2. Drop the current mappings relver triggers before we rebuild the table.
--    (SQLite drops triggers automatically when the table is dropped, but we
--    drop them explicitly here for clarity and to avoid name conflicts later.)

DROP TRIGGER IF EXISTS mappings_relver_ai;
DROP TRIGGER IF EXISTS mappings_relver_au;
DROP TRIGGER IF EXISTS mappings_relver_ad;

-- 3. Drop federated tables. SQLite automatically removes triggers associated
--    with a dropped table, so author_trust_trustver_*, author_trust_relver_*,
--    supporters_trustver_*, and supporters_relver_* are gone implicitly.

DROP TABLE IF EXISTS mapping_supporters;   -- 0026; auto-drops supporters_* triggers
DROP TABLE IF EXISTS rejection_tools;      -- 0028
DROP TABLE IF EXISTS filed_petitions;      -- 0029
DROP TABLE IF EXISTS author_trust;         -- 0009; auto-drops author_trust_* triggers
DROP TABLE IF EXISTS trust_score_version;  -- 0021
DROP TABLE IF EXISTS tools;               -- 0020

-- 4. Clean up data in surviving tables.

DELETE FROM app_settings WHERE key = 'trust_floor';
DELETE FROM block_rules WHERE kind = 'author';

-- 5. Drop dead columns: contributor_mode from services, tool_id from staged_mappings.
--    Both are nullable additions with FK references to dropped tables — SQLite 3.35+
--    ALTER TABLE … DROP COLUMN handles these without the rename dance.

ALTER TABLE services DROP COLUMN contributor_mode;
ALTER TABLE staged_mappings DROP COLUMN tool_id;

-- 6. Rebuild mappings to remove author, signature, tool_id, supporter_total columns.
--    SQLite requires the create-copy-drop-rename dance to drop multiple columns at once.
--    Only file_id, tag_id, service_id, status, created_at survive.

CREATE TABLE mappings_new (
    file_id    INTEGER NOT NULL REFERENCES files(id),
    tag_id     INTEGER NOT NULL REFERENCES tags(id),
    service_id INTEGER NOT NULL REFERENCES services(id),
    status     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(file_id, tag_id, service_id)
);

INSERT INTO mappings_new (file_id, tag_id, service_id, status, created_at)
    SELECT file_id, tag_id, service_id, status, created_at FROM mappings;

DROP TABLE mappings;
ALTER TABLE mappings_new RENAME TO mappings;

-- Recreate the surviving indexes (idx_mappings_service_author and the authored
-- covering indexes are gone — they referenced dead columns).
CREATE INDEX idx_mappings_tag_id  ON mappings (tag_id);
CREATE INDEX idx_mappings_file_id ON mappings (file_id);

-- 7. Recreate all triggers on mappings.
--    (a) Completion-count materialization (from 0014) — auto-dropped with the
--        old table.  Recreate identically on the rebuilt table.

CREATE TRIGGER mappings_completion_counts_after_insert
AFTER INSERT ON mappings
WHEN NEW.status = 'current'
BEGIN
    INSERT INTO tag_completion_counts (tag_id, current_count)
    VALUES (NEW.tag_id, 1)
    ON CONFLICT(tag_id) DO UPDATE
    SET current_count = current_count + 1;
END;

CREATE TRIGGER mappings_completion_counts_after_delete
AFTER DELETE ON mappings
WHEN OLD.status = 'current'
BEGIN
    DELETE FROM tag_completion_counts
    WHERE tag_id = OLD.tag_id AND current_count = 1;

    UPDATE tag_completion_counts
    SET current_count = current_count - 1
    WHERE tag_id = OLD.tag_id AND current_count > 1;
END;

CREATE TRIGGER mappings_completion_counts_after_update
AFTER UPDATE OF tag_id, status ON mappings
WHEN OLD.status = 'current' OR NEW.status = 'current'
BEGIN
    DELETE FROM tag_completion_counts
    WHERE tag_id = OLD.tag_id AND OLD.status = 'current' AND current_count = 1;

    UPDATE tag_completion_counts
    SET current_count = current_count - 1
    WHERE tag_id = OLD.tag_id AND OLD.status = 'current' AND current_count > 1;

    INSERT INTO tag_completion_counts (tag_id, current_count)
    SELECT NEW.tag_id, 1
    WHERE NEW.status = 'current'
    ON CONFLICT(tag_id) DO UPDATE
    SET current_count = current_count + 1;
END;

-- (b) Relation-graph cache-invalidation triggers — now unguarded (the author
--     column is gone, so every mapping write is relevant).

CREATE TRIGGER mappings_relver_ai AFTER INSERT ON mappings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

CREATE TRIGGER mappings_relver_au AFTER UPDATE ON mappings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

CREATE TRIGGER mappings_relver_ad AFTER DELETE ON mappings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;

-- 8. First-v6 re-pull: wipe all pulled (shared) service mappings so the next pull
--    starts from a clean slate (v6 wire has no supporter metadata to backfill).
--    Local (scope = 'local') mappings are untouched. Reset the mapping_cursor so
--    the next pull fetches from seq = 0.

DELETE FROM mappings
    WHERE service_id IN (SELECT id FROM services WHERE url IS NOT NULL);

UPDATE services SET mapping_cursor = NULL WHERE url IS NOT NULL;
