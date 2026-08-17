-- Folders the daemon watches for live reindexing. `scan` registers a root here;
-- the daemon watches every registered root on startup.
CREATE TABLE roots (
    id       INTEGER PRIMARY KEY,
    path     BLOB    NOT NULL UNIQUE,   -- raw OS bytes, like file_locations.path
    added_at INTEGER NOT NULL           -- unix seconds, first registration
);
