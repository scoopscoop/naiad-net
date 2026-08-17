-- Startup rescan and watch reindex look locations up by path alone;
-- UNIQUE(file_id, path)'s composite autoindex cannot serve that, so every
-- touch_location was a full table scan — ~30 min per launch at 100k files.
CREATE INDEX idx_locations_path ON file_locations (path);
