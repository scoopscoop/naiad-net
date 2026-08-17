-- Tag completion performance (#50): keep current mapping counts materialized
-- per tag so typeahead does not GROUP BY over the full mappings table on every
-- keystroke.
CREATE TABLE tag_completion_counts (
    tag_id        INTEGER PRIMARY KEY REFERENCES tags(id) ON DELETE CASCADE,
    current_count INTEGER NOT NULL CHECK(current_count > 0)
);

INSERT INTO tag_completion_counts (tag_id, current_count)
SELECT tag_id, COUNT(*)
FROM mappings
WHERE status = 'current'
GROUP BY tag_id;

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
