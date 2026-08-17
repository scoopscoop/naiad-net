-- Imported file->tag records that may arrive before the file (or its sha256) is
-- known. Keyed by the SHA-256 interop hex; resolved into `mappings` once a file
-- with that sha256 exists. `status` mirrors mappings: 'current' | 'deleted'.
CREATE TABLE staged_mappings (
    sha256     TEXT    NOT NULL,           -- 64-char lowercase hex
    tag_id     INTEGER NOT NULL REFERENCES tags(id),
    service_id INTEGER NOT NULL REFERENCES services(id),
    status     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(sha256, tag_id, service_id)
);
CREATE INDEX idx_staged_mappings_sha256 ON staged_mappings (sha256);

-- Speeds the resolve join files.sha256 -> staged_mappings.sha256.
CREATE INDEX idx_files_sha256 ON files (sha256);
