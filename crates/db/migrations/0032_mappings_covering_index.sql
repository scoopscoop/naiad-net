-- Covering index for file_ids_with_any_tag: the search hot path filters
-- WHERE tag_id IN (...) AND service_id IN (...) AND status = 'current'
-- and projects file_id. Column order enters the B-tree on tag_id (the most
-- selective predicate from the IN list), then narrows within each tag bucket
-- on service_id and the equality on status, and delivers file_id from the
-- index leaf without any table lookup.
CREATE INDEX idx_mappings_tag_svc_status_file
    ON mappings (tag_id, service_id, status, file_id);
