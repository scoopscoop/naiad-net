-- The SHA-256 interop key (nullable). BLAKE3 stays the primary hash; sha256
-- exists only so a future Hydrus import can match files by SHA-256.
ALTER TABLE files ADD COLUMN sha256 TEXT;

-- The safety boundary (README §4/§7). Seeded with one local-only service.
CREATE TABLE services (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL              -- 'local' | 'shared'
);

-- Deduped tag dictionary, namespace split out.
CREATE TABLE tags (
    id        INTEGER PRIMARY KEY,
    namespace TEXT NOT NULL,         -- '' = unnamespaced
    subtag    TEXT NOT NULL,
    UNIQUE(namespace, subtag)
);

-- A tag attached to a file within a service.
CREATE TABLE mappings (
    file_id    INTEGER NOT NULL REFERENCES files(id),
    tag_id     INTEGER NOT NULL REFERENCES tags(id),
    service_id INTEGER NOT NULL REFERENCES services(id),
    status     TEXT    NOT NULL,     -- Phase 1 writes 'current'
    author     TEXT,                 -- NULL locally; set for network tags
    signature  BLOB,                 -- NULL locally
    created_at INTEGER NOT NULL,
    UNIQUE(file_id, tag_id, service_id)
);

-- Tag relations (ADR 0002). Created now for a frozen schema; NO code uses them
-- in this slice — query-time application lands in the relations spec.
CREATE TABLE tag_siblings (
    id           INTEGER PRIMARY KEY,
    bad_tag_id   INTEGER NOT NULL REFERENCES tags(id),
    ideal_tag_id INTEGER NOT NULL REFERENCES tags(id),
    service_id   INTEGER NOT NULL REFERENCES services(id),
    status       TEXT    NOT NULL,
    author       TEXT,
    signature    BLOB,
    created_at   INTEGER NOT NULL,
    UNIQUE(bad_tag_id, service_id)
);

CREATE TABLE tag_parents (
    id            INTEGER PRIMARY KEY,
    child_tag_id  INTEGER NOT NULL REFERENCES tags(id),
    parent_tag_id INTEGER NOT NULL REFERENCES tags(id),
    service_id    INTEGER NOT NULL REFERENCES services(id),
    status        TEXT    NOT NULL,
    author        TEXT,
    signature     BLOB,
    created_at    INTEGER NOT NULL,
    UNIQUE(child_tag_id, parent_tag_id, service_id)
);

CREATE INDEX idx_mappings_tag_id  ON mappings (tag_id);
CREATE INDEX idx_mappings_file_id ON mappings (file_id);
CREATE INDEX idx_siblings_ideal   ON tag_siblings (ideal_tag_id);
CREATE INDEX idx_parents_parent   ON tag_parents (parent_tag_id);

INSERT INTO services (id, name, scope) VALUES (1, 'my tags', 'local');
