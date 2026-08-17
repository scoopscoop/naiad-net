-- Mapping pull state keyed by (service_id, domain) instead of by service alone
-- (ADR 0024 addendum 2026-07-27, spec §1). A client subscribed to a repo that
-- serves BOTH a BLAKE3 and a SHA-256 domain needs two independent cursors; the
-- two columns on `services` can hold only one, so the second domain's pull
-- would silently clobber the first's progress.
CREATE TABLE service_domain_pull_state (
    service_id            INTEGER NOT NULL REFERENCES services(id),
    domain                TEXT    NOT NULL,
    mapping_cursor        INTEGER,
    last_pull_file_marker INTEGER,
    PRIMARY KEY (service_id, domain)
);

-- Carry existing progress forward under 'blake3'. Every subscription that
-- exists today pulled exactly one domain, and all but bridge subscriptions are
-- native BLAKE3 repos, so this preserves incremental progress for them.
--
-- A mirror-mode SHA-256 subscription mis-files its cursor under a domain it
-- does not serve: that row is then never read, and its SHA-256 pull (which is
-- non-incremental in v1 regardless) simply starts fresh. One extra full pull,
-- no data loss, no wrong answers — provided the re-sync guard at
-- ops.rs:pull_repo fires when the new repo's sequence counter is lower than
-- the stored cursor (delta.cursor < stored_cursor → re-fetch with since=0).
-- That guard handles the case where a re-pointed repo has a lower seq, but it
-- does NOT clear state when re-pointing to a *different* URL; see
-- Db::set_service_url and Db::detach_service for that cleanup.
--
-- The legacy `services.mapping_cursor` / `services.last_pull_file_marker`
-- columns are deliberately left in place (SQLite column drops rewrite the
-- table); they are dead after this migration.
INSERT INTO service_domain_pull_state
    (service_id, domain, mapping_cursor, last_pull_file_marker)
SELECT id, 'blake3', mapping_cursor, last_pull_file_marker
FROM services
WHERE mapping_cursor IS NOT NULL OR last_pull_file_marker IS NOT NULL;
