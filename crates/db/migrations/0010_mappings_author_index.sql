-- Supports auto-derived trust scoring: a single grouped scan of an author's
-- pulled mappings per (service_id, author). Pure index, no schema change.
CREATE INDEX IF NOT EXISTS idx_mappings_service_author
    ON mappings (service_id, author);
