-- SHA-256 incremental sync watermark and delta support (#142).
--
-- A pull from the store-backed (mirror-mode) SHA-256 domain has been a FULL
-- pull every time: the delta machinery is keyed to BLAKE3 file positions
-- (owned_bucket_keys_after_file_id, max_file_id, the merge's blake3 bucket
-- clear), all wrong for SHA-256 whose interop hash arrives AFTER the row exists
-- (import-without-hash, then backfill/rescan). This migration adds the
-- SHA-256-correct key source and watermark the delta path needs.
--
-- Four steps, each with a correctness reason:
--
--   1. Lowercase-normalise files.sha256. Bucket range scans are lexical on the
--      hex string (hash >= lo AND hash < hi); an uppercase digit sorts outside
--      the lowercase [0-9a-f] range and would fall out of every bucket. The
--      read path already lowercases defensively (sha256_domain_pull_inputs);
--      normalising the stored value and lowercasing on every write from now on
--      makes that defensive read a backstop rather than the only guard.
--
--   2. files.sha256_seq + a dedicated single-row monotonic counter. sha256_seq
--      is stamped whenever a row GAINS its sha256 (option A). A files.id
--      watermark is UNSOUND: a file imported without a sha256 gets its id at
--      import and contributes no bucket key; a later backfill fills its sha256
--      but leaves files.id unchanged, so a max_file_id marker that had advanced
--      past it would classify its new bucket "already covered" and never pull
--      it. sha256_seq closes this because it is stamped at the moment the sha256
--      becomes known. NOTE: this schema has no generic key/value "meta" table
--      (app_settings is TEXT-typed); a dedicated integer counter table mirrors
--      the established single-row-counter pattern (relation_graph_version,
--      migration 0016). A counter, not MAX(sha256_seq)+1, so monotonicity
--      survives row deletion: deleting the max-seq row must never let the next
--      gain REISSUE that value.
--
--      The bucket-clear range scan in merge_mapping_delta (made domain-aware in
--      this feature) range-scans files.sha256. That scan is served by the FULL
--      idx_files_sha256 index migration 0011 already created
--      (ON files(sha256), no WHERE clause). A partial variant
--      (WHERE sha256 IS NOT NULL) was considered and rejected: it cannot serve
--      the sha256 IS NULL count queries (count_files_missing_sha256) while
--      buying only a few MB on a typical library.
--
--   3. Retroactively stamp existing sha256-bearing rows in a stable ORDER BY id
--      so the assignment is deterministic, and set the counter to the max
--      assigned. These rows genuinely need offering to the repo once (their keys
--      have never been sent under the new semantics); step 4 arranges that.
--
--   4. Clear the SHA-256 rows in service_domain_pull_state. Migration 0033 keyed
--      pull state by (service_id, domain); the sha256 rows store a
--      last_pull_file_marker that is a files.id — the WRONG unit for sha256_seq.
--      There is no sound translation between the two orderings (that is the
--      whole point), so zero the sha256 pull state and let the next pull be one
--      clean authoritative full re-pull that rewrites the marker in the correct
--      unit. Exactly the precedent 0034 set. BLAKE3 rows are untouched — their
--      marker is a files.id and still correct.
--
-- Cost: one full SHA-256 pull per subscribed mirror, once — the same cost these
-- subscriptions pay on EVERY pull today.

-- 1. Lowercase-normalise stored sha256.
UPDATE files SET sha256 = lower(sha256) WHERE sha256 IS NOT NULL;

-- 2. sha256_seq column + a dedicated single-row monotonic counter.
ALTER TABLE files ADD COLUMN sha256_seq INTEGER;

CREATE TABLE sha256_seq_counter (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL
);
INSERT INTO sha256_seq_counter (id, value) VALUES (1, 0);

-- 3. Retroactively stamp existing sha256-bearing rows, dense, ORDER BY id.
UPDATE files
   SET sha256_seq = ordered.rn
  FROM (
      SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn
        FROM files
       WHERE sha256 IS NOT NULL
  ) AS ordered
 WHERE files.id = ordered.id;

UPDATE sha256_seq_counter
   SET value = COALESCE((SELECT MAX(sha256_seq) FROM files), 0)
 WHERE id = 1;

-- 4. Clear the sha256 pull state (its marker is a files.id, the wrong unit now).
UPDATE service_domain_pull_state
   SET mapping_cursor = 0,
       last_pull_file_marker = NULL
 WHERE domain = 'sha256';
