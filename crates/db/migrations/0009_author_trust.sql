-- Manual, per-author/per-service trust weight (Phase 4 trust/reputation slice 1).
-- Unrated author = implicit weight 0. Signed; <0 = active distrust.
CREATE TABLE author_trust (
    service_id  INTEGER NOT NULL REFERENCES services(id) ON DELETE CASCADE,
    author      TEXT    NOT NULL,
    weight      INTEGER NOT NULL,
    note        TEXT,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (service_id, author)
);

-- Generic scalar key/value store for client settings. First key: 'trust_floor'.
CREATE TABLE app_settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
