-- The content itself: one row per distinct hash.
CREATE TABLE files (
    id          INTEGER PRIMARY KEY,
    blake3      TEXT    NOT NULL UNIQUE, -- 64-char lowercase hex
    size        INTEGER NOT NULL,
    mime        TEXT,
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    state       TEXT    NOT NULL,        -- 'active' | 'archived' | 'trashed'
    imported_at INTEGER NOT NULL
);

-- Where copies live: many rows per file.
CREATE TABLE file_locations (
    id        INTEGER PRIMARY KEY,
    file_id   INTEGER NOT NULL REFERENCES files(id),
    path      BLOB    NOT NULL,          -- raw OS bytes, NOT lossy text
    mtime     INTEGER,
    present   INTEGER NOT NULL,          -- 1 = seen at last scan, 0 = missing
    last_seen INTEGER NOT NULL,
    UNIQUE(file_id, path)
);

CREATE INDEX idx_files_blake3 ON files (blake3);
CREATE INDEX idx_locations_file_id ON file_locations (file_id);
