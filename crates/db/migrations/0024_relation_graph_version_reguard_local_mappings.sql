-- Re-guard the `mappings` triggers behind `relation_graph_version`, reversing
-- migration `0022_relation_graph_version_local_mappings`.
--
-- The arc so far, for a reader landing on this file cold:
--   - `0016_relation_graph_version` gated the triggers on
--     `WHEN NEW.author IS NOT NULL` / `WHEN OLD.author IS NOT NULL`, on the
--     premise "auto-scores are derived from AUTHORED mappings only" — true at
--     the time, and it kept bulk local imports trigger-cheap.
--   - ADR 0019 (adoption scoring) falsified that premise: `local_mapping_keys`
--     reads *local* (`author IS NULL`) mappings to compute adoption, and
--     `merged_sibling_edges` folds adoption into the sibling tie-break. So a
--     purely local tag write COULD change which sibling a merged graph
--     resolves to — but under 0016's guard, only `trust_score_version` (0021,
--     unconditional on `mappings` for exactly this reason) would bump.
--     `relation_graph_version` stayed pinned to the pre-write value, so the
--     cached graph in `Db::relation_graph` kept serving stale weights.
--     `0022` fixed this the only way available at the time: drop the guard
--     entirely, so ANY `mappings` write (local or authored) bumps
--     `relation_graph_version` and the whole `RelationCacheStore` — every
--     cached `(services, use_auto)` entry, `false` and `true` alike — clears.
--   - That fix overshot. The `use_auto = false` graph variant NEVER consults
--     adoption (`auto_score_map` zeroes it out before scoring — ADR 0019 §3),
--     so it cannot possibly be affected by a local mapping write. Under
--     `0022`'s unconditional trigger, though, every local tag add still forced
--     a full rebuild of the merged relation graph for BOTH cache slots — on a
--     >1M-tag library the ~600MB characterization from #70 — in the
--     *default* configuration (`trust_uses_auto = false`), for a write that
--     provably cannot change the served result.
--
-- The real fix is per-slot invalidation, not a single store-wide version: see
-- `RelationCacheStore`/`RelationCache` in `crates/db/src/lib.rs`. Each cached
-- entry now stamps the `relation_graph_version` (and, for the `use_auto = true`
-- slot only, the `trust_score_version`) it was built against, and is checked
-- independently on lookup. That makes it safe — and correct — to re-narrow
-- `relation_graph_version`'s own trigger back to authored rows: the
-- `use_auto = true` slot's adoption-dependent staleness is now caught by its
-- own `trust_score_version` stamp instead, and the `use_auto = false` slot is
-- rightly immune to a local write, exactly as before `0022`.
--
-- We do not edit `0016` or `0022` in place (migrations are an ordered,
-- append-only list) — this recreates the same three `mappings` triggers a
-- second time, restoring the original guard.
DROP TRIGGER mappings_relver_ai;
DROP TRIGGER mappings_relver_au;
DROP TRIGGER mappings_relver_ad;

CREATE TRIGGER mappings_relver_ai AFTER INSERT ON mappings
WHEN NEW.author IS NOT NULL
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_relver_au AFTER UPDATE ON mappings
WHEN OLD.author IS NOT NULL OR NEW.author IS NOT NULL
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER mappings_relver_ad AFTER DELETE ON mappings
WHEN OLD.author IS NOT NULL
BEGIN UPDATE relation_graph_version SET version = version + 1 WHERE id = 1; END;
