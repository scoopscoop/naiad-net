-- Service priority drives cross-service conflict resolution (display merge spec).
-- Higher number = higher priority. The local service wins by default; pulled
-- shared services start at 0 and can be raised via `repo priority`.
ALTER TABLE services ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
UPDATE services SET priority = 1000 WHERE scope = 'local';
