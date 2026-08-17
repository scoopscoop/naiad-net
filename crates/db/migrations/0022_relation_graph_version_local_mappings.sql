-- Widen the `mappings` triggers that bump `relation_graph_version` (0016) to
-- fire unconditionally, dropping the `WHEN NEW.author IS NOT NULL` /
-- `WHEN OLD.author IS NOT NULL` guards.
--
-- 0016's in-file justification — "auto-scores are derived from AUTHORED
-- mappings only... [so] local and Hydrus-import rows (author IS NULL) never
-- affect the merged graph" — was true when it was written, but ADR 0019
-- (adoption-based positive trust scoring) falsified it: `local_mapping_keys`
-- now scans *local* (`author IS NULL`) mappings to compute each author's
-- adoption ratio, and `merged_sibling_edges` consults `effective_trust_map`
-- (which folds in that adoption ratio) when it resolves sibling tie-breaks.
-- So a purely local tag write can change an author's effective weight, which
-- can change which sibling wins — but under the narrower 0016 guard, only
-- `trust_score_version` (0021, which already fires unconditionally on
-- `mappings` for exactly this reason — see its own header comment) would
-- bump. `relation_graph_version` stayed pinned to the stale pre-write value
-- until some unrelated authored write or relation-table change happened to
-- bump it, so `relation_graph()`'s cache kept serving the old merged graph.
--
-- A future reader landing on 0016's trigger comment needs this pointer: that
-- comment is now describing the *previous* invariant, not the current one.
-- We do not edit 0016 in place (migrations are an ordered, append-only list)
-- — instead we drop and recreate its three `mappings` triggers here, without
-- the guard. 0016's relation-table triggers (`tag_siblings`, `tag_parents`,
-- `services`, `author_trust`, `block_rules`) are untouched: those are not
-- gated on `author` and are not affected by this fix.
DROP TRIGGER mappings_relver_ai;
DROP TRIGGER mappings_relver_au;
DROP TRIGGER mappings_relver_ad;

CREATE TRIGGER mappings_relver_ai AFTER INSERT ON mappings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_relver_au AFTER UPDATE ON mappings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_relver_ad AFTER DELETE ON mappings
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
