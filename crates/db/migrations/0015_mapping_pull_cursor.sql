-- Cursor state for incremental mapping bucket pulls.
-- mapping_cursor is the repo's submissions.seq high-watermark.
-- last_pull_file_marker is the max files.id whose buckets are known to be covered
-- by at least one full pull.
ALTER TABLE services ADD COLUMN mapping_cursor INTEGER;
ALTER TABLE services ADD COLUMN last_pull_file_marker INTEGER;
