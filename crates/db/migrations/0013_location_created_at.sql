-- Best-effort filesystem creation time for the representative path shown in
-- gallery listings. Existing rows stay NULL until the file is re-scanned.
ALTER TABLE file_locations ADD COLUMN created_at INTEGER;
