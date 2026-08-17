-- Per-domain provenance for pulled mappings (#151).
--
-- A repo that advertises more than one hash domain sends the same service_id
-- rows from two independent sources. Until now a row recorded no trace of which
-- domain supplied it, so neither domain's merge could be authoritative without
-- destroying the other's rows — the reason `pull_repo`'s multi-domain arm
-- coalesces everything into one whole-service `merge_pulled_mappings` and pays
-- a full snapshot merge on every pull, forever.
--
-- `domains` is a bitmask of the domains that CURRENTLY supply a row:
--
--     bit 0 (1) = blake3   -- also the value for local (non-pulled) rows
--     bit 1 (2) = sha256
--     3         = supplied by both domains
--
-- A mask, not a discriminator column, deliberately: keeping one row per
-- (file_id, tag_id, service_id) leaves the UNIQUE constraint, every read query,
-- and the tag_completion_counts triggers untouched. A discriminator would need
-- two rows for a tag both domains supply, which means widening UNIQUE (a full
-- table rebuild of the largest table in the schema), adding DISTINCT to the
-- read path, and teaching the count triggers to count distinct (file, tag).
--
-- Each domain's merge sets or clears only its own bit and deletes a row only
-- once the mask reaches 0, so neither leg can drop rows the other still
-- supplies. `domains` never appears in a read predicate, so no index changes
-- are needed.
--
-- DEFAULT 1 is correct for LOCAL rows: they have no hash domain, and the mask
-- only needs to be non-zero so the pull path's reap can never reach them.
--
-- It is NOT safely inferable for pulled rows, which is why they are discarded
-- below rather than backfilled. A pre-migration row records nothing about its
-- source, and guessing the native domain is wrong in both directions:
--
--   * a dual-domain subscription's sha256-sourced rows would be mislabelled
--     blake3, so the sha256 leg would never reap them, and
--   * a sha256-only (mirror-mode) subscription's rows would ALL be labelled
--     blake3 while its pulls only ever write the sha256 bit — so upstream
--     retractions could never remove them, and they would leak forever.
ALTER TABLE mappings ADD COLUMN domains INTEGER NOT NULL DEFAULT 1;

-- Discard every pulled mapping and force a clean, authoritative re-pull.
--
-- Migration 0030 §8 set the precedent for exactly this move, and for the same
-- reason: when rows cannot carry the provenance the new code depends on, the
-- honest fix is to re-derive them from the repo rather than invent it. Local
-- (`scope = 'local'`) mappings are untouched — they are user data, and their
-- DEFAULT 1 mask is already correct.
--
-- Clearing `last_pull_file_marker` is what makes the next pull a full one:
-- `pull_repo` treats a NULL marker as "no incremental state" and requests every
-- bucket with since = 0, so each bucket comes back FULL and authoritative for
-- its hash range. `mapping_cursor` is zeroed alongside it so the request and the
-- stored cursor agree.
--
-- Cost is one full pull per subscription, once — the same cost a dual-domain
-- subscriber was already paying on EVERY pull before this change.
DELETE FROM mappings
 WHERE service_id IN (SELECT id FROM services WHERE url IS NOT NULL);

UPDATE service_domain_pull_state
   SET mapping_cursor = 0,
       last_pull_file_marker = NULL
 WHERE service_id IN (SELECT id FROM services WHERE url IS NOT NULL);
