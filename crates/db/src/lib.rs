//! SQLite-backed storage for the Naiad library.
//!
//! The client store is deliberately SQLite (bundled, so there is no system
//! dependency) — proven for exactly this workload. Migrations run automatically
//! on [`Db::open`], so callers never touch schema setup directly.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use naiad_core::{
    CmpOp, FileContent, FileMetadata, FileRecord, FileState, Hash, Location, MatchMode,
    ParentEdges, Predicate, Query, RelationGraph, SiblingEdges, SysField, SystemPredicate, Tag,
    TagPattern, bucket_key, canonicalize, effective_tags, path_from_bytes, path_to_bytes,
    tag_normalize,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use rusqlite_migration::{HookResult, M, Migrations};

mod error;
pub use error::Error;

/// Convenience result type for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A slice of `(Hash, tags)` pairs as returned by a tag-pull response.
///
/// Each tag entry is `(Tag, Option<subtag-value>)` — the subtag value is
/// `Some` for value-tags (e.g. `"rating:5"`) and `None` for plain tags.
pub type TaggedEntries<'a> = &'a [(Hash, Vec<(Tag, Option<String>)>)];

/// Embedded, ordered schema migrations. Append new `M::up(...)` entries; never
/// edit a released one.
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_files.sql")),
        M::up(include_str!("../migrations/0002_tags.sql")),
        M::up(include_str!("../migrations/0003_roots.sql")),
        M::up(include_str!("../migrations/0004_service_url.sql")),
        M::up(include_str!("../migrations/0005_service_priority.sql")),
        M::up(include_str!("../migrations/0006_service_relation_pull.sql")),
        M::up(include_str!("../migrations/0007_block_rules.sql")),
        M::up(include_str!("../migrations/0008_relation_staging.sql")),
        M::up(include_str!("../migrations/0009_author_trust.sql")),
        M::up(include_str!("../migrations/0010_mappings_author_index.sql")),
        M::up(include_str!("../migrations/0011_hydrus_staging.sql")),
        M::up(include_str!("../migrations/0012_tags_subtag_index.sql")),
        M::up(include_str!("../migrations/0013_location_created_at.sql")),
        M::up(include_str!("../migrations/0014_tag_completion_counts.sql")),
        M::up(include_str!("../migrations/0015_mapping_pull_cursor.sql")),
        M::up(include_str!(
            "../migrations/0016_relation_graph_version.sql"
        )),
        M::up(include_str!("../migrations/0017_locations_path_index.sql")),
        M::up(include_str!("../migrations/0018_tag_namespace_counts.sql")),
        M::up(include_str!(
            "../migrations/0019_tags_subtag_nocase_index.sql"
        )),
        M::up(include_str!("../migrations/0020_tool_provenance.sql")),
        M::up(include_str!("../migrations/0021_trust_score_version.sql")),
        M::up(include_str!(
            "../migrations/0022_relation_graph_version_local_mappings.sql"
        )),
        M::up(include_str!(
            "../migrations/0023_mappings_authored_covering_index.sql"
        )),
        M::up(include_str!(
            "../migrations/0024_relation_graph_version_reguard_local_mappings.sql"
        )),
        M::up(include_str!(
            "../migrations/0025_mappings_authored_covering_index_status.sql"
        )),
        M::up(include_str!("../migrations/0026_mapping_supporters.sql")),
        M::up(include_str!(
            "../migrations/0027_supporter_trust_triggers.sql"
        )),
        M::up(include_str!("../migrations/0028_mapping_rejections.sql")),
        M::up(include_str!("../migrations/0029_contributor_identity.sql")),
        M::up(include_str!("../migrations/0030_pivot_simple.sql")),
        M::up_with_hook(
            include_str!("../migrations/0031_recanonicalize_tags.sql"),
            |tx: &rusqlite::Transaction| -> HookResult { recanonicalize_tags(tx) },
        ),
        M::up(include_str!(
            "../migrations/0032_mappings_covering_index.sql"
        )),
        M::up(include_str!("../migrations/0033_domain_pull_state.sql")),
        M::up(include_str!(
            "../migrations/0034_mapping_domain_provenance.sql"
        )),
        M::up(include_str!("../migrations/0035_sha256_incremental.sql")),
        M::up(include_str!("../migrations/0036_tag_origin.sql")),
        M::up(include_str!(
            "../migrations/0037_service_store_generation.sql"
        )),
    ])
});

/// Re-canonicalize every `tags` row by round-tripping through [`Tag`]'s
/// parse → Display cycle. Rows already in canonical form (the overwhelming
/// majority on any database written by a correct build) are skipped without
/// touching the database. Non-canonical rows are either updated in place (no
/// collision with an existing canonical row) or merged into the canonical row
/// (collision case: mappings, staged_mappings, siblings, and parents are
/// re-pointed and the duplicate tag row is deleted).
///
/// After any row is changed, both count tables are rebuilt deterministically
/// so triggers that fired during the merge do not leave stale counts.
///
/// Called as the hook for migration 0031.
fn recanonicalize_tags(tx: &rusqlite::Transaction) -> HookResult {
    // Collect all tag rows first so we can mutate freely without holding an
    // open statement borrow.
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = tx.prepare("SELECT id, namespace, subtag FROM tags")?;
        let iter = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        iter.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let scanned = rows.len();

    let mut any_changed = false;

    for (old_id, namespace, subtag) in rows {
        // Compute canonical (namespace, subtag) via parse∘Display.
        let stored = Tag {
            namespace: namespace.clone(),
            subtag: subtag.clone(),
        };
        let canon = match Tag::parse(&stored.to_string()) {
            Ok(t) => t,
            // Unparseable stored pair: leave it alone, nothing we can do.
            Err(_) => continue,
        };
        if canon.namespace == namespace && canon.subtag == subtag {
            // Already canonical — the common case for real data.
            continue;
        }

        any_changed = true;

        // Check whether the canonical row already exists.
        let existing_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM tags WHERE namespace = ?1 AND subtag = ?2",
                params![canon.namespace, canon.subtag],
                |r| r.get(0),
            )
            .optional()?;

        match existing_id {
            None => {
                // No collision: rename the row in place.
                tx.execute(
                    "UPDATE tags SET namespace = ?1, subtag = ?2 WHERE id = ?3",
                    params![canon.namespace, canon.subtag, old_id],
                )?;
            }
            Some(new_id) => {
                // Collision: merge old_id into new_id, then delete old_id.

                // mappings: UNIQUE(file_id, tag_id, service_id)
                tx.execute(
                    "UPDATE OR IGNORE mappings SET tag_id = ?1 WHERE tag_id = ?2",
                    params![new_id, old_id],
                )?;
                tx.execute("DELETE FROM mappings WHERE tag_id = ?1", params![old_id])?;

                // staged_mappings: UNIQUE(sha256, tag_id, service_id)
                tx.execute(
                    "UPDATE OR IGNORE staged_mappings SET tag_id = ?1 WHERE tag_id = ?2",
                    params![new_id, old_id],
                )?;
                tx.execute(
                    "DELETE FROM staged_mappings WHERE tag_id = ?1",
                    params![old_id],
                )?;

                // tag_siblings: UNIQUE(bad_tag_id, service_id)
                tx.execute(
                    "UPDATE OR IGNORE tag_siblings SET bad_tag_id = ?1 WHERE bad_tag_id = ?2",
                    params![new_id, old_id],
                )?;
                tx.execute(
                    "UPDATE OR IGNORE tag_siblings SET ideal_tag_id = ?1 \
                     WHERE ideal_tag_id = ?2",
                    params![new_id, old_id],
                )?;
                // Remove any remaining rows still referencing old_id (those
                // that could not be moved due to UNIQUE conflicts).
                tx.execute(
                    "DELETE FROM tag_siblings \
                     WHERE bad_tag_id = ?1 OR ideal_tag_id = ?1",
                    params![old_id],
                )?;

                // tag_parents: UNIQUE(child_tag_id, parent_tag_id, service_id)
                tx.execute(
                    "UPDATE OR IGNORE tag_parents \
                     SET child_tag_id = ?1 WHERE child_tag_id = ?2",
                    params![new_id, old_id],
                )?;
                tx.execute(
                    "UPDATE OR IGNORE tag_parents \
                     SET parent_tag_id = ?1 WHERE parent_tag_id = ?2",
                    params![new_id, old_id],
                )?;
                tx.execute(
                    "DELETE FROM tag_parents \
                     WHERE child_tag_id = ?1 OR parent_tag_id = ?1",
                    params![old_id],
                )?;

                // Finally remove the non-canonical tag row itself.
                tx.execute("DELETE FROM tags WHERE id = ?1", params![old_id])?;
            }
        }
    }

    // Rebuild count tables deterministically when anything changed.
    // (Triggers fired during the merge may have left counts in an
    // intermediate state; a full rebuild is the safe reset.)
    if any_changed {
        tx.execute_batch(
            "DELETE FROM tag_completion_counts;
             INSERT INTO tag_completion_counts (tag_id, current_count)
             SELECT tag_id, COUNT(*)
             FROM mappings
             WHERE status = 'current'
             GROUP BY tag_id;
             DELETE FROM tag_namespace_counts;
             INSERT INTO tag_namespace_counts (namespace, tag_count)
             SELECT t.namespace, COUNT(*)
             FROM tags t
             JOIN tag_completion_counts c ON c.tag_id = t.id
             WHERE t.namespace <> ''
             GROUP BY t.namespace;",
        )?;
    }

    tracing::info!(
        target: "db",
        rows = scanned as u64,
        any_changed,
        "migration 0031: tag recanonicalization complete",
    );
    Ok(())
}

/// A shared, invalidatable cache of the merged relation graph. Wrapping it in an
/// `Arc` lets several read-only connections point at the **same** cache so the
/// ~600MB graph is built once, not once per connection — the dominant cold-start
/// cost when the library holds >1M tags (#70). Connections opened without a
/// shared cache each get a fresh, private one.
pub type SharedRelationCache = Arc<std::sync::Mutex<RelationCacheStore>>;

/// Maximum number of distinct service-set graphs kept in one
/// [`RelationCacheStore`] before the oldest is evicted. Keeps a `Merged` and a
/// `LocalOnly` graph (plus a couple of ad-hoc scopes) resident at once without
/// growing unbounded.
const RELATION_CACHE_CAP: usize = 4;

/// Thread-safe handle for interrupting the SQL statement currently running on
/// a [`Db`] connection. The handle does not borrow the connection, so an async
/// caller can keep it outside the mutex that guards `Db` and signal a blocking
/// query when the request that owns it is dropped.
///
/// The handle is connection-scoped, not request-scoped: callers sharing a
/// connection must coordinate ownership so a late interrupt cannot land on a
/// later request. The daemon's tag lane provides that coordination.
pub struct DbInterruptHandle {
    inner: rusqlite::InterruptHandle,
}

impl DbInterruptHandle {
    /// Interrupt the statement currently executing on the associated
    /// connection. Calling this while no statement is running is a no-op.
    ///
    /// SQLite reports the interrupted statement as `SQLITE_INTERRUPT`; this
    /// method does not wait for that statement to observe the interrupt.
    pub fn interrupt(&self) {
        self.inner.interrupt();
    }
}

struct ProgressHandlerGuard<'a>(&'a Connection);

impl Drop for ProgressHandlerGuard<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

/// `PRAGMA journal_size_limit` for the writer connection: 64 MB (#232).
///
/// A completed checkpoint truncates `naiad.db-wal` back under this limit, so
/// the file stops sitting at its historical high-water mark. The daemon's WAL
/// backstop uses the same value as its "worth checkpointing" threshold.
pub const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// Result of one `PRAGMA wal_checkpoint(TRUNCATE)` on the writer (#232;
/// mirrors the sidecar's `WalCheckpoint` from #231).
#[derive(Debug, Clone, Copy)]
pub struct WalCheckpoint {
    /// True when a reader's snapshot blocked the checkpoint from completing —
    /// the WAL was not (fully) truncated. Persistent `busy` across passes is
    /// the visible symptom of checkpoint starvation.
    pub busy: bool,
    /// Total frames in the WAL at checkpoint time.
    pub log_frames: i64,
    /// Frames successfully copied back into the main database file.
    pub checkpointed_frames: i64,
}

/// A handle to the Naiad library database.
pub struct Db {
    conn: Connection,
    /// Cached relation graphs keyed by (relation_graph_version, services).
    /// Interior mutability keeps read methods `&self`; `Db` stays `Send` for
    /// the daemon's `Arc<Mutex<Db>>`. Shared across the read pool + tag lane via
    /// [`Db::open_readonly_with_cache`] so each scope's graph is built once (#70).
    relation_cache: SharedRelationCache,
}

/// Holds several built relation graphs so distinct service scopes (e.g.
/// `Merged` and `LocalOnly`) coexist instead of evicting each other from a
/// single slot.
///
/// Each [`RelationCache`] entry stamps the `relation_graph_version` it was
/// built against; [`Db::relation_graph`] checks a looked-up entry's stamp
/// rather than clearing the whole store.
#[derive(Default)]
pub struct RelationCacheStore {
    entries: Vec<RelationCache>,
}

/// One built relation graph plus the service-set it was built for and the
/// relation version stamp it is valid against.
struct RelationCache {
    services: Vec<i64>,
    relation_version: i64,
    graph: Arc<RelationGraph>,
    completion: Arc<RelationCompletion>,
}

/// An in-process cache mapping [`Tag`] values to their interned row id in the
/// `tags` table.
///
/// Allocate one per import and pass `&mut cache` into each batch write method.
/// Tags are stable once written — the same `(namespace, subtag)` pair always
/// maps to the same `id` row — so a cache that outlives individual transaction
/// batches is correct for the lifetime of one import. Batch methods merge
/// newly interned ids into the cache only after their transaction commits, so
/// a cache that survives a failed (rolled-back) batch never holds ids of rows
/// that no longer exist. Drop it when the import finishes to release memory.
///
/// The inner map key is `(namespace, subtag)` rather than [`Tag`] directly
/// because [`Tag`] does not implement [`std::hash::Hash`].
#[derive(Default)]
pub struct TagCache(HashMap<(String, String), i64>);

impl TagCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }
}

/// A network-backed tag service the client subscribes to: a `scope = 'shared'`
/// row bound to the repository `url` it pulls from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedService {
    pub id: i64,
    pub name: String,
    pub url: String,
}

/// Which relation table an edge came from. The read-side counterpart to
/// netproto's `RelKind` (db does not depend on netproto).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// A sibling/alias edge: `from` (bad) aliases to `to` (ideal).
    Sibling,
    /// A parent/implication edge: `from` (child) implies `to` (parent).
    Parent,
}

impl EdgeKind {
    /// The lowercase wire/display name (`"sibling"` / `"parent"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Sibling => "sibling",
            EdgeKind::Parent => "parent",
        }
    }
}

/// One stored relation edge with provenance, for the `relation list` read path.
/// `author` is the submitter's public-key hex for pulled edges, `None` for
/// locally-created ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationEdgeRow {
    pub kind: EdgeKind,
    pub from: Tag,
    pub to: Tag,
    pub service: String,
    pub author: Option<String>,
}

/// Per-service relation summary for the `relation status` read path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRelationStatus {
    pub service: String,
    pub siblings: u64,
    pub parents: u64,
    /// Unix-seconds of the last relation pull, or `None` if never pulled.
    pub last_pull: Option<i64>,
}

/// Outcome of merging a pulled snapshot into a shared service.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MergeStats {
    /// Owned files that appeared in the snapshot (tags were applied to them).
    pub matched_files: u64,
    /// Mapping rows present in the service after the (authoritative) merge.
    pub mappings: u64,
}

/// Outcome of merging a pulled relation graph into a shared service.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RelationMergeStats {
    /// Sibling edges present in the service after the (authoritative) merge.
    pub siblings: u64,
    /// Parent edges present in the service after the merge.
    pub parents: u64,
}

/// One incremental relation edge to merge into a service's staging mirror. The
/// db-native counterpart to netproto's `DeltaEdge` (db does not depend on
/// netproto); the daemon converts wire edges into these, skipping unparseable
/// tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaEdgeInput {
    pub kind: EdgeKind,
    pub from: Tag,
    pub to: Tag,
    pub author: String,
    /// True for a tombstone (`status = 'deleted'` on the wire).
    pub deleted: bool,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MappingDeltaStatus {
    Current,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingDeltaInput {
    pub hash: Hash,
    pub tag: Tag,
    pub status: MappingDeltaStatus,
    pub seq: u64,
    /// Asserted generation origin (ADR 0026); `None` = manual. Set on INSERT of a
    /// `Current` row only — never in a conflict update (perf rule).
    pub origin: Option<String>,
}

/// `mappings.domains` bit for the native BLAKE3 domain (migration 0034).
///
/// Also the value carried by local, non-pulled rows: they have no hash domain,
/// and the mask only ever needs to be non-zero for them so that the
/// mask-reaches-zero delete in the pull path can never reach a local row.
pub const DOMAIN_BIT_BLAKE3: i64 = 1;
/// `mappings.domains` bit for the SHA-256 interop domain (migration 0034).
pub const DOMAIN_BIT_SHA256: i64 = 2;

/// The `mappings.domains` bit for a wire domain spelling.
///
/// Unknown spellings map to [`DOMAIN_BIT_BLAKE3`]. A repo can only reach this
/// with a domain the client does not implement, in which case treating its rows
/// as native is the conservative choice: they merge into the domain whose merge
/// path is authoritative for everything the client owns, so they are still
/// reaped by a later pull rather than stranded under a bit nothing clears.
#[must_use]
pub const fn domain_bit(domain: &str) -> i64 {
    // `match` on &str is not const; compare bytes instead.
    if matches!(domain.as_bytes(), b"sha256") {
        DOMAIN_BIT_SHA256
    } else {
        DOMAIN_BIT_BLAKE3
    }
}

/// Which services a display/search read draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    /// All services, highest priority first (the default).
    Merged,
    /// The local service only (the `--local-only` toggle).
    LocalOnly,
}

/// Whether a search applies tag-relation inference (siblings/parents) or matches
/// literally. `Raw` is the power-user `--raw` mode: it skips relation expansion
/// entirely (and the edge reads that feed it), matching only stored tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expansion {
    /// Apply sibling (alias) and parent (implication) relations (the default).
    Expanded,
    /// Match stored tags literally — no relation inference.
    Raw,
}

/// Token match strategy for tag completion. v1 always uses `Prefix`; `Substring`
/// is wired for the #32 settings toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionMode {
    Prefix,
    Substring,
}

/// A tag completion candidate: the tag and how many current mappings use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSuggestion {
    pub namespace: String,
    pub subtag: String,
    pub count: i64,
    /// Pre-formatted full tag form of the alias that surfaced this canonical
    /// (e.g. `"character:badtag"`, bare `"badtag"` if unnamespaced);
    /// display-only; set ONLY on step-4 injected rows, never on direct
    /// (step 1) or ideal-name (step 3) matches.
    pub alias_source: Option<String>,
}

/// Bounded relation-completion overlay, cached alongside the relation graph
/// (keyed by `relation_graph_version` + `services`). Sized by the sibling-edge
/// count, not the full `tags` table: only relation-involved ids appear. Rebuilt
/// only when the relation version bumps (every write bumps it in this schema),
/// so its merged counts stay fresh without a per-keystroke recount.
#[derive(Debug, Default)]
pub struct RelationCompletion {
    /// Every bad (aliased) tag id -> its canonical terminal id.
    alias_to_canonical: HashMap<i64, i64>,
    /// Canonical id -> merged effective count (raw(canon) + Σ raw(aliases)).
    /// Present only for canonical ids with >= 1 alias and a positive merged sum.
    merged: HashMap<i64, i64>,
    /// (canonical id, alias tag) for every bad alias — for in-memory fragment
    /// matching of alias spellings.
    alias_names: Vec<(i64, Tag)>,
    /// (canonical id, ideal tag) for every positive-merged canonical ideal —
    /// for in-memory fragment matching / injection of ideal spellings.
    ideal_names: Vec<(i64, Tag)>,
}

impl RelationCompletion {
    /// True when there are no sibling relations at all (the plain indexed scan
    /// is then already exactly right — no merge work needed).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.alias_to_canonical.is_empty()
    }

    /// The canonical id a bad alias id collapses to, if `tag_id` is an alias.
    #[must_use]
    pub fn canonical_of(&self, tag_id: i64) -> Option<i64> {
        self.alias_to_canonical.get(&tag_id).copied()
    }

    /// The merged effective count for a canonical id, if it has aliases and a
    /// positive merged sum.
    #[must_use]
    pub fn merged_count(&self, canonical_id: i64) -> Option<i64> {
        self.merged.get(&canonical_id).copied()
    }

    /// (canonical id, alias tag) pairs for fragment-matching alias spellings.
    pub fn alias_names_iter(&self) -> impl Iterator<Item = (i64, &Tag)> {
        self.alias_names.iter().map(|(c, t)| (*c, t))
    }

    /// (canonical id, ideal tag) pairs for fragment-matching ideal spellings.
    pub fn ideal_names_iter(&self) -> impl Iterator<Item = (i64, &Tag)> {
        self.ideal_names.iter().map(|(c, t)| (*c, t))
    }

    /// The ideal tag name for a positive-merged canonical id, if known.
    #[must_use]
    pub fn ideal_name(&self, canonical_id: i64) -> Option<&Tag> {
        self.ideal_names
            .iter()
            .find(|(c, _)| *c == canonical_id)
            .map(|(_, t)| t)
    }
}

/// A namespace completion candidate: the namespace and how many distinct tags it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceSuggestion {
    pub namespace: String,
    pub tag_count: i64,
}

/// What a [`BlockRule`] targets. `Tag` and `TagPattern` match the tag text;
/// `Author` matches the submitter's public-key hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// An exact `namespace:subtag`.
    Tag,
    /// A glob over tag text (the query parser's `TagPattern` grammar).
    TagPattern,
    /// A 64-char Ed25519 public-key hex.
    Author,
}

impl BlockKind {
    /// The lowercase wire/storage name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Tag => "tag",
            BlockKind::TagPattern => "tag_pattern",
            BlockKind::Author => "author",
        }
    }

    /// Parse a stored/wire kind string.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an unrecognized kind.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "tag" => Ok(BlockKind::Tag),
            "tag_pattern" => Ok(BlockKind::TagPattern),
            "author" => Ok(BlockKind::Author),
            other => Err(Error::Invalid(format!("unknown block kind {other:?}"))),
        }
    }
}

/// One stored block rule. `id` is shown by `block list` and used by
/// `block remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRule {
    pub id: i64,
    pub kind: BlockKind,
    pub target: String,
    pub note: Option<String>,
    pub created_at: i64,
}

/// One stored mapping rejection (ADR 0006 fourth suppression kind).
/// Returned by [`Db::list_rejections`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub service_id: i64,
    /// Display name of the shared service.
    pub service: String,
    pub file_id: i64,
    pub tag_id: i64,
    /// Canonical text of the rejected tag.
    pub tag: String,
    pub note: Option<String>,
    pub created_at: i64,
    /// Blake3 hex hash of the rejected file (`files.blake3`).
    pub hash: String,
}

/// Contributor identity for one shared service.
/// `repo_anchor` is `None` until first resolved.
/// Returned by [`Db::contributor_identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributorIdentity {
    pub repo_anchor: Option<String>,
}

/// Select the relation graph a single predicate evaluates against: the merged
/// graph for `Expanded`, or an empty graph for `Exact` (the per-term `=`
/// operator), which makes `match_set`/wildcard matching degenerate to a literal
/// match.
fn pick<'a>(
    mode: MatchMode,
    merged: &'a RelationGraph,
    empty: &'a RelationGraph,
) -> &'a RelationGraph {
    match mode {
        MatchMode::Expanded => merged,
        MatchMode::Exact => empty,
    }
}

/// A per-read view of the block list: precomputed suppressed tag ids (exact +
/// pattern, resolved against the current tag dictionary), plus the set of all
/// local service ids. Rebuilt on each read so rule edits take effect
/// immediately. The `!local_service_ids.contains(service_id)` check is the
/// local-exempt rule (ADR 0006): a user's own tags are never suppressed,
/// regardless of which local service they were mapped on.
struct BlockMatcher {
    suppressed_tag_ids: HashSet<i64>,
    local_service_ids: HashSet<i64>,
}

impl BlockMatcher {
    /// Whether a mapping row `(tag_id, service_id)` should be hidden.
    fn is_suppressed(&self, tag_id: i64, service_id: i64) -> bool {
        !self.local_service_ids.contains(&service_id) && self.suppressed_tag_ids.contains(&tag_id)
    }
}

/// Per-read set of rejected mappings. Rebuilt each read so a reject/undo takes
/// effect immediately (same discipline as [`BlockMatcher`]). Local mappings are
/// never rejected — a user's own tag is deleted directly, not rejected (ADR 0006
/// local-exempt rule); rejections are keyed by a *shared* service anyway, so a
/// local-service row can never appear here.
///
/// Wired into [`Db::display_tags_of`], [`Db::display_tags_detailed`], and
/// effective-expansion [`Db::search`] (ADR 0020 §6). Raw paths never build this.
pub(crate) struct RejectMatcher {
    rejected: HashSet<(i64, i64, i64)>, // (service_id, file_id, tag_id)
}

impl RejectMatcher {
    /// True when the `(service_id, file_id, tag_id)` triple was explicitly
    /// rejected by the user (ADR 0020 §6).
    pub(crate) fn is_rejected(&self, service_id: i64, file_id: i64, tag_id: i64) -> bool {
        self.rejected.contains(&(service_id, file_id, tag_id))
    }
}

/// Single shared display predicate (spec §5).
///
/// Returns `true` when the mapping `(service_id, file_id, tag_id)` should be
/// shown on any display surface:
///
/// - Rejected mappings (present in `mapping_rejections`) are hidden.
/// - Suppressed tags (matched by a block rule for a non-local service) are hidden.
/// - All other mappings — including local-service mappings, which are exempt from
///   both block rules and rejections — are visible.
///
/// `reject` is `None` on raw-search paths (spec §7): rejection filtering is
/// intentionally bypassed so the caller sees the raw database contents.
#[inline]
fn tag_visible(
    blocks: &BlockMatcher,
    reject: Option<&RejectMatcher>,
    service_id: i64,
    file_id: i64,
    tag_id: i64,
) -> bool {
    reject.is_none_or(|rj| !rj.is_rejected(service_id, file_id, tag_id))
        && !blocks.is_suppressed(tag_id, service_id)
}

/// Which of *this client's* services supply an effective display tag. A
/// read-time attribution ("presence"), never stored on a mapping: a tag that
/// exists only via a relation is attributed to the service(s) of the raw tag(s)
/// that produced it. This is unrelated to a tag's generation `origin` (the tool
/// that made it — see ADR 0026); it records *whose* services carry it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagPresence {
    /// Mapped only on the local service.
    Local,
    /// Mapped only on one or more pulled (shared) services.
    Pulled,
    /// Mapped on the local service and at least one pulled service.
    Both,
}

/// An effective tag with its presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagWithPresence {
    pub tag: Tag,
    pub presence: TagPresence,
}

/// An effective (display) tag with presence and the shared (pulled) services
/// that supply it for this file. `services` lists the display names of every
/// shared service whose current mapping contributes the tag; it is empty when
/// `presence == Local`. Used by the ghost-reject flow to call `rejectTag` once
/// per service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagDetail {
    pub tag: Tag,
    pub presence: TagPresence,
    /// Shared service display names carrying this tag (deduplicated, sorted).
    pub services: Vec<String>,
    /// True iff this tag (after canonicalization) has any alias, parent, or
    /// child. Drives the detail-chip relations glyph; set from the
    /// already-loaded graph with no extra query.
    pub relations: bool,
    /// Generation source (ADR 0026): the tool that produced this tag, from the
    /// pulled mapping's `origin_id → origins.name`. `None` = manual/local. Purely
    /// display metadata — asserted, not proven; never read by trust logic.
    pub origin: Option<String>,
}

/// One related tag with its count. Count semantics depend on the section (see
/// [`TagRelations`]): alias rows carry their own raw mapping count, parent/child
/// rows carry the merged display count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationTag {
    pub tag: Tag,
    /// In the **aliases** section: this spelling's own raw `tag_completion_counts`
    /// value (0 when the spelling is never used directly — the common case; the
    /// UI hides a 0). In the **parents/children** sections: the merged display
    /// count (raw of the canonical plus the sum of raw counts for all its
    /// aliases), falling back to the canonical's raw value, or 0 if unmapped.
    pub count: i64,
}

/// One capped section of the relations popover (aliases / parents / children).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelationSection {
    /// At most `cap` rows, ranked count desc then name.
    pub items: Vec<RelationTag>,
    /// Uncapped (pre-cap) count; the client's "… N more" is `total - items.len()`.
    pub total: usize,
}

/// The relations of one tag for the detail popover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRelations {
    /// The canonical tag the query resolves to.
    pub canonical: Tag,
    /// Merged display count for the canonical tag: raw(canonical) plus the sum
    /// of raw counts for all its aliases. Consistent with what tag completions
    /// show; 0 if unmapped.
    pub count: i64,
    /// True iff the queried file carries the tag via an alias raw mapping.
    pub via_alias: bool,
    /// Alias tags (sibling preimage of the canonical): other names for the same
    /// concept. Each row carries its own raw mapping count (usually 0, hidden in
    /// the UI); `total` tells how many alternate spellings exist. Ranked count
    /// desc then name.
    pub aliases: RelationSection,
    /// Parent tags (implications fired by this tag), canonicalized and ranked by
    /// library-wide merged display count.
    pub parents: RelationSection,
    /// Child tags (tags that imply this one), canonicalized and ranked by
    /// library-wide merged display count.
    pub children: RelationSection,
}

impl Db {
    /// Return a thread-safe handle that can interrupt this connection's active
    /// SQLite statement without first acquiring the [`Db`] mutex.
    ///
    /// See [`DbInterruptHandle`] for the connection-ownership requirement.
    #[must_use]
    pub fn interrupt_handle(&self) -> DbInterruptHandle {
        DbInterruptHandle {
            inner: self.conn.get_interrupt_handle(),
        }
    }

    /// Run an operation with a connection-scoped cancellation predicate.
    ///
    /// SQLite polls `cancelled` every 1,000 virtual-machine instructions for
    /// every statement executed by `f`. Returning `true` aborts the statement
    /// with `SQLITE_INTERRUPT`. The progress handler is removed when `f`
    /// returns or unwinds, so later operations do not inherit the predicate.
    /// Call this only while the caller exclusively owns the connection.
    pub fn with_query_cancellation<T, C, F>(&self, cancelled: C, f: F) -> T
    where
        C: FnMut() -> bool + Send + 'static,
        F: FnOnce(&Self) -> T,
    {
        self.conn.progress_handler(1_000, Some(cancelled));
        let _guard = ProgressHandlerGuard(&self.conn);
        f(self)
    }

    /// Open (creating if absent) the database at `path` and apply migrations.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be opened or migrations fail.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let newly_created = !path.exists();
        let conn = Connection::open(path)?;
        let db = Self::init(conn)?;
        tracing::info!(
            target: "db",
            path = %path.display(),
            newly_created,
            "database opened",
        );
        Ok(db)
    }

    /// Open an in-memory database (for tests) and apply migrations.
    ///
    /// # Errors
    /// Returns an error if migrations fail.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    /// Expose the raw `rusqlite::Connection` for test assertions (schema
    /// introspection via `pragma_table_info`, `sqlite_master` queries, etc.).
    /// Not available in production builds.
    #[cfg(test)]
    pub(crate) fn raw_conn_for_test(&self) -> &Connection {
        &self.conn
    }

    /// Open an existing database file **read-only**, for the daemon's read path.
    ///
    /// Because the library DB runs in WAL mode, a separate read-only connection
    /// can serve queries (`/file`, `/search`, `/tags`, …) concurrently while the
    /// writer connection holds a long write — e.g. a bulk Hydrus import — so the
    /// UI never freezes behind it. This never migrates or writes: the writer
    /// [`Db::open`] is the single source of schema truth and must run first.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened read-only.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_readonly_with_cache(path, Db::new_relation_cache())
    }

    /// Like [`Db::open_readonly`], but the connection shares `cache` (the merged
    /// relation graph) with every other connection given the same `Arc`. The
    /// first cold reader builds the ~600MB graph while holding the cache lock;
    /// the rest block briefly then reuse the built `Arc` — so the daemon's read
    /// pool + tag lane pay the cold build once, not once per connection (#70).
    ///
    /// Create the shared cache with [`Db::new_relation_cache`].
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened read-only.
    pub fn open_readonly_with_cache(
        path: impl AsRef<Path>,
        cache: SharedRelationCache,
    ) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
        let path_ref = path.as_ref();
        let conn = Connection::open_with_flags(path_ref, flags)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // synchronous is a no-op on read-only connections but set it
        // consistently so if the connection is ever promoted it inherits the
        // same policy; cache_size and mmap_size apply equally to readers.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -32768i64)?;
        conn.pragma_update(None, "mmap_size", 268_435_456i64)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        tracing::debug!(target: "db", path = %path_ref.display(), "read-only pool connection opened");
        Ok(Self {
            conn,
            relation_cache: cache,
        })
    }

    /// Allocate a fresh, empty [`SharedRelationCache`] to hand to a group of
    /// [`Db::open_readonly_with_cache`] connections that should share one graph.
    #[must_use]
    pub fn new_relation_cache() -> SharedRelationCache {
        Arc::new(std::sync::Mutex::new(RelationCacheStore::default()))
    }

    /// Run `PRAGMA wal_checkpoint(TRUNCATE)` on this connection (#232).
    ///
    /// Copies all WAL frames into the main database and truncates the WAL file
    /// (to `journal_size_limit`) when no reader holds a snapshot past them.
    /// Waits for in-flight readers up to the connection's `busy_timeout`; a
    /// reader that outlasts it yields `busy = true` (frames copied so far
    /// remain copied). The daemon's periodic backstop calls this on the writer
    /// when `naiad.db-wal` outgrows [`WAL_SIZE_LIMIT`].
    ///
    /// # Errors
    /// Returns an error if the PRAGMA itself fails (e.g. called on a read-only
    /// connection).
    pub fn checkpoint_wal(&self) -> Result<WalCheckpoint> {
        // Returns one row: (busy, log, checkpointed).
        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
        Ok(WalCheckpoint {
            busy: busy != 0,
            log_frames,
            checkpointed_frames,
        })
    }

    fn init(mut conn: Connection) -> Result<Self> {
        // Sensible pragmas for a local single-writer app.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // synchronous=NORMAL: WAL crash recovery makes FULL redundant. A
        // power-loss can lose the last committed transaction if the WAL has
        // not yet been checkpointed, but WAL-mode recovery replays the WAL on
        // next open so the database is never left in an inconsistent state.
        // For a local library DB this is the right trade-off: the removed
        // per-commit fsync dominates import and tag-edit latency (same
        // reasoning applied in thumb_store.rs for the regenerable thumb cache).
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // cache_size: negative values are KiB. 32 MiB keeps the tag
        // dictionary, hot mapping pages, and recent search result sets in
        // memory across many queries without over-committing RAM on modest
        // hardware. A 100k-file library with a full tag set fits comfortably.
        conn.pragma_update(None, "cache_size", -32768i64)?;
        // mmap_size: map 256 MiB of the file into the process address space.
        // On reads SQLite normally copies pages from the OS page cache;
        // with mmap it references them directly, removing one memcpy per read
        // on 64-bit hosts (which all targets for this crate are).
        conn.pragma_update(None, "mmap_size", 268_435_456i64)?;
        // journal_size_limit: without it a checkpoint reuses the WAL file but
        // never shrinks it, so `naiad.db-wal` permanently sits at the largest
        // write burst it ever saw (#232). With the limit, a completed
        // checkpoint truncates the file back under 64 MB.
        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
        // Wait rather than fail if a reader holds the file during a WAL
        // checkpoint (or vice versa) — both connections share the same file.
        conn.busy_timeout(Duration::from_secs(10))?;
        let from = MIGRATIONS.current_version(&conn)?;
        let started = Instant::now();
        if let Err(e) = MIGRATIONS.to_latest(&mut conn) {
            tracing::error!(target: "db", error = %e, "schema migration failed");
            return Err(e.into());
        }
        let to = MIGRATIONS.current_version(&conn)?;
        tracing::info!(
            target: "db",
            from = %from,
            to = %to,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "schema migrated",
        );
        Ok(Self {
            conn,
            relation_cache: Db::new_relation_cache(),
        })
    }

    /// Upsert a scanned file: ensure its `files` row (by blake3) and its
    /// `file_locations` row (by path) exist, marking the location present and
    /// stamping its `last_seen` with `scan_marker`.
    ///
    /// `scan_marker` is a per-scan value from [`Db::next_scan_marker`]; every
    /// location touched in one scan shares the exact same `last_seen ==
    /// scan_marker`, so the post-scan reconcile can cleanly separate "touched
    /// this scan" from "stale" — even for many scans within one wall-clock
    /// second.
    ///
    /// Idempotent: re-importing the same bytes does not duplicate content;
    /// re-importing at a new path ADDS a location rather than overwriting.
    /// Existing content metadata and state are left untouched.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn insert_file(&self, rec: &FileRecord, scan_marker: i64) -> Result<()> {
        tracing::trace!(target: "db", hash = %rec.hash.to_hex(), size = rec.size, "insert_file");
        // 1. Ensure the content row exists; fetch its id either way.
        //    `imported_at` is wall-clock first-import time, only set on initial
        //    insert (DO NOTHING leaves it untouched on conflict).
        // Lowercase the interop hash on the way in (see migration 0035 §1). A
        // pre-lowered value is idempotent under lower().
        let sha_lc = rec.sha256.as_deref().map(str::to_lowercase);
        self.conn.execute(
            "INSERT INTO files (blake3, size, sha256, state, imported_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(blake3) DO UPDATE SET
                 sha256 = COALESCE(excluded.sha256, files.sha256)",
            params![
                rec.hash.to_hex(),
                rec.size,
                sha_lc,
                FileState::Active.as_str(),
                unix_now()
            ],
        )?;
        let file_id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE blake3 = ?1",
            params![rec.hash.to_hex()],
            |row| row.get(0),
        )?;
        // Stamp sha256_seq on the NULL→present transition only. The guard
        // (sha256 present AND sha256_seq still NULL) makes this idempotent: a
        // re-import that merely re-confirms an existing sha256 leaves the row's
        // seq — and the counter — untouched, so rescans never re-offer synced
        // buckets. import-with-hash mints its key here and must be stamped, or
        // it would carry NULL seq forever and be skipped by every delta.
        //
        // Watch-path note: `reindex_upsert` calls this outside an explicit
        // transaction. That is safe — the guard `sha256_seq IS NULL` means a
        // crash between the INSERT and this stamp self-heals on the next write
        // (the row re-enters with sha256 present but seq still NULL and gets
        // stamped then). The daemon's single writable connection serializes all
        // writers, so the read-modify-write on `sha256_seq_counter` is
        // race-free.
        let needs_stamp: bool = self.conn.query_row(
            "SELECT sha256 IS NOT NULL AND sha256_seq IS NULL FROM files WHERE id = ?1",
            params![file_id],
            |r| r.get(0),
        )?;
        if needs_stamp {
            let seq = Self::reserve_sha256_seq(&self.conn, 1)?;
            self.conn.execute(
                "UPDATE files SET sha256_seq = ?1 WHERE id = ?2",
                params![seq, file_id],
            )?;
        }
        // 2. Upsert the location by (file_id, path); stamp last_seen = marker.
        self.conn.execute(
            "INSERT INTO file_locations (file_id, path, mtime, created_at, present, last_seen)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT(file_id, path) DO UPDATE SET
                 mtime = excluded.mtime,
                 created_at = excluded.created_at,
                 present = 1,
                 last_seen = excluded.last_seen",
            params![
                file_id,
                path_to_bytes(&rec.path),
                rec.mtime,
                rec.created_at,
                scan_marker
            ],
        )?;
        Ok(())
    }

    /// Back up the database to `dest` using `VACUUM INTO`.
    ///
    /// `VACUUM INTO` reads under a single transaction snapshot and writes a
    /// consistent, self-contained, compacted copy at `dest` — safe to run while
    /// readers continue on the WAL-mode file. Runs **outside** any transaction:
    /// SQLite forbids `VACUUM` inside one, so this must **not** be called via
    /// [`with_tx`].
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] if `dest` already exists or its path is not
    /// valid UTF-8. Returns [`Error::Sqlite`] on any SQLite-level failure.
    pub fn vacuum_into(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            tracing::error!(target: "db", dest = %dest.display(), "vacuum refused: destination exists");
            return Err(Error::Invalid(format!(
                "backup destination already exists: {}",
                dest.display()
            )));
        }
        let dest_str = dest.to_str().ok_or_else(|| {
            tracing::error!(target: "db", dest = %dest.display(), "vacuum refused: destination path not UTF-8");
            Error::Invalid(format!(
                "backup path is not valid UTF-8: {}",
                dest.display()
            ))
        })?;
        let started = Instant::now();
        tracing::info!(target: "db", dest = %dest.display(), "vacuum starting");
        self.conn.execute("VACUUM INTO ?1", [dest_str])?;
        tracing::info!(
            target: "db",
            dest = %dest.display(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "vacuum complete",
        );
        Ok(())
    }

    /// Run `f` inside a single transaction: begin, call, commit on `Ok`, roll
    /// back on `Err`.
    ///
    /// Autocommit mode pays one WAL commit (an fsync) per statement, which
    /// dominates bulk writes like scan batches; wrapping the batch makes it a
    /// single commit. Nested calls are not supported (SQLite has one
    /// transaction per connection).
    ///
    /// # Errors
    /// Propagates any error from `f` (after rolling back) or from the
    /// begin/commit statements themselves.
    pub fn with_tx<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        let out = f(self)?;
        tx.commit()?;
        Ok(out)
    }

    /// A scan marker strictly greater than every existing location's
    /// `last_seen`, but never below the current wall clock (so it still sorts
    /// ~chronologically for display). Used to stamp locations touched during a
    /// scan and to reconcile afterward.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn next_scan_marker(&self) -> Result<i64> {
        let max_seen: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(last_seen), 0) FROM file_locations",
            [],
            |row| row.get(0),
        )?;
        Ok(unix_now().max(max_seen + 1))
    }

    /// All files joined to one representative location, ordered by path.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn list_files(&self) -> Result<Vec<FileListing>> {
        // Only files with at least one present location are listed; a file whose
        // every location is missing (deleted from disk, or hidden when its root
        // was unwatched) drops out, and the representative path is a present one.
        let mut stmt = self.conn.prepare(
            "SELECT f.blake3, f.size, l.path, f.imported_at, l.created_at, l.mtime, f.mime
             FROM files f
             JOIN file_locations l ON l.file_id = f.id AND l.present = 1
             GROUP BY f.id
             ORDER BY l.path",
        )?;
        let rows = stmt
            .query_map([], row_to_listing)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Look up a file's content row by hash, if present.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn get_by_hash(&self, hash: &Hash) -> Result<Option<FileContent>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, blake3, size, mime, width, height, duration_ms, state, imported_at
                 FROM files WHERE blake3 = ?1",
                params![hash.to_hex()],
                row_to_content,
            )
            .optional()?;
        Ok(row)
    }

    /// All known locations for the content with `hash`, in insertion order.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn locations_of(&self, hash: &Hash) -> Result<Vec<Location>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.path, l.mtime, l.created_at, l.present, l.last_seen
             FROM file_locations l
             JOIN files f ON f.id = l.file_id
             WHERE f.blake3 = ?1
             ORDER BY l.id",
        )?;
        let rows = stmt
            .query_map(params![hash.to_hex()], row_to_location)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Set the extracted metadata (`mime`/`width`/`height`) on the content row
    /// with `hash`. Called by the post-hash extraction pass; a no-op if no row
    /// matches. State and other columns are left untouched.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn update_metadata(&self, hash: &Hash, meta: &FileMetadata) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET mime = ?1, width = ?2, height = ?3 WHERE blake3 = ?4",
            params![meta.mime, meta.width, meta.height, hash.to_hex()],
        )?;
        Ok(())
    }

    /// Reconcile after scanning `root`: mark every location *under `root`* not
    /// touched since `scan_marker` (a value from [`Db::next_scan_marker`]) as
    /// missing (`present = 0`). Never deletes. Returns the number of locations
    /// newly marked missing.
    ///
    /// The reconcile is scoped to `root`'s subtree so scanning one watched
    /// folder never disturbs another: a file under a *different* root keeps its
    /// own (older) `last_seen` and stays present.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn mark_missing_under_before(&self, root: &Path, scan_marker: i64) -> Result<u64> {
        // `?1` is the marker; the subtree predicate binds from `?2`.
        let (subtree, blobs) = subtree_predicate(root, 2);
        let sql = format!(
            "UPDATE file_locations SET present = 0
             WHERE present = 1 AND last_seen < ?1 AND ({subtree})"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(blobs.len() + 1);
        params.push(&scan_marker);
        for blob in &blobs {
            params.push(blob);
        }
        let changed = self.conn.execute(&sql, params.as_slice())?;
        Ok(changed as u64)
    }

    /// Mark the location at `path` — and every location beneath it — missing
    /// (`present = 0`). Returns the number of locations newly flipped. Used by the
    /// live watcher for a removed file (exact match) or a removed directory
    /// (descendant match). Never deletes rows.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn mark_missing_path(&self, path: &Path) -> Result<u64> {
        let (subtree, blobs) = subtree_predicate(path, 1);
        let sql =
            format!("UPDATE file_locations SET present = 0 WHERE present = 1 AND ({subtree})");
        let changed = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(blobs.iter()))?;
        Ok(changed as u64)
    }

    /// A snapshot of every *present* location's `(size, mtime)`, keyed by path.
    ///
    /// A scan reads this once up front, then for each file on disk compares its
    /// stat'd `(size, mtime)` against this map: a match means the file is
    /// unchanged and the expensive content re-hash can be skipped (see
    /// [`Db::touch_location`]). `size` comes from the content row, `mtime` from
    /// the location row.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn present_fingerprints(&self) -> Result<HashMap<PathBuf, (u64, Option<i64>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.path, f.size, l.mtime
             FROM file_locations l
             JOIN files f ON f.id = l.file_id
             WHERE l.present = 1",
        )?;
        let rows = stmt.query_map([], |row| {
            let path: Vec<u8> = row.get(0)?;
            let size: i64 = row.get(1)?;
            let mtime: Option<i64> = row.get(2)?;
            Ok((path_from_bytes(&path), (size as u64, mtime)))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(Into::into)
    }

    /// SQL shared with the `touch_location_uses_the_path_index` test so the
    /// EXPLAIN QUERY PLAN always reflects the real statement.
    const TOUCH_LOCATION_SQL: &str = "UPDATE file_locations
         SET present = 1, last_seen = ?2, created_at = COALESCE(created_at, ?3)
         WHERE path = ?1 AND last_seen < ?2";

    /// Re-stamp an unchanged location as seen this scan: set `present = 1` and
    /// `last_seen = marker` for the row at `path`, without rewriting the content
    /// row (the file's bytes haven't changed). `created_at` is only used to
    /// backfill old rows where the value is still unknown. The `last_seen <
    /// marker` guard keeps it idempotent within a scan. Returns whether a row
    /// was updated.
    ///
    /// # Errors
    /// Returns an error if the update fails.
    pub fn touch_location(
        &self,
        path: &Path,
        marker: i64,
        created_at: Option<i64>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            Self::TOUCH_LOCATION_SQL,
            params![path_to_bytes(path), marker, created_at],
        )?;
        Ok(changed > 0)
    }

    /// Register `path` as a watched root. Idempotent (`UNIQUE(path)`).
    ///
    /// # Errors
    /// Returns an error if the insert fails.
    pub fn add_root(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "INSERT INTO roots (path, added_at) VALUES (?1, ?2)
             ON CONFLICT(path) DO NOTHING",
            params![path_to_bytes(path), unix_now()],
        )?;
        Ok(())
    }

    /// All watched roots, ordered by path.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn list_roots(&self) -> Result<Vec<std::path::PathBuf>> {
        let mut stmt = self.conn.prepare("SELECT path FROM roots ORDER BY path")?;
        let rows = stmt
            .query_map([], |row| {
                let bytes: Vec<u8> = row.get(0)?;
                Ok(path_from_bytes(&bytes))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Stop watching `path`. Returns whether a root row was removed.
    ///
    /// # Errors
    /// Returns an error if the delete fails.
    pub fn remove_root(&self, path: &Path) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM roots WHERE path = ?1",
            params![path_to_bytes(path)],
        )?;
        Ok(n > 0)
    }

    /// Number of indexed files.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn file_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(n as u64)
    }

    /// The `files.id` for `hash`, if that content is known.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn file_id_by_hash(&self, hash: &Hash) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM files WHERE blake3 = ?1",
                params![hash.to_hex()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// The content hash of `file_id`, or `None` if no such file.
    ///
    /// # Errors
    /// Returns an error if the query fails or a stored hash is unparseable.
    pub fn file_hash(&self, file_id: i64) -> Result<Option<Hash>> {
        let hex: Option<String> = self
            .conn
            .query_row(
                "SELECT blake3 FROM files WHERE id = ?1",
                params![file_id],
                |r| r.get(0),
            )
            .optional()?;
        match hex {
            Some(h) => Ok(Some(h.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?)),
            None => Ok(None),
        }
    }

    /// Inputs for a SHA-256-domain repo pull (bridge repos): the owned SHA-256
    /// values (parsed into `Hash` so `bucket_key` masks them like any 32-byte
    /// hash) and a lowercase-sha256-hex → BLAKE3 `Hash` map for translating the
    /// repo's sha256-keyed mappings back to local file identities. Only `files`
    /// rows with a non-null `sha256` are included; NULL-sha files are skipped
    /// (covered after `backfill_sha256`).
    ///
    /// Malformed rows (sha256 or blake3 values that do not parse as a valid hex
    /// hash) are **skipped** with a structured WARN rather than propagating an
    /// error that would abort the whole pull. This matches the per-file pull
    /// path introduced in #143. Both keys are lowercased before parsing so the
    /// map lookup and the parse agree regardless of stored case.
    ///
    /// Returns the count of skipped malformed rows alongside the usable pairs.
    ///
    /// # Errors
    /// Returns an error if the query itself fails (not for individual row
    /// parse failures, which are counted and warned about).
    pub fn sha256_domain_pull_inputs(&self) -> Result<(Vec<Hash>, HashMap<String, Hash>, u64)> {
        let mut stmt = self
            .conn
            .prepare("SELECT blake3, sha256 FROM files WHERE sha256 IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut keys = Vec::new();
        let mut map = HashMap::new();
        let mut malformed: u64 = 0;
        let mut malformed_samples: Vec<String> = Vec::new();
        for row in rows {
            let (blake_hex, sha_hex) = row?;
            // Lowercase before parsing: the per-file pull path (ops.rs) does the
            // same, so both paths agree regardless of how values were stored.
            let blake_lc = blake_hex.to_lowercase();
            let sha_lc = sha_hex.to_lowercase();
            let blake: Hash = match blake_lc.parse() {
                Ok(h) => h,
                Err(_) => {
                    malformed += 1;
                    if malformed_samples.len() < 3 {
                        let truncated: String = blake_lc.chars().take(64).collect();
                        malformed_samples.push(truncated);
                    }
                    continue;
                }
            };
            let sha_key: Hash = match sha_lc.parse() {
                Ok(h) => h,
                Err(_) => {
                    malformed += 1;
                    if malformed_samples.len() < 3 {
                        let truncated: String = sha_lc.chars().take(64).collect();
                        malformed_samples.push(truncated);
                    }
                    continue;
                }
            };
            keys.push(sha_key);
            // Key the map on the already-lowercased sha hex so the lookup
            // in ops.rs (which also lowercases) always finds the entry.
            map.insert(sha_lc, blake);
        }
        if malformed > 0 {
            tracing::warn!(
                target: "db",
                malformed,
                sample = %malformed_samples.join(", "),
                "sha256_domain_pull_inputs: skipped rows with unparseable blake3 or \
                 sha256 (data integrity issue); those files will not participate in \
                 the sha256-domain pull"
            );
        }
        Ok((keys, map, malformed))
    }

    /// Every owned content hash (one per `files` row) — the set whose buckets a
    /// pull requests. Order is unspecified; callers dedupe by bucket key.
    ///
    /// # Errors
    /// Returns an error if the query fails or a stored hash is unparseable.
    pub fn owned_hashes(&self) -> Result<Vec<Hash>> {
        let mut stmt = self.conn.prepare("SELECT blake3 FROM files")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for hex in rows {
            let hash = hex?.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            out.push(hash);
        }
        Ok(out)
    }

    /// Highest content-row id in `files`, or 0 for an empty library.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn max_file_id(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM files", [], |r| r.get(0))
            .map_err(Into::into)
    }

    /// Highest `sha256_seq` ever reserved — the monotonic meta counter, NOT
    /// `MAX(files.sha256_seq)`. Reading the counter (rather than the max over
    /// `files`) keeps the watermark monotonic across row deletion: a deleted
    /// max-seq row must never let the next SHA-256 gain reissue that value.
    /// This is the SHA-256 sibling of [`Db::max_file_id`].
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn max_sha256_seq(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT value FROM sha256_seq_counter WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Reserve `n` fresh `sha256_seq` values on `conn`, returning the highest
    /// reserved; the reserved contiguous range is `(hi - n + 1 ..= hi)`. Must be
    /// called inside the caller's write transaction so reservation and stamping
    /// commit atomically. SQLite is single-writer, so the read-modify-write is
    /// race-free without extra locking.
    fn reserve_sha256_seq(conn: &rusqlite::Connection, n: i64) -> Result<i64> {
        conn.query_row(
            "UPDATE sha256_seq_counter SET value = value + ?1 WHERE id = 1 RETURNING value",
            params![n],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// Bucket keys for files imported after `after_file_id`, keyed by `files.id`.
    ///
    /// # Errors
    /// Returns an error if the query fails or a stored hash is unparseable.
    pub fn owned_bucket_keys_after_file_id(
        &self,
        prefix_bits: u32,
        after_file_id: i64,
    ) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT blake3 FROM files WHERE id > ?1")?;
        let rows = stmt.query_map(params![after_file_id], |r| r.get::<_, String>(0))?;
        let mut keys = Vec::new();
        for hex in rows {
            let hash: Hash = hex?.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            keys.push(bucket_key(&hash, prefix_bits));
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// Bucket keys for files that GAINED their sha256 after `after_seq`, keyed by
    /// `files.sha256_seq`. The SHA-256 analogue of
    /// [`Db::owned_bucket_keys_after_file_id`]: a file's `sha256_seq` is stamped
    /// when its sha256 becomes known (not at import), so a backfilled file
    /// correctly appears here even though its `files.id` never moved. Keys are
    /// lowercased implicitly — stored sha256 is normalised on write (migration
    /// 0035) — parsed, bucketed, sorted and deduped.
    ///
    /// Malformed sha256 rows are skipped with a WARN (#205 / #158 parity with
    /// [`Db::sha256_domain_pull_inputs`]) — a data-integrity issue must not abort
    /// the incremental delta path.
    ///
    /// # Errors
    /// Returns an error if the query itself fails (not for individual row parse
    /// failures, which are counted and warned about).
    pub fn owned_sha256_bucket_keys_after_seq(
        &self,
        prefix_bits: u32,
        after_seq: i64,
    ) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sha256 FROM files WHERE sha256_seq > ?1")?;
        let rows = stmt.query_map(params![after_seq], |r| r.get::<_, String>(0))?;
        let mut keys = Vec::new();
        let mut malformed: u64 = 0;
        let mut malformed_samples: Vec<String> = Vec::new();
        for hex in rows {
            let sha_lc = hex?.to_lowercase();
            let hash: Hash = match sha_lc.parse() {
                Ok(h) => h,
                Err(_) => {
                    malformed += 1;
                    if malformed_samples.len() < 3 {
                        malformed_samples.push(sha_lc.chars().take(64).collect());
                    }
                    continue;
                }
            };
            keys.push(bucket_key(&hash, prefix_bits));
        }
        if malformed > 0 {
            tracing::warn!(
                target: "db",
                malformed,
                sample = %malformed_samples.join(", "),
                "owned_sha256_bucket_keys_after_seq: skipped rows with unparseable sha256 \
                 (data integrity issue); those files will not contribute new bucket keys \
                 to this incremental pull"
            );
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// The `files.id` owning a location at `path`, if any.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn file_id_by_path(&self, path: &Path) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT file_id FROM file_locations WHERE path = ?1",
                params![path_to_bytes(path)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Intern a tag into the dictionary, returning its id. Idempotent: the same
    /// `(namespace, subtag)` always maps to one row.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn intern_tag(&self, tag: &Tag) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES (?1, ?2)
             ON CONFLICT(namespace, subtag) DO NOTHING",
            params![tag.namespace, tag.subtag],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM tags WHERE namespace = ?1 AND subtag = ?2",
            params![tag.namespace, tag.subtag],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Intern a tag through `cache` and `pending`, using
    /// [`Connection::prepare_cached`] so each SQL statement is parsed once per
    /// connection rather than once per call.
    ///
    /// On a hit in either map the SQL layer is never touched. On a miss an
    /// `INSERT … ON CONFLICT DO NOTHING` is issued (idempotent, identical to
    /// [`Db::intern_tag`]) followed by a `SELECT id`; the result is stored in
    /// `pending`, not `cache`. Callers running inside a transaction merge
    /// `pending` into `cache` only after committing, so a rollback never
    /// leaves `cache` holding ids of rows that no longer exist.
    ///
    /// # Errors
    /// Returns an error if either SQL statement fails.
    fn intern_tag_cached(
        &self,
        tag: &Tag,
        cache: &TagCache,
        pending: &mut TagCache,
    ) -> Result<i64> {
        let key = (tag.namespace.clone(), tag.subtag.clone());
        if let Some(&id) = cache.0.get(&key).or_else(|| pending.0.get(&key)) {
            return Ok(id);
        }
        let mut insert = self.conn.prepare_cached(
            "INSERT INTO tags (namespace, subtag) VALUES (?1, ?2)
             ON CONFLICT(namespace, subtag) DO NOTHING",
        )?;
        insert.execute(params![tag.namespace, tag.subtag])?;
        let mut select = self
            .conn
            .prepare_cached("SELECT id FROM tags WHERE namespace = ?1 AND subtag = ?2")?;
        let id: i64 = select.query_row(params![tag.namespace, tag.subtag], |r| r.get(0))?;
        pending.0.insert(key, id);
        Ok(id)
    }

    /// Intern an origin name into the `origins` dictionary, returning its id.
    /// Idempotent: the same `name` always maps to one row. Origin is asserted,
    /// not proven (ADR 0026) — this stores a claimed generation source, not a
    /// verified fact.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn intern_origin(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO origins (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            params![name],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM origins WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Intern an origin through a per-call `cache`, using `prepare_cached` so the
    /// SQL is parsed once per connection. On a cache hit the SQL layer is never
    /// touched. Simplified mirror of [`Db::intern_tag_cached`] for the `origins`
    /// table — unlike the tag version this does NOT split a committed `cache` from
    /// a `pending` map for rollback safety. The caller must therefore use a fresh
    /// `HashMap` scoped to a single transaction and discard it on rollback; a cache
    /// that outlives a rolled-back transaction may hold ids of rows that no longer
    /// exist.
    ///
    /// # Errors
    /// Returns an error if either SQL statement fails.
    fn intern_origin_cached(
        &self,
        name: &str,
        cache: &mut std::collections::HashMap<String, i64>,
    ) -> Result<i64> {
        if let Some(&id) = cache.get(name) {
            return Ok(id);
        }
        let mut insert = self.conn.prepare_cached(
            "INSERT INTO origins (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
        )?;
        insert.execute(params![name])?;
        let mut select = self
            .conn
            .prepare_cached("SELECT id FROM origins WHERE name = ?1")?;
        let id: i64 = select.query_row(params![name], |r| r.get(0))?;
        cache.insert(name.to_string(), id);
        Ok(id)
    }

    /// The asserted generation origin name (ADR 0026) stored in
    /// `mappings.origin_id` for a specific `(service_id, file_id, tag_id)` row.
    /// Returns `None` when the row's `origin_id` is NULL (manual) or the row
    /// does not exist.
    ///
    /// Used by integration tests to verify that a pulled mapping carried its
    /// asserted origin through the merge pipeline (#162 Task 14).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn pulled_mapping_origin(
        &self,
        service_id: i64,
        file_id: i64,
        tag_id: i64,
    ) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT o.name
               FROM mappings m
               LEFT JOIN origins o ON o.id = m.origin_id
              WHERE m.service_id = ?1 AND m.file_id = ?2 AND m.tag_id = ?3",
        )?;
        match stmt.query_row(params![service_id, file_id, tag_id], |r| r.get(0)) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The id of the *seeded* local service (lowest-id `scope = 'local'`) — the
    /// default write target. Use [`local_service_ids`] for read scoping.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    ///
    /// [`local_service_ids`]: Db::local_service_ids
    pub fn local_service_id(&self) -> Result<i64> {
        let id: i64 = self.conn.query_row(
            "SELECT id FROM services WHERE scope = 'local' ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Monotonic version of relation-affecting state (siblings, parents,
    /// services, author trust, block rules, authored mappings). Bumped by
    /// triggers, so writes on the writer connection are visible to read-only
    /// connections — the invalidation signal for the relation-graph cache.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn relation_graph_version(&self) -> Result<i64> {
        let v = self.conn.query_row(
            "SELECT version FROM relation_graph_version WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    /// Ids of **all** local (`scope = 'local'`) services, ordered
    /// `priority DESC, id ASC` — the seeded service plus any created later
    /// (e.g. the Hydrus import service). Local-only reads and the
    /// local-exemption filters draw from this set.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn local_service_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM services WHERE scope = 'local' ORDER BY priority DESC, id ASC",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// The generation `origin` (ADR 0026) of the local mapping supplying
    /// `(file_id, tag_id)`, resolved by the local-service ordering
    /// `priority DESC, id ASC` (the same tiebreak as [`Db::local_service_ids`]).
    /// Returns `None` when the winning local service is manual (origin NULL —
    /// the "my tags" case) or when no local service supplies the pair. This is
    /// the publish-side provenance-by-location read: the tag is published under
    /// the source service's origin, or manual when promoted into "my tags".
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn origin_of_local_mapping(&self, file_id: i64, tag_id: i64) -> Result<Option<String>> {
        let origin: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT s.origin
                   FROM mappings m
                   JOIN services s ON s.id = m.service_id
                  WHERE m.file_id = ?1 AND m.tag_id = ?2
                    AND m.status = 'current' AND s.scope = 'local'
                  ORDER BY s.priority DESC, s.id ASC
                  LIMIT 1",
                params![file_id, tag_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(origin.flatten())
    }

    /// Subscribe a shared service named `name` to `url`. A detached shared
    /// service with this name is re-attached (same row — its kept tags go live
    /// again); otherwise a new row is created. Errors if the name belongs to a
    /// still-subscribed service.
    ///
    /// # Errors
    /// Returns an error if the name is already subscribed or a statement fails.
    pub fn subscribe_shared_service(
        &self,
        name: &str,
        url: &str,
        origin: Option<&str>,
    ) -> Result<i64> {
        let existing: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, url FROM services WHERE scope = 'shared' AND name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((id, None)) => {
                self.conn.execute(
                    "UPDATE services SET url = ?2 WHERE id = ?1",
                    params![id, url],
                )?;
                Ok(id)
            }
            Some((_, Some(_))) => Err(Error::Invalid(format!(
                "repo name already subscribed: {name}"
            ))),
            None => {
                self.conn.execute(
                    "INSERT INTO services (name, scope, url, origin) VALUES (?1, 'shared', ?2, ?3)",
                    params![name, url, origin],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    /// Create a shared (network) service named `name`, bound to `url`. Returns the
    /// new service id. Delegates to [`Db::subscribe_shared_service`].
    ///
    /// # Errors
    /// Returns an error if the name is already subscribed or the statement fails.
    pub fn add_shared_service(&self, name: &str, url: &str, origin: Option<&str>) -> Result<i64> {
        self.subscribe_shared_service(name, url, origin)
    }

    /// Detach a shared service: clear its URL so it is no longer subscribed
    /// (listed or pullable) while keeping the row and every tag it contributed.
    /// The inverse of a same-name [`Db::subscribe_shared_service`].
    ///
    /// Pull cursors are deliberately **kept**. A detach preserves the tags the
    /// service contributed, and re-attaching the same repo should not force a
    /// full re-pull. Invalidation belongs to the operation that actually
    /// changes which repo the name points at — see [`Db::set_service_url`],
    /// which clears the state when it replaces one URL with a different one.
    /// (`repos_remove_handler` also re-attaches via `set_service_url` when its
    /// toml write fails; clearing here would make that "rolled back" path
    /// silently destroy the cursors it claims to have restored.)
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn detach_service(&self, service_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE services SET url = NULL WHERE id = ?1 AND scope = 'shared'",
            params![service_id],
        )?;
        Ok(())
    }

    /// Point a subscribed shared service at a new URL (boot reconcile: the toml
    /// is the source of truth for where a named repo lives).
    ///
    /// Clears `service_domain_pull_state` **only when this replaces a different
    /// existing URL**: the new URL may point at a completely different repo
    /// whose cursor sequence is unrelated, and a re-pointed repo would
    /// otherwise send `since=<old cursor>` to the new server and silently skip
    /// its early history.
    ///
    /// Re-attaching where there was no URL (the row was detached, or this is
    /// the same URL) keeps the cursors: nothing has changed about which repo
    /// the name refers to, so forcing a full re-pull would be pure cost. This
    /// is also what makes `repos_remove_handler`'s failed-toml-write rollback
    /// actually restore the state it detached.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn set_service_url(&self, service_id: i64, url: &str) -> Result<()> {
        // Read the current URL first: only a genuine re-point invalidates.
        let previous: Option<String> = self
            .conn
            .query_row(
                "SELECT url FROM services WHERE id = ?1 AND scope = 'shared'",
                params![service_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        self.conn.execute(
            "UPDATE services SET url = ?2 WHERE id = ?1 AND scope = 'shared'",
            params![service_id, url],
        )?;
        if previous.is_some_and(|prev| prev != url) {
            self.conn.execute(
                "DELETE FROM service_domain_pull_state WHERE service_id = ?1",
                params![service_id],
            )?;
        }
        Ok(())
    }

    /// The subscribed (url IS NOT NULL) shared service named `name`, if one exists.
    /// Detached services (url = NULL) are excluded.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn shared_service_by_name(&self, name: &str) -> Result<Option<SharedService>> {
        let svc = self
            .conn
            .query_row(
                "SELECT id, name, url FROM services WHERE scope = 'shared' AND url IS NOT NULL AND name = ?1",
                params![name],
                |r| {
                    Ok(SharedService {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        url: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(svc)
    }

    /// Whether ANY shared-scope service row (attached or detached) holds this
    /// name.  The subscribe-time uniqueness probe must use this — not
    /// `shared_service_by_name`, which hides detached rows — because
    /// `subscribe_shared_service` re-attaches a detached row of the same name,
    /// which must never happen implicitly under a server-advertised name.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn shared_service_name_taken(&self, name: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM services WHERE scope = 'shared' AND name = ?1 LIMIT 1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Every subscribed (url IS NOT NULL) shared service, ordered by id.
    /// Detached services (url = NULL) are excluded.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn list_shared_services(&self) -> Result<Vec<SharedService>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, url FROM services WHERE scope = 'shared' AND url IS NOT NULL ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SharedService {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The priority of a service (higher wins on conflict).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn service_priority(&self, service_id: i64) -> Result<i64> {
        let p = self.conn.query_row(
            "SELECT priority FROM services WHERE id = ?1",
            params![service_id],
            |r| r.get(0),
        )?;
        Ok(p)
    }

    /// The unix-seconds timestamp of the last relation pull into `service_id`,
    /// or `None` if it has never been relation-pulled.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn last_relation_pull_at(&self, service_id: i64) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT last_relation_pull_at FROM services WHERE id = ?1",
            params![service_id],
            |r| r.get::<_, Option<i64>>(0),
        )?)
    }

    /// The last incremental relation-pull cursor for `service_id`, or `None` if it
    /// has never been incrementally pulled (fresh, or an old repo using fallback).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn relation_cursor(&self, service_id: i64) -> Result<Option<i64>> {
        let v: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT relation_cursor FROM services WHERE id = ?1",
                params![service_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    // ── Store-generation tracking (#194) ─────────────────────────────────────

    /// The store-generation id last recorded for `service_id`, or `None` if
    /// this service has never advertised one (pre-feature repo or never seeded).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn service_store_generation(&self, service_id: i64) -> Result<Option<String>> {
        let v: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT store_generation FROM services WHERE id = ?1",
                params![service_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// Persist the store-generation id for `service_id`.
    ///
    /// # Errors
    /// Returns an error if the statement fails or no such service exists.
    pub fn set_service_store_generation(&self, service_id: i64, generation: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE services SET store_generation = ?1 WHERE id = ?2",
            params![generation, service_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("no service with id {service_id}")));
        }
        Ok(())
    }

    /// Reset all incremental-pull cursors for `service_id` in one transaction.
    ///
    /// Sets `relation_cursor` to NULL and deletes all rows in
    /// `service_domain_pull_state` for this service. On the next sync tick the
    /// daemon re-pulls relations and every mapping domain from zero, exactly as
    /// it would on a freshly added service.
    ///
    /// Called when a repo's `store_generation` changes (the server was
    /// re-seeded and seq numbers may have reshuffled).
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn reset_service_cursors(&self, service_id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE services SET relation_cursor = NULL WHERE id = ?1",
            params![service_id],
        )?;
        tx.execute(
            "DELETE FROM service_domain_pull_state WHERE service_id = ?1",
            params![service_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The last incremental mapping-pull cursor for `service_id` in `domain`
    /// (`"blake3"` / `"sha256"` — `naiad_netproto::HashDomain`'s wire
    /// spelling), or `None`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn mapping_cursor(&self, service_id: i64, domain: &str) -> Result<Option<i64>> {
        let v: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT mapping_cursor FROM service_domain_pull_state
                 WHERE service_id = ?1 AND domain = ?2",
                params![service_id, domain],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// The max `files.id` covered by at least one full mapping pull of
    /// `service_id` in `domain`, or `None`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn last_pull_file_marker(&self, service_id: i64, domain: &str) -> Result<Option<i64>> {
        let v: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT last_pull_file_marker FROM service_domain_pull_state
                 WHERE service_id = ?1 AND domain = ?2",
                params![service_id, domain],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// Store the incremental mapping pull cursor and covered file marker for
    /// one `(service, domain)` pair. Upserts.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn set_mapping_pull_state(
        &self,
        service_id: i64,
        domain: &str,
        cursor: u64,
        marker: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO service_domain_pull_state
                 (service_id, domain, mapping_cursor, last_pull_file_marker)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(service_id, domain) DO UPDATE SET
                 mapping_cursor = excluded.mapping_cursor,
                 last_pull_file_marker = excluded.last_pull_file_marker",
            params![
                service_id,
                domain,
                i64::try_from(cursor).unwrap_or(i64::MAX),
                marker
            ],
        )?;
        Ok(())
    }

    /// Clear one `(service, domain)` pair's incremental pull state. Other
    /// domains on the same service are untouched.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn clear_mapping_pull_state(&self, service_id: i64, domain: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM service_domain_pull_state WHERE service_id = ?1 AND domain = ?2",
            params![service_id, domain],
        )?;
        Ok(())
    }

    /// Set a service's priority. Higher number = higher priority on conflict.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] if no service with `service_id` exists, or an
    /// error if the statement fails.
    pub fn set_service_priority(&self, service_id: i64, priority: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE services SET priority = ?1 WHERE id = ?2",
            params![priority, service_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("no service with id {service_id}")));
        }
        Ok(())
    }

    /// Read an internal scalar from `app_settings`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn app_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Upsert an internal scalar into `app_settings`.
    ///
    /// # Errors
    /// Returns an error if the write fails.
    pub fn set_app_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Add a block rule, validating and normalizing `target` by `kind`:
    /// a `Tag` is parsed and stored as `ns:subtag`; a `TagPattern` is validated
    /// and stored in canonical form (lowercased, whitespace-collapsed); an
    /// `Author` must be 64 hex chars (lowercased). Idempotent under
    /// `UNIQUE(kind, target)` — re-adding an existing rule returns its id
    /// without changing it; when the rule already exists, the `note` argument
    /// is ignored (the existing row, including its note, is left unchanged).
    /// Returns the rule id.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] if `target` is malformed for `kind`, or an
    /// error if a statement fails.
    pub fn add_block_rule(&self, kind: BlockKind, target: &str, note: Option<&str>) -> Result<i64> {
        let normalized = match kind {
            BlockKind::Tag => Tag::parse(target)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .to_string(),
            BlockKind::TagPattern => TagPattern::parse(target)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .to_string(),
            BlockKind::Author => {
                let t = target.trim().to_lowercase();
                if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(Error::Invalid(format!(
                        "author must be 64 hex chars, got {target:?}"
                    )));
                }
                t
            }
        };
        self.conn.execute(
            "INSERT INTO block_rules (kind, target, note, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(kind, target) DO NOTHING",
            params![kind.as_str(), normalized, note, unix_now()],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM block_rules WHERE kind = ?1 AND target = ?2",
            params![kind.as_str(), normalized],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Every block rule, ordered by id.
    ///
    /// # Errors
    /// Returns an error if a query fails, or [`Error::Invalid`] if a stored kind
    /// is unrecognized (should not happen for rows this code wrote).
    pub fn list_block_rules(&self) -> Result<Vec<BlockRule>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, target, note, created_at FROM block_rules ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, kind, target, note, created_at) in rows {
            out.push(BlockRule {
                id,
                kind: BlockKind::parse(&kind)?,
                target,
                note,
                created_at,
            });
        }
        Ok(out)
    }

    /// Remove a block rule by id.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] if no rule has that id, or an error if the
    /// statement fails.
    pub fn remove_block_rule(&self, id: i64) -> Result<()> {
        let n = self
            .conn
            .execute("DELETE FROM block_rules WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(Error::NotFound(format!("no block rule with id {id}")));
        }
        Ok(())
    }

    /// Add a mapping rejection (local hide). Idempotent.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn add_rejection(
        &self,
        service_id: i64,
        file_id: i64,
        tag_id: i64,
        note: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO mapping_rejections
               (service_id, file_id, tag_id, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![service_id, file_id, tag_id, note, unix_now()],
        )?;
        Ok(())
    }

    /// Undo a rejection. Idempotent.
    ///
    /// # Errors
    /// Returns an error if the delete fails.
    pub fn remove_rejection(&self, service_id: i64, file_id: i64, tag_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM mapping_rejections
             WHERE service_id = ?1 AND file_id = ?2 AND tag_id = ?3",
            params![service_id, file_id, tag_id],
        )?;
        Ok(())
    }

    /// All rejections, optionally scoped to one file (the per-file "Rejected"
    /// disclosure). Joins `tags`/`services` for display names.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn list_rejections(&self, file_id: Option<i64>) -> Result<Vec<Rejection>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.service_id, sv.name, r.file_id, r.tag_id, t.namespace || ':' || t.subtag,
                    r.note, r.created_at, f.blake3
               FROM mapping_rejections r
               JOIN services sv ON sv.id = r.service_id
               JOIN tags t ON t.id = r.tag_id
               JOIN files f ON f.id = r.file_id
              WHERE (?1 IS NULL OR r.file_id = ?1)
              ORDER BY r.created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![file_id], |r| {
                Ok(Rejection {
                    service_id: r.get(0)?,
                    service: r.get(1)?,
                    file_id: r.get(2)?,
                    tag_id: r.get(3)?,
                    tag: r.get(4)?,
                    note: r.get(5)?,
                    created_at: r.get(6)?,
                    hash: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── contributor identity (ADR 0020 §6, migration 0029) ───────────────────

    /// Return the contributor identity row for `service_id`.
    ///
    /// # Errors
    /// Returns an error if the service does not exist or a query fails.
    pub fn contributor_identity(&self, service_id: i64) -> Result<ContributorIdentity> {
        let repo_anchor = self.conn.query_row(
            "SELECT repo_anchor FROM services WHERE id = ?1",
            params![service_id],
            |r| r.get::<_, Option<String>>(0),
        )?;
        Ok(ContributorIdentity { repo_anchor })
    }

    /// Freeze the derivation anchor for `service_id` to `anchor`. A second call
    /// with a different value is a **no-op** — the anchor is written once and
    /// never updated (ADR 0020 §6: a #83 rotation moves the verification pin,
    /// never this anchor).
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn freeze_repo_anchor(&self, service_id: i64, anchor: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE services SET repo_anchor = ?2
             WHERE id = ?1 AND repo_anchor IS NULL",
            params![service_id, anchor],
        )?;
        Ok(())
    }

    /// The service ids a read should draw from, ordered highest-priority first
    /// (ties broken by ascending id). `LocalOnly` yields every local service
    /// (all `scope = 'local'` rows) so that mappings on any local service are
    /// included.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn included_services(&self, scope: ReadScope) -> Result<Vec<i64>> {
        match scope {
            ReadScope::LocalOnly => self.local_service_ids(),
            ReadScope::Merged => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM services ORDER BY priority DESC, id ASC")?;
                let ids = stmt
                    .query_map([], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(ids)
            }
        }
    }

    /// Replace this shared service's mappings with the owned entries from a pulled
    /// snapshot, authoritatively for **every** domain. A shared service is sourced
    /// entirely from its one repo, so each pull is authoritative: all of the
    /// service's mappings are cleared, then every owned `(hash, tag)` is
    /// reinserted. This makes upstream *removes* propagate (a retracted tag is
    /// simply absent from the next snapshot). Only this `service_id` is touched —
    /// the local service is untouched.
    ///
    /// Rows are written with the native-domain provenance bit. Use
    /// [`Db::merge_pulled_mappings_in_domain`] for a repo that serves more than
    /// one hash domain: this function's whole-service `DELETE` would wipe the
    /// other domain's rows (#151).
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn merge_pulled_mappings(
        &self,
        service_id: i64,
        entries: &[(Hash, Vec<Tag>)],
    ) -> Result<MergeStats> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM mappings WHERE service_id = ?1",
            params![service_id],
        )?;
        let mut stats = MergeStats::default();
        // Per-call caches. Both are built here and dropped when the call
        // returns, so a rolled-back transaction can never leave ids for
        // uncommitted tag rows visible to a later call. `intern_tag_cached`
        // consults `pending` as well as `cache`, so tags repeated within this
        // call still resolve from memory rather than re-querying.
        let local_cache = TagCache::new();
        let mut pending = TagCache::new();
        for (hash, tags) in entries {
            let Some(file_id) = self.file_id_by_hash(hash)? else {
                continue; // not in the library; download discarded, never stored
            };
            stats.matched_files += 1;
            for tag in tags {
                let tag_id = self.intern_tag_cached(tag, &local_cache, &mut pending)?;
                let changed = tx.execute(
                    "INSERT INTO mappings
                         (file_id, tag_id, service_id, status, created_at)
                     VALUES (?1, ?2, ?3, 'current', ?4)
                     ON CONFLICT(file_id, tag_id, service_id) DO NOTHING",
                    params![file_id, tag_id, service_id, unix_now()],
                )?;
                stats.mappings += changed as u64;
            }
        }
        tx.commit()?;
        tracing::debug!(target: "db", service_id, matched = stats.matched_files, mappings = stats.mappings, "merged pulled mappings (snapshot)");
        Ok(stats)
    }

    /// The mappings a service currently carries, shaped as [`MergeStats`].
    ///
    /// Used to report a dual-domain pull's result (#151): the two domain legs
    /// merge independently, so their individual stats cannot simply be summed —
    /// a file both domains matched would be counted twice, and an incremental
    /// leg's numbers describe only what changed. Reading the merged totals once
    /// avoids both problems.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn service_mapping_stats(&self, service_id: i64) -> Result<MergeStats> {
        let (matched_files, mappings) = self.conn.query_row(
            "SELECT COUNT(DISTINCT file_id), COUNT(*) FROM mappings WHERE service_id = ?1",
            params![service_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(MergeStats {
            matched_files: matched_files.max(0) as u64,
            mappings: mappings.max(0) as u64,
        })
    }

    /// The mappings a service carries for `hashes`, shaped as [`MergeStats`].
    ///
    /// The file-scoped counterpart of [`Db::service_mapping_stats`], used to
    /// report a per-file pull that merged each domain separately (#151).
    /// `matched_files` counts requested files that ended up with at least one
    /// mapping on this service, which is the same population the single-merge
    /// form used to report: a requested file upstream sent nothing for has all
    /// of its rows authoritatively removed, so it contributes zero either way.
    /// Hashes not in the library are skipped, as everywhere else.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn file_mapping_stats(&self, service_id: i64, hashes: &[Hash]) -> Result<MergeStats> {
        let mut stats = MergeStats::default();
        let mut stmt = self.conn.prepare_cached(
            "SELECT COUNT(*) FROM mappings WHERE service_id = ?1 AND file_id = ?2",
        )?;
        for hash in hashes {
            let Some(file_id) = self.file_id_by_hash(hash)? else {
                continue;
            };
            let n: i64 = stmt.query_row(params![service_id, file_id], |r| r.get(0))?;
            if n > 0 {
                stats.matched_files += 1;
                stats.mappings += n.max(0) as u64;
            }
        }
        Ok(stats)
    }

    /// Domain-scoped variant of [`Db::merge_pulled_mappings`]: authoritative for
    /// `domain` across the whole service, and for that domain only.
    ///
    /// This is the primitive that lets a dual-domain repo pull each domain
    /// independently (#151). Instead of deleting the service's rows outright, it
    /// retracts only this domain's provenance bit, re-asserts it for the pulled
    /// entries, and finally reaps rows no domain supplies any more. A row that
    /// the other domain still supplies keeps a non-zero mask and survives, so
    /// the two legs of a dual-domain pull cannot destroy each other's work.
    ///
    /// `mappings` in the returned stats counts the rows this domain supplies
    /// after the merge, not the rows newly inserted by it — a re-asserted row is
    /// indistinguishable from a fresh one once the mask is set, and the previous
    /// whole-service behaviour (delete everything, re-insert everything) made the
    /// two identical anyway.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn merge_pulled_mappings_in_domain(
        &self,
        service_id: i64,
        domain: &str,
        entries: TaggedEntries<'_>,
    ) -> Result<MergeStats> {
        let bit = domain_bit(domain);
        let tx = self.conn.unchecked_transaction()?;

        // Retract this domain's claim service-wide, in two steps that look
        // redundant but are the difference between this merge costing the same
        // as the whole-service one and costing 2.5x more (measured at 4.28M
        // rows; see docs/perf/2026-07-28-issue-151-*).
        //
        // Rows THIS domain alone supplies — the overwhelming majority — are
        // deleted outright, exactly as `merge_pulled_mappings` would, so their
        // re-insert below takes the cheap empty-slot path. Only rows shared with
        // another domain need the read-modify-write bit clear, and there are
        // normally few of them.
        //
        // Doing the bit clear for everything instead would rewrite all 4.28M
        // rows here and then rewrite them AGAIN via `ON CONFLICT DO UPDATE`,
        // paying the row triggers twice over.
        tx.execute(
            "DELETE FROM mappings WHERE service_id = ?1 AND domains = ?2",
            params![service_id, bit],
        )?;
        tx.execute(
            "UPDATE mappings SET domains = domains & ~?2
              WHERE service_id = ?1 AND domains & ?2 <> 0",
            params![service_id, bit],
        )?;

        let mut stats = MergeStats::default();
        // Per-call caches. Both are built here and dropped when the call
        // returns, so a rolled-back transaction can never leave ids for
        // uncommitted tag rows visible to a later call. `intern_tag_cached`
        // consults `pending` as well as `cache`, so tags repeated within this
        // call still resolve from memory rather than re-querying.
        let local_cache = TagCache::new();
        let mut pending = TagCache::new();
        let mut origin_cache: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for (hash, tags) in entries {
            let Some(file_id) = self.file_id_by_hash(hash)? else {
                continue; // not in the library; download discarded, never stored
            };
            stats.matched_files += 1;
            for (tag, origin) in tags {
                let tag_id = self.intern_tag_cached(tag, &local_cache, &mut pending)?;
                let origin_id: Option<i64> = match origin {
                    Some(name) => Some(self.intern_origin_cached(name, &mut origin_cache)?),
                    None => None,
                };
                // Re-assert this domain's bit. `DO UPDATE` rather than
                // `DO NOTHING`: the row may survive because the other domain
                // also supplies it, in which case the bit must go back on.
                //
                // The conflict clause sets `domains` and nothing else. Adding
                // `status = 'current'` here would be a no-op in value — every
                // pulled row is already 'current' — but it would put `status` in
                // the UPDATE's column list and so fire
                // `mappings_completion_counts_after_update`, a three-statement
                // trigger, once per row. That alone accounted for most of a 2.5x
                // regression against the whole-service merge.
                //
                // origin_id rides the INSERT only. It is DELIBERATELY absent from
                // the DO UPDATE SET below: the conflict clause must touch only
                // `domains` so the AFTER UPDATE OF completion-count trigger does
                // not fire per row (#151/0034 perf rule — the trigger fires on the
                // column LIST, not on value change). Consequence: a row supplied by
                // two domains keeps the FIRST domain's origin, which is acceptable
                // for coarse asserted metadata (this merge DELETEs-then-reinserts
                // rows a domain solely supplies, so those get a fresh origin_id).
                tx.execute(
                    "INSERT INTO mappings
                         (file_id, tag_id, service_id, status, created_at, domains, origin_id)
                     VALUES (?1, ?2, ?3, 'current', ?4, ?5, ?6)
                     ON CONFLICT(file_id, tag_id, service_id) DO UPDATE SET
                         domains = domains | ?5",
                    params![file_id, tag_id, service_id, unix_now(), bit, origin_id],
                )?;
                stats.mappings += 1;
            }
        }

        // No reap pass is needed, and adding one would cost a full scan of the
        // service for nothing. The two retraction statements above leave no row
        // at mask 0: rows this domain alone supplied were DELETEd outright, and
        // a shared row only ever loses one of its >= 2 bits. Every write path
        // keeps `domains` non-zero, and the column is NOT NULL DEFAULT 1.
        tx.commit()?;
        tracing::debug!(target: "db", service_id, domain, matched = stats.matched_files, mappings = stats.mappings, "merged pulled mappings (domain-scoped snapshot)");
        Ok(stats)
    }

    /// File-scoped variant of [`Db::merge_pulled_mappings`]: authoritative only
    /// for `hashes`. Rows for other files on this service are untouched, so a
    /// per-file pull can never wipe the rest of a repo service. Entries whose
    /// hash is not requested or not in the library are discarded, never stored.
    ///
    /// `matched_files` counts only requested files for which upstream sent at
    /// least one entry (i.e. files that appear in `entries`). A requested file
    /// absent from `entries` is still authoritatively cleared — all its mappings
    /// on this service are deleted — but it is not counted in `matched_files`.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn merge_pulled_mappings_for_files(
        &self,
        service_id: i64,
        hashes: &[Hash],
        entries: TaggedEntries<'_>,
    ) -> Result<MergeStats> {
        self.merge_for_files_masked(service_id, hashes, entries, None)
    }

    /// Domain-scoped variant of [`Db::merge_pulled_mappings_for_files`]:
    /// authoritative for `hashes` **within `domain` only**.
    ///
    /// Rows for the requested files that another domain still supplies keep a
    /// non-zero provenance mask and survive, so a per-file pull against a
    /// dual-domain repo can run one merge per domain instead of coalescing every
    /// domain's entries into a single call (#151, and it retires the one-merge
    /// constraint #143 was written under).
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn merge_pulled_mappings_for_files_in_domain(
        &self,
        service_id: i64,
        hashes: &[Hash],
        domain: &str,
        entries: TaggedEntries<'_>,
    ) -> Result<MergeStats> {
        self.merge_for_files_masked(service_id, hashes, entries, Some(domain_bit(domain)))
    }

    /// Shared implementation of the two file-scoped merges.
    ///
    /// `bit` selects the provenance scope: `None` is authoritative for every
    /// domain (the whole-service semantics older callers rely on), `Some(bit)`
    /// only for that domain's rows. In the `Some` case a removal clears the bit
    /// rather than deleting the row outright, and rows are reaped only once no
    /// domain supplies them.
    fn merge_for_files_masked(
        &self,
        service_id: i64,
        hashes: &[Hash],
        entries: TaggedEntries<'_>,
        bit: Option<i64>,
    ) -> Result<MergeStats> {
        // `None` writes the native bit and treats every row as in scope, which
        // reproduces the pre-#151 behaviour exactly.
        let write_bit = bit.unwrap_or(DOMAIN_BIT_BLAKE3);
        let requested: HashSet<&Hash> = hashes.iter().collect();
        let tx = self.conn.unchecked_transaction()?;
        let mut stats = MergeStats::default();
        // Per-call caches. Both are built here and dropped when the call
        // returns, so a rolled-back transaction can never leave ids for
        // uncommitted tag rows visible to a later call. `intern_tag_cached`
        // consults `pending` as well as `cache`, so tags repeated within this
        // call still resolve from memory rather than re-querying.
        let local_cache = TagCache::new();
        let mut pending = TagCache::new();
        let mut origin_cache: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();

        // First pass: resolve file ids and intern tag ids for every entry in
        // `requested`. Doing this once avoids a second `file_id_by_hash` lookup
        // in the diff loop below. Origin is recorded per (file_id, tag_id) so it
        // can be carried into the net-new INSERT without a second pass over entries.
        let mut resolved: HashMap<&Hash, i64> = HashMap::new();
        let mut new_tags_by_file: HashMap<i64, HashSet<i64>> = HashMap::new();
        let mut new_origin_by_file_tag: HashMap<(i64, i64), Option<i64>> = HashMap::new();
        for (hash, tags) in entries {
            if !requested.contains(hash) {
                continue;
            }
            let Some(file_id) = self.file_id_by_hash(hash)? else {
                continue;
            };
            resolved.insert(hash, file_id);
            stats.matched_files += 1;
            let mut tag_ids: HashSet<i64> = HashSet::new();
            for (tag, origin) in tags {
                let tag_id = self.intern_tag_cached(tag, &local_cache, &mut pending)?;
                let origin_id: Option<i64> = match origin {
                    Some(name) => Some(self.intern_origin_cached(name, &mut origin_cache)?),
                    None => None,
                };
                new_origin_by_file_tag.insert((file_id, tag_id), origin_id);
                tag_ids.insert(tag_id);
            }
            new_tags_by_file.insert(file_id, tag_ids);
        }

        // Second pass: for each requested hash, diff current vs new and apply.
        // Re-use the resolved file id from the first pass; fall back to a DB
        // lookup only for hashes that appeared in `hashes` but not in `entries`.
        for hash in hashes {
            let file_id = match resolved.get(hash) {
                Some(&fid) => fid,
                None => match self.file_id_by_hash(hash)? {
                    Some(fid) => fid,
                    None => continue,
                },
            };
            let new_tag_ids = new_tags_by_file.get(&file_id).cloned().unwrap_or_default();

            // Read the current tag_ids for this file on this service. When a
            // domain is selected, only rows that domain supplies are in scope —
            // the other domain's rows are none of this merge's business.
            let current_tag_ids: HashSet<i64> = match bit {
                None => {
                    let mut stmt = self.conn.prepare_cached(
                        "SELECT tag_id FROM mappings WHERE file_id = ?1 AND service_id = ?2",
                    )?;
                    stmt.query_map(params![file_id, service_id], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?
                }
                Some(bit) => {
                    let mut stmt = self.conn.prepare_cached(
                        "SELECT tag_id FROM mappings
                          WHERE file_id = ?1 AND service_id = ?2 AND domains & ?3 <> 0",
                    )?;
                    stmt.query_map(params![file_id, service_id, bit], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?
                }
            };

            // Apply upstream's removals. Whole-service scope deletes the row;
            // domain scope only retracts this domain's claim, leaving a row the
            // other domain still supplies in place.
            for &tag_id in current_tag_ids.difference(&new_tag_ids) {
                match bit {
                    None => {
                        tx.execute(
                            "DELETE FROM mappings
                              WHERE service_id = ?1 AND file_id = ?2 AND tag_id = ?3",
                            params![service_id, file_id, tag_id],
                        )?;
                    }
                    Some(bit) => {
                        tx.execute(
                            "UPDATE mappings SET domains = domains & ~?4
                              WHERE service_id = ?1 AND file_id = ?2 AND tag_id = ?3",
                            params![service_id, file_id, tag_id, bit],
                        )?;
                    }
                }
            }

            // Insert net-new tags. The diff guarantees these are absent *for
            // this scope*, but under domain scope the row may still exist
            // carrying only the other domain's bit, so the conflict clause ORs
            // this domain in rather than assuming a fresh insert.
            //
            // origin_id rides the INSERT only. It is DELIBERATELY absent from
            // the DO UPDATE SET below: the conflict clause must touch only
            // `domains` and `status` so the AFTER UPDATE OF completion-count
            // trigger does not fire per row on origin changes (#151/0034 perf
            // rule — the trigger fires on the column LIST, not on value change).
            // Consequence: a row supplied by two domains keeps the FIRST domain's
            // origin, which is acceptable for coarse asserted metadata.
            for &tag_id in new_tag_ids.difference(&current_tag_ids) {
                let origin_id: Option<i64> = new_origin_by_file_tag
                    .get(&(file_id, tag_id))
                    .copied()
                    .flatten();
                tx.execute(
                    "INSERT INTO mappings
                         (file_id, tag_id, service_id, status, created_at, domains, origin_id)
                     VALUES (?1, ?2, ?3, 'current', ?4, ?5, ?6)
                     ON CONFLICT(file_id, tag_id, service_id) DO UPDATE SET
                         domains = domains | ?5,
                         status = 'current'",
                    params![
                        file_id,
                        tag_id,
                        service_id,
                        unix_now(),
                        write_bit,
                        origin_id
                    ],
                )?;
                stats.mappings += 1;
            }

            // Reap rows for this file that no domain supplies any more. Scoped
            // to the file so the sweep stays as cheap as the merge itself.
            if bit.is_some() {
                tx.execute(
                    "DELETE FROM mappings
                      WHERE service_id = ?1 AND file_id = ?2 AND domains = 0",
                    params![service_id, file_id],
                )?;
            }
        }

        tx.commit()?;
        tracing::debug!(target: "db", service_id, matched = stats.matched_files, mappings = stats.mappings, "merged pulled mappings (file-scoped)");
        Ok(stats)
    }

    /// Merge an incremental mapping delta into one shared service.
    ///
    /// Full buckets are authoritative for their hash ranges and are cleared
    /// before individual current/deleted changes are applied.
    ///
    /// `domain` is the wire spelling of the hash domain this delta belongs to;
    /// the cursor is recorded per `(service, domain)`.
    ///
    /// Every write is scoped to `domain`'s provenance bit (#151): a full
    /// bucket's clear and an upstream retraction both retract only this domain's
    /// claim, and a row is reaped only once no domain supplies it. That is what
    /// makes it safe to run this delta on the BLAKE3 leg of a dual-domain pull
    /// while the SHA-256 leg merges independently.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn merge_mapping_delta(
        &self,
        service_id: i64,
        domain: &str,
        changes: &[MappingDeltaInput],
        full_buckets: &[(String, String)],
        cursor: u64,
        file_marker: i64,
    ) -> Result<MergeStats> {
        let bit = domain_bit(domain);
        let tx = self.conn.unchecked_transaction()?;

        // The per-row work below stays BLAKE3-keyed (the daemon translates
        // sha256 wire rows to blake3 identities before this call), but the
        // full-bucket clear range-scans a HASH COLUMN whose values must match
        // the bucket bounds. For a sha256-domain delta those bounds are sha256
        // prefixes, so the clear must scan files.sha256, not files.blake3.
        let key_col = match domain {
            "sha256" => "sha256",
            // blake3, local, and any unknown domain fall back to blake3 — the
            // same column domain_bit() treats as the default bit.
            _ => "blake3",
        };
        let retract_sql = format!(
            "UPDATE mappings SET domains = domains & ~?4
             WHERE service_id = ?1
               AND domains & ?4 <> 0
               AND file_id IN (SELECT id FROM files WHERE {key_col} >= ?2 AND {key_col} < ?3)"
        );
        let reap_sql = format!(
            "DELETE FROM mappings
             WHERE service_id = ?1
               AND domains = 0
               AND file_id IN (SELECT id FROM files WHERE {key_col} >= ?2 AND {key_col} < ?3)"
        );
        for (lo, hi) in full_buckets {
            // Retract this domain's claim across the bucket, then reap within
            // the same range. Both stay scoped to the bucket: a service-wide
            // `domains = 0` sweep would scan every row on the service — the
            // full-table cost a delta exists to avoid (#151).
            tx.execute(&retract_sql, params![service_id, lo, hi, bit])?;
            tx.execute(&reap_sql, params![service_id, lo, hi])?;
        }

        let mut matched = HashSet::new();
        // Per-call caches. Both are built here and dropped when the call
        // returns, so a rolled-back transaction can never leave ids for
        // uncommitted tag rows visible to a later call. `intern_tag_cached`
        // consults `pending` as well as `cache`, so tags repeated within this
        // call still resolve from memory rather than re-querying.
        let local_cache = TagCache::new();
        let mut pending = TagCache::new();
        let mut origin_cache: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for c in changes {
            tracing::trace!(target: "db", hash = %c.hash, tag = %c.tag, "merge mapping delta row");
            let Some(file_id) = self.file_id_by_hash(&c.hash)? else {
                continue;
            };
            matched.insert(file_id);
            let tag_id = self.intern_tag_cached(&c.tag, &local_cache, &mut pending)?;
            match c.status {
                MappingDeltaStatus::Current => {
                    let origin_id: Option<i64> = match &c.origin {
                        Some(name) => Some(self.intern_origin_cached(name, &mut origin_cache)?),
                        None => None,
                    };
                    // origin_id rides the INSERT only. It is DELIBERATELY absent from
                    // the DO UPDATE SET below: the conflict clause must touch only
                    // `status`, `created_at`, and `domains` so the AFTER UPDATE OF
                    // completion-count trigger does not fire per row on origin changes
                    // (#151/0034 perf rule — the trigger fires on the column LIST,
                    // not on value change). Consequence: a row supplied by two
                    // domains keeps the FIRST domain's origin, which is acceptable
                    // for coarse asserted metadata.
                    tx.execute(
                        "INSERT INTO mappings
                             (file_id, tag_id, service_id, status, created_at, domains, origin_id)
                         VALUES (?1, ?2, ?3, 'current', ?4, ?5, ?6)
                         ON CONFLICT(file_id, tag_id, service_id) DO UPDATE SET
                             status = 'current',
                             created_at = excluded.created_at,
                             domains = domains | ?5",
                        params![file_id, tag_id, service_id, unix_now(), bit, origin_id],
                    )?;
                }
                MappingDeltaStatus::Deleted => {
                    // Retract only this domain's claim, then reap this exact row
                    // if that was the last domain supplying it. Both statements
                    // hit the UNIQUE(file_id, tag_id, service_id) index, so a
                    // retraction costs the same as the old outright DELETE.
                    tx.execute(
                        "UPDATE mappings SET domains = domains & ~?4
                         WHERE file_id = ?1 AND tag_id = ?2 AND service_id = ?3",
                        params![file_id, tag_id, service_id, bit],
                    )?;
                    tx.execute(
                        "DELETE FROM mappings
                         WHERE file_id = ?1 AND tag_id = ?2 AND service_id = ?3
                           AND domains = 0",
                        params![file_id, tag_id, service_id],
                    )?;
                }
            }
        }

        tx.execute(
            "INSERT INTO service_domain_pull_state
                 (service_id, domain, mapping_cursor, last_pull_file_marker)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(service_id, domain) DO UPDATE SET
                 mapping_cursor = excluded.mapping_cursor,
                 last_pull_file_marker = excluded.last_pull_file_marker",
            params![
                service_id,
                domain,
                i64::try_from(cursor).unwrap_or(i64::MAX),
                file_marker
            ],
        )?;
        // NOTE (#142): this is a full count of the service's mappings on EVERY
        // delta merge — cheap on a small library, not free at PTR scale where
        // the service may hold millions of rows, and now on the SHA-256 delta
        // hot path. Pre-existing, not a regression here. The cheap fix (compute
        // MergeStats.mappings only where a caller needs it, or reuse
        // service_mapping_stats which the daemon already reads for the
        // dual-domain summary) is deferred to #108 phase-2 rather than taken
        // here. Recorded so it is not silently paid at scale without a why.
        let stats = MergeStats {
            matched_files: matched.len() as u64,
            mappings: tx.query_row(
                "SELECT COUNT(*) FROM mappings WHERE service_id = ?1",
                params![service_id],
                |r| r.get::<_, i64>(0),
            )? as u64,
        };
        tx.commit()?;
        tracing::debug!(target: "db", service_id, cursor, matched = matched.len() as u64, mappings = stats.mappings, "merged mapping delta");
        Ok(stats)
    }

    /// Replace this shared service's relations with a pulled relation graph.
    /// Like [`Db::merge_pulled_mappings`], each pull is authoritative: both
    /// relation tables are cleared for this `service_id`, then the pulled edges
    /// are interned and re-inserted (author set, signature NULL). Only this
    /// service is touched; the local service is untouched.
    ///
    /// Two client-side guards the dumb repo never enforces: conflicting sibling
    /// ideals for the same `from` collapse to the lexicographically-smallest `to`
    /// (the local schema is `UNIQUE(bad_tag_id, service_id)` — one ideal per
    /// alias), and self-edges (`from == to`) are skipped.
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn merge_pulled_relations(
        &self,
        service_id: i64,
        siblings: &[(Tag, Tag, String)],
        parents: &[(Tag, Tag, String)],
    ) -> Result<RelationMergeStats> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM tag_siblings WHERE service_id = ?1",
            params![service_id],
        )?;
        tx.execute(
            "DELETE FROM tag_parents WHERE service_id = ?1",
            params![service_id],
        )?;
        let mut stats = RelationMergeStats::default();
        // Per-call caches. Both are built here and dropped when the call
        // returns, so a rolled-back transaction can never leave ids for
        // uncommitted tag rows visible to a later call. `intern_tag_cached`
        // consults `pending` as well as `cache`, so tags repeated within this
        // call still resolve from memory rather than re-querying.
        let local_cache = TagCache::new();
        let mut pending = TagCache::new();

        // Collapse conflicting sibling ideals: per `from`, keep the smallest `to`.
        let mut chosen: HashMap<String, (Tag, Tag, String)> = HashMap::new();
        for (from, to, author) in siblings {
            if from == to {
                continue; // self-edge guard
            }
            let key = from.to_string();
            match chosen.get(&key) {
                Some((_, prev_to, _)) if prev_to.to_string() <= to.to_string() => {}
                _ => {
                    chosen.insert(key, (from.clone(), to.clone(), author.clone()));
                }
            }
        }
        for (from, to, author) in chosen.values() {
            let bad_id = self.intern_tag_cached(from, &local_cache, &mut pending)?;
            let ideal_id = self.intern_tag_cached(to, &local_cache, &mut pending)?;
            tx.execute(
                "INSERT INTO tag_siblings
                     (bad_tag_id, ideal_tag_id, service_id, status, author, signature, created_at)
                 VALUES (?1, ?2, ?3, 'current', ?4, NULL, ?5)
                 ON CONFLICT(bad_tag_id, service_id) DO NOTHING",
                params![bad_id, ideal_id, service_id, author, unix_now()],
            )?;
            stats.siblings += 1;
        }

        for (child, parent, author) in parents {
            if child == parent {
                continue; // self-edge guard (add_parent rejects these too)
            }
            let child_id = self.intern_tag_cached(child, &local_cache, &mut pending)?;
            let parent_id = self.intern_tag_cached(parent, &local_cache, &mut pending)?;
            let changed = tx.execute(
                "INSERT INTO tag_parents
                     (child_tag_id, parent_tag_id, service_id, status, author, signature, created_at)
                 VALUES (?1, ?2, ?3, 'current', ?4, NULL, ?5)
                 ON CONFLICT(child_tag_id, parent_tag_id, service_id) DO NOTHING",
                params![child_id, parent_id, service_id, author, unix_now()],
            )?;
            stats.parents += changed as u64;
        }
        tx.execute(
            "UPDATE services SET last_relation_pull_at = ?1 WHERE id = ?2",
            params![unix_now(), service_id],
        )?;
        tx.commit()?;
        Ok(stats)
    }

    /// Apply an incremental relation delta to this service's raw staging mirror
    /// (`service_relation_edges`), then recompute the collapsed `tag_siblings` /
    /// `tag_parents` rows **only for the `(kind, from)` keys the batch touched**.
    /// The end state per key is identical to what [`Db::merge_pulled_relations`]
    /// would produce over the whole graph (ADR 0005 §4).
    ///
    /// `full_reset` clears this service's staging first — set it when pulling
    /// `since = 0` (first pull, or a repo-reset re-sync). `cursor` is the repo's
    /// new high-watermark; it is stored on the service and `last_relation_pull_at`
    /// is stamped. Edges must arrive in `seq` order (the repo guarantees this).
    ///
    /// Two client-side guards mirror the full-replace path: per `from`, the
    /// surviving sibling winner is the lexicographically-smallest current `to`
    /// (smallest `author` as tiebreak); self-edges (`from == to`) are skipped.
    ///
    /// # Errors
    /// Returns an error if a statement fails or a staged tag is unparseable.
    pub fn merge_relation_delta(
        &self,
        service_id: i64,
        full_reset: bool,
        cursor: u64,
        edges: &[DeltaEdgeInput],
    ) -> Result<RelationMergeStats> {
        let tx = self.conn.unchecked_transaction()?;
        if full_reset {
            tx.execute(
                "DELETE FROM service_relation_edges WHERE service_id = ?1",
                params![service_id],
            )?;
            tx.execute(
                "DELETE FROM tag_siblings WHERE service_id = ?1",
                params![service_id],
            )?;
            tx.execute(
                "DELETE FROM tag_parents WHERE service_id = ?1",
                params![service_id],
            )?;
        }

        // 1. Apply each edge to staging; collect the keys it touched.
        let mut touched_sibling_from: HashSet<String> = HashSet::new();
        let mut touched_parent: HashSet<(String, String)> = HashSet::new();
        for e in edges {
            let from_s = e.from.to_string();
            let to_s = e.to.to_string();
            let status = if e.deleted { "deleted" } else { "current" };
            tx.execute(
                "INSERT INTO service_relation_edges
                     (service_id, kind, from_tag, to_tag, author, status, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(service_id, kind, from_tag, to_tag, author) DO UPDATE SET
                     status = excluded.status, seq = excluded.seq",
                params![
                    service_id,
                    e.kind.as_str(),
                    from_s,
                    to_s,
                    e.author,
                    status,
                    i64::try_from(e.seq).unwrap_or(i64::MAX)
                ],
            )?;
            match e.kind {
                EdgeKind::Sibling => {
                    touched_sibling_from.insert(from_s);
                }
                EdgeKind::Parent => {
                    touched_parent.insert((from_s, to_s));
                }
            }
        }

        // 2. Recompute the sibling winner for each touched `from`.
        for from_s in &touched_sibling_from {
            let from_tag = Tag::parse(from_s).map_err(|e| Error::Invalid(e.to_string()))?;
            let winner: Option<(String, String)> = tx
                .query_row(
                    "SELECT to_tag, author FROM service_relation_edges
                     WHERE service_id = ?1 AND kind = 'sibling' AND from_tag = ?2
                       AND status = 'current' AND to_tag <> from_tag
                     ORDER BY to_tag, author LIMIT 1",
                    params![service_id, from_s],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match winner {
                Some((to_s, author)) => {
                    let bad_id = self.intern_tag(&from_tag)?;
                    let ideal_id = self.intern_tag(
                        &Tag::parse(&to_s).map_err(|e| Error::Invalid(e.to_string()))?,
                    )?;
                    tx.execute(
                        "INSERT INTO tag_siblings
                             (bad_tag_id, ideal_tag_id, service_id, status, author, signature, created_at)
                         VALUES (?1, ?2, ?3, 'current', ?4, NULL, ?5)
                         ON CONFLICT(bad_tag_id, service_id) DO UPDATE SET
                             ideal_tag_id = excluded.ideal_tag_id,
                             author = excluded.author",
                        params![bad_id, ideal_id, service_id, author, unix_now()],
                    )?;
                }
                None => {
                    if let Some(bad_id) = self.tag_id(&from_tag)? {
                        tx.execute(
                            "DELETE FROM tag_siblings WHERE bad_tag_id = ?1 AND service_id = ?2",
                            params![bad_id, service_id],
                        )?;
                    }
                }
            }
        }

        // 3. Recompute each touched parent edge (parents do not collapse).
        for (child_s, parent_s) in &touched_parent {
            if child_s == parent_s {
                continue; // self-edge guard
            }
            let child = Tag::parse(child_s).map_err(|e| Error::Invalid(e.to_string()))?;
            let parent = Tag::parse(parent_s).map_err(|e| Error::Invalid(e.to_string()))?;
            let author: Option<String> = tx.query_row(
                "SELECT MIN(author) FROM service_relation_edges
                 WHERE service_id = ?1 AND kind = 'parent'
                   AND from_tag = ?2 AND to_tag = ?3 AND status = 'current'",
                params![service_id, child_s, parent_s],
                |r| r.get::<_, Option<String>>(0),
            )?;
            match author {
                Some(author) => {
                    let child_id = self.intern_tag(&child)?;
                    let parent_id = self.intern_tag(&parent)?;
                    tx.execute(
                        "INSERT INTO tag_parents
                             (child_tag_id, parent_tag_id, service_id, status, author, signature, created_at)
                         VALUES (?1, ?2, ?3, 'current', ?4, NULL, ?5)
                         ON CONFLICT(child_tag_id, parent_tag_id, service_id) DO UPDATE SET
                             author = excluded.author",
                        params![child_id, parent_id, service_id, author, unix_now()],
                    )?;
                }
                None => {
                    if let (Some(c), Some(p)) = (self.tag_id(&child)?, self.tag_id(&parent)?) {
                        tx.execute(
                            "DELETE FROM tag_parents
                             WHERE child_tag_id = ?1 AND parent_tag_id = ?2 AND service_id = ?3",
                            params![c, p, service_id],
                        )?;
                    }
                }
            }
        }

        // 4. Advance the cursor and stamp the pull time; report post-merge counts.
        tx.execute(
            "UPDATE services SET relation_cursor = ?1, last_relation_pull_at = ?2 WHERE id = ?3",
            params![
                i64::try_from(cursor).unwrap_or(i64::MAX),
                unix_now(),
                service_id
            ],
        )?;
        let stats = RelationMergeStats {
            siblings: tx.query_row(
                "SELECT COUNT(*) FROM tag_siblings WHERE service_id = ?1",
                params![service_id],
                |r| r.get::<_, i64>(0),
            )? as u64,
            parents: tx.query_row(
                "SELECT COUNT(*) FROM tag_parents WHERE service_id = ?1",
                params![service_id],
                |r| r.get::<_, i64>(0),
            )? as u64,
        };
        tx.commit()?;
        tracing::debug!(target: "db", service_id, full_reset, cursor, siblings = stats.siblings, parents = stats.parents, "merged relation delta");
        Ok(stats)
    }

    /// Delete a service and every row scoped to it (mappings, siblings, parents),
    /// honoring the "drop a service purges its tags" safety promise (README §7).
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub fn drop_service(&self, service_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM mappings WHERE service_id = ?1",
            params![service_id],
        )?;
        self.conn.execute(
            "DELETE FROM tag_siblings WHERE service_id = ?1",
            params![service_id],
        )?;
        self.conn.execute(
            "DELETE FROM tag_parents WHERE service_id = ?1",
            params![service_id],
        )?;
        self.conn.execute(
            "DELETE FROM service_domain_pull_state WHERE service_id = ?1",
            params![service_id],
        )?;
        self.conn
            .execute("DELETE FROM services WHERE id = ?1", params![service_id])?;
        Ok(())
    }

    /// Attach `tag_id` to `file_id` within `service_id`. Idempotent: re-adding an
    /// existing mapping is a no-op. Phase 1 writes `status = 'current'`.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn add_mapping(&self, file_id: i64, tag_id: i64, service_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, ?3, 'current', ?4)
             ON CONFLICT(file_id, tag_id, service_id) DO NOTHING",
            params![file_id, tag_id, service_id, unix_now()],
        )?;
        Ok(())
    }

    /// Remove the mapping of `tag_id` to `file_id` within `service_id`. A no-op if
    /// it does not exist. (Local removal deletes the row; networked services will
    /// track deletions via `status` in a later spec.)
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn remove_mapping(&self, file_id: i64, tag_id: i64, service_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM mappings WHERE file_id = ?1 AND tag_id = ?2 AND service_id = ?3",
            params![file_id, tag_id, service_id],
        )?;
        Ok(())
    }

    /// All tags mapped to `file_id` (across services, any status), ordered by
    /// namespace then subtag. This is the literal `--raw` view: stored rows, no
    /// relation application and no status/service filtering by design. Use
    /// [`Db::display_tags_of`] for the computed effective set.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn tags_of(&self, file_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.namespace, t.subtag
             FROM mappings m
             JOIN tags t ON t.id = m.tag_id
             WHERE m.file_id = ?1
             ORDER BY t.namespace, t.subtag",
        )?;
        let rows = stmt
            .query_map(params![file_id], row_to_tag)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Alias `bad_tag_id` to `ideal_tag_id` within `service_id`. There is one
    /// ideal per bad tag, so re-aliasing replaces the prior ideal.
    ///
    /// # Errors
    /// Returns [`Error::SelfRelation`] if `bad_tag_id == ideal_tag_id`, or an
    /// error if the statement fails.
    pub fn add_sibling(&self, bad_tag_id: i64, ideal_tag_id: i64, service_id: i64) -> Result<()> {
        if bad_tag_id == ideal_tag_id {
            return Err(Error::SelfRelation);
        }
        self.conn.execute(
            "INSERT INTO tag_siblings (bad_tag_id, ideal_tag_id, service_id, status, created_at)
             VALUES (?1, ?2, ?3, 'current', ?4)
             ON CONFLICT(bad_tag_id, service_id)
             DO UPDATE SET ideal_tag_id = excluded.ideal_tag_id,
                           status = 'current',
                           created_at = excluded.created_at",
            params![bad_tag_id, ideal_tag_id, service_id, unix_now()],
        )?;
        Ok(())
    }

    /// Remove the sibling alias for `bad_tag_id` within `service_id`. No-op if
    /// absent.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn remove_sibling(&self, bad_tag_id: i64, service_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tag_siblings WHERE bad_tag_id = ?1 AND service_id = ?2",
            params![bad_tag_id, service_id],
        )?;
        Ok(())
    }

    /// Imply `parent_tag_id` from `child_tag_id` within `service_id`. Idempotent.
    ///
    /// # Errors
    /// Returns [`Error::SelfRelation`] if `child_tag_id == parent_tag_id`, or an
    /// error if the statement fails.
    pub fn add_parent(&self, child_tag_id: i64, parent_tag_id: i64, service_id: i64) -> Result<()> {
        if child_tag_id == parent_tag_id {
            return Err(Error::SelfRelation);
        }
        self.conn.execute(
            "INSERT INTO tag_parents (child_tag_id, parent_tag_id, service_id, status, created_at)
             VALUES (?1, ?2, ?3, 'current', ?4)
             ON CONFLICT(child_tag_id, parent_tag_id, service_id) DO NOTHING",
            params![child_tag_id, parent_tag_id, service_id, unix_now()],
        )?;
        Ok(())
    }

    /// Remove the implication `child_tag_id -> parent_tag_id` within
    /// `service_id`. No-op if absent.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn remove_parent(
        &self,
        child_tag_id: i64,
        parent_tag_id: i64,
        service_id: i64,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tag_parents
             WHERE child_tag_id = ?1 AND parent_tag_id = ?2 AND service_id = ?3",
            params![child_tag_id, parent_tag_id, service_id],
        )?;
        Ok(())
    }

    /// Load the current sibling edges for `service_id` as `bad -> ideal`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn load_sibling_edges(&self, service_id: i64) -> Result<SiblingEdges> {
        let mut stmt = self.conn.prepare(
            "SELECT bad_tag_id, ideal_tag_id FROM tag_siblings
             WHERE service_id = ?1 AND status = 'current'",
        )?;
        let mut edges = SiblingEdges::new();
        let rows = stmt.query_map(params![service_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (bad, ideal) = row?;
            edges.insert(bad, ideal);
        }
        Ok(edges)
    }

    /// Current collapsed sibling edges for `service_id` with their author:
    /// `(bad_tag_id, ideal_tag_id, author)`. Like `load_sibling_edges` but carries
    /// the author so cross-service merging can weight by trust.
    fn load_sibling_candidates(&self, service_id: i64) -> Result<Vec<(i64, i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT bad_tag_id, ideal_tag_id, author FROM tag_siblings
             WHERE service_id = ?1 AND status = 'current'",
        )?;
        let rows = stmt.query_map(params![service_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Merge sibling edges across `services`. For each `bad` tag the winning
    /// `ideal` is the candidate with the highest service priority. Ties keep the
    /// first-iterated candidate; since `services` arrives ordered
    /// `priority DESC, id ASC`, this is priority-first, first-wins.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn merged_sibling_edges(&self, services: &[i64]) -> Result<SiblingEdges> {
        let started = Instant::now();
        let mut merged = SiblingEdges::new();
        // Per `bad` tag: the best priority seen so far.
        let mut best: HashMap<i64, i64> = HashMap::new();
        for &service_id in services {
            let priority = self.service_priority(service_id)?;
            for (bad, ideal, _author) in self.load_sibling_candidates(service_id)? {
                match best.get(&bad) {
                    Some(&cur) if priority <= cur => {}
                    _ => {
                        best.insert(bad, priority);
                        merged.insert(bad, ideal);
                    }
                }
            }
        }
        tracing::debug!(target: "db", edges = merged.len() as u64, services = services.len(), elapsed_ms = started.elapsed().as_millis() as u64, "merged sibling edges");
        Ok(merged)
    }

    /// Load the current parent edges for `service_id` as `child -> [parents]`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn load_parent_edges(&self, service_id: i64) -> Result<ParentEdges> {
        let mut stmt = self.conn.prepare(
            "SELECT child_tag_id, parent_tag_id FROM tag_parents
             WHERE service_id = ?1 AND status = 'current'",
        )?;
        let mut edges = ParentEdges::new();
        let rows = stmt.query_map(params![service_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (child, parent) = row?;
            edges.entry(child).or_default().push(parent);
        }
        Ok(edges)
    }

    /// Merge parent edges across `services` by union (parents are additive).
    /// Duplicate `(child, parent)` pairs are collapsed.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn merged_parent_edges(&self, services: &[i64]) -> Result<ParentEdges> {
        let started = Instant::now();
        let mut merged = ParentEdges::new();
        for &service_id in services {
            for (child, parents) in self.load_parent_edges(service_id)? {
                let slot = merged.entry(child).or_default();
                for p in parents {
                    if !slot.contains(&p) {
                        slot.push(p);
                    }
                }
            }
        }
        tracing::debug!(target: "db", edges = merged.len() as u64, services = services.len(), elapsed_ms = started.elapsed().as_millis() as u64, "merged parent edges");
        Ok(merged)
    }

    /// The merged relation graph for `services`, served from an in-process
    /// cache. Version + triggers live in SQLite, so a committed write on the
    /// writer connection invalidates caches held by read-only connections.
    ///
    /// A cached entry is valid — and returned as-is — only while
    /// [`Db::relation_graph_version`] still matches its build-time stamp.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn relation_graph(&self, services: &[i64]) -> Result<Arc<RelationGraph>> {
        Ok(self.ensure_relation_cache(services)?.0)
    }

    /// The completion overlay for `services`, built and cached alongside the
    /// relation graph (same version stamp, same eviction). See
    /// [`RelationCompletion`].
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn relation_completion(&self, services: &[i64]) -> Result<Arc<RelationCompletion>> {
        Ok(self.ensure_relation_cache(services)?.1)
    }

    /// Build or serve from cache the relation graph and completion overlay for
    /// `services`. Both are stored together in [`RelationCache`] and keyed by
    /// `(services, relation_graph_version)`. A stale version stamp evicts the
    /// old entry and triggers a rebuild of both.
    fn ensure_relation_cache(
        &self,
        services: &[i64],
    ) -> Result<(Arc<RelationGraph>, Arc<RelationCompletion>)> {
        let started = Instant::now();
        // IMPORTANT: read the version AFTER acquiring the lock so the stamp
        // captured here is consistent with the subsequent build. Reading before
        // the lock creates a TOCTOU window where a writer can bump the version
        // between the read and the lock acquisition: another thread then builds
        // and stores the correct entry, and this thread (holding an old stamp)
        // evicts it and stores a stale-stamped rebuild, causing spurious cache
        // misses on every subsequent request until the next write.
        let mut guard = self
            .relation_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let relation_version = self.relation_graph_version()?;
        if let Some(idx) = guard.entries.iter().position(|c| c.services == services) {
            let cache = &guard.entries[idx];
            if cache.relation_version == relation_version {
                let graph = cache.graph.clone();
                let completion = cache.completion.clone();
                drop(guard);
                // debug, not info: cache hits happen on every expanded search
                // once warm, and a per-query line at the default level would
                // out-shout the search summary itself.
                tracing::debug!(
                    target: "db",
                    cache_hit = true,
                    services = services.len(),
                    relation_version,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "relation graph served from cache",
                );
                return Ok((graph, completion));
            }
            // Stale — drop it now so a fresh entry can be pushed below without
            // growing the store past its intended contents.
            guard.entries.remove(idx);
        }
        let graph = Arc::new(RelationGraph::new(
            self.merged_sibling_edges(services)?,
            self.merged_parent_edges(services)?,
        ));
        let completion = Arc::new(self.build_relation_completion(&graph)?);
        if guard.entries.len() >= RELATION_CACHE_CAP {
            tracing::debug!(target: "db", cap = RELATION_CACHE_CAP, "relation cache full; evicting oldest entry");
            guard.entries.remove(0);
        }
        guard.entries.push(RelationCache {
            services: services.to_vec(),
            relation_version,
            graph: graph.clone(),
            completion: completion.clone(),
        });
        drop(guard);
        tracing::info!(
            target: "db",
            cache_hit = false,
            services = services.len(),
            relation_version,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "relation graph built",
        );
        Ok((graph, completion))
    }

    /// Build the completion overlay from an already-merged graph: one bounded
    /// count query + one bounded name query over the relation-involved ids.
    fn build_relation_completion(&self, graph: &RelationGraph) -> Result<RelationCompletion> {
        let preimage = graph.sibling_preimage_map();
        if preimage.is_empty() {
            return Ok(RelationCompletion::default());
        }
        // Every relation-involved id: canonical ideals + their aliases.
        let mut ids: Vec<i64> = Vec::new();
        for (canon, bads) in preimage {
            ids.push(*canon);
            ids.extend(bads.iter().copied());
        }
        let in_list = int_list(ids.iter().copied());

        // Raw current-mapping counts for all involved ids, in one IN query.
        let mut raw: HashMap<i64, i64> = HashMap::new();
        {
            let sql = format!(
                "SELECT tag_id, current_count FROM tag_completion_counts WHERE tag_id IN ({in_list})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (id, c) = row?;
                raw.insert(id, c);
            }
        }
        // Names for all involved ids, in one IN query.
        let mut names: HashMap<i64, Tag> = HashMap::new();
        {
            let sql = format!("SELECT id, namespace, subtag FROM tags WHERE id IN ({in_list})");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Tag {
                        namespace: r.get(1)?,
                        subtag: r.get(2)?,
                    },
                ))
            })?;
            for row in rows {
                let (id, tag) = row?;
                names.insert(id, tag);
            }
        }

        let mut alias_to_canonical: HashMap<i64, i64> = HashMap::new();
        let mut merged: HashMap<i64, i64> = HashMap::new();
        let mut alias_names: Vec<(i64, Tag)> = Vec::new();
        let mut ideal_names: Vec<(i64, Tag)> = Vec::new();
        for (canon, bads) in preimage {
            let mut sum = raw.get(canon).copied().unwrap_or(0);
            for bad in bads {
                alias_to_canonical.insert(*bad, *canon);
                if let Some(name) = names.get(bad) {
                    alias_names.push((*canon, name.clone()));
                }
                sum += raw.get(bad).copied().unwrap_or(0);
            }
            if sum > 0 {
                merged.insert(*canon, sum);
                if let Some(name) = names.get(canon) {
                    ideal_names.push((*canon, name.clone()));
                }
            }
        }
        Ok(RelationCompletion {
            alias_to_canonical,
            merged,
            alias_names,
            ideal_names,
        })
    }

    /// Build and cache the merged relation graph, then warm tag-completion pages,
    /// for the default merged scope before the UI's first query burst. Called once
    /// at daemon startup on a background connection.
    ///
    /// The graph is built **first** (before the much longer completion walk) so
    /// the shared relation cache is populated as early as possible: the
    /// interactive tag lane's detail/completion handlers must never be the
    /// connection that triggers the ~34s graph build, which would hold the single
    /// tag-lane mutex and stall every other tag read for tens of seconds (#126).
    /// The ~600MB graph read runs once off the request path (#70); the three
    /// completion scans then fault in the index/table/count pages (#76).
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn warm_caches(&self, scope: ReadScope) -> Result<()> {
        let started = Instant::now();
        self.warm_relation_graph(scope)?;
        self.warm_tag_completion()?;
        tracing::info!(
            target: "db",
            elapsed_ms = started.elapsed().as_millis() as u64,
            "caches warmed",
        );
        Ok(())
    }

    /// Build and cache the merged relation graph (and its completion overlay) for
    /// `scope` on this connection. Idempotent: a warm cache returns in
    /// microseconds. Exposed so the daemon can force the build onto a read-pool
    /// connection, keeping the ~34s cold build off the interactive tag lane (#126).
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn warm_relation_graph(&self, scope: ReadScope) -> Result<()> {
        let services = self.included_services(scope)?;
        // ensure_relation_cache builds graph + completion together; calling
        // relation_graph here warms both in one pass.
        self.relation_graph(&services)?;
        Ok(())
    }

    /// Touch the primary pages used by bare-prefix tag completion and namespace
    /// listing so the first new prefix after daemon startup does not pay their
    /// cold-page IO on the request path. This is intentionally a sequential
    /// index/table walk: it is best-effort startup work, not an interactive query.
    ///
    /// # Errors
    /// Returns an error if a warmup query fails.
    pub fn warm_tag_completion(&self) -> Result<()> {
        // Completion is the first interactive read most users issue. Warm its
        // index/table/count pages before the much larger relation graph so a
        // newly typed prefix does not spend seconds faulting a fresh range in
        // from disk (#76). Keep these as three sequential b-tree walks: joining
        // every index entry back to two tables caused effectively random IO and
        // took >5 minutes on the 95k-file acceptance library.
        let t0 = Instant::now();
        let _: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(length(subtag)), 0)
             FROM tags INDEXED BY idx_tags_subtag_nocase",
            [],
            |row| row.get(0),
        )?;
        tracing::debug!(target: "db", elapsed_ms = t0.elapsed().as_millis() as u64, "warmed subtag index pages");

        let t1 = Instant::now();
        let _: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(length(namespace)), 0)
             FROM tags NOT INDEXED",
            [],
            |row| row.get(0),
        )?;
        tracing::debug!(target: "db", elapsed_ms = t1.elapsed().as_millis() as u64, "warmed tag namespace pages");

        let t2 = Instant::now();
        let _: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(current_count), 0)
             FROM tag_completion_counts NOT INDEXED",
            [],
            |row| row.get(0),
        )?;
        tracing::debug!(target: "db", elapsed_ms = t2.elapsed().as_millis() as u64, "warmed completion counts pages");

        // Namespace completion has its own compact trigger-maintained table.
        let t3 = Instant::now();
        self.complete_namespaces("", 200)?;
        tracing::debug!(target: "db", elapsed_ms = t3.elapsed().as_millis() as u64, "warmed namespace completion pages");
        Ok(())
    }

    /// The effective (display) tag set for `file_id` under `scope`, each tag
    /// carrying presence. Raw mappings across the included services are
    /// canonicalized through merged siblings and expanded through merged parents.
    ///
    /// Use [`Db::tags_of`] for the raw mappings.
    ///
    /// # Provenance
    /// A tag's presence reflects the raw mapping(s) it traces to: a tag mapped on
    /// the local service is `Local`, on a pulled service `Pulled`, on both
    /// `Both`. Tags that exist only via parent expansion inherit the union of all
    /// raw presences on the file.
    ///
    /// # Errors
    /// Returns an error if any query fails.
    pub fn display_tags_of(&self, file_id: i64, scope: ReadScope) -> Result<Vec<TagWithPresence>> {
        let services = self.included_services(scope)?;
        let local_ids: HashSet<i64> = self.local_service_ids()?.into_iter().collect();
        let svc_in = int_list(services.iter().copied());

        let blocks = self.block_matcher()?;
        let reject = self.reject_matcher()?;
        let sql = format!(
            "SELECT m.tag_id, m.service_id
               FROM mappings m
              WHERE m.file_id = ?1 AND m.status = 'current'
                AND m.service_id IN ({svc_in})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let raw_rows: Vec<(i64, i64)> = stmt
            .query_map(params![file_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(tag_id, service_id)| {
                tag_visible(&blocks, Some(&reject), *service_id, file_id, *tag_id)
            })
            .collect();

        // Per raw tag id: did a local and/or pulled service assert it?
        let mut raw_presence: HashMap<i64, (bool, bool)> = HashMap::new();
        for (tag_id, service_id) in &raw_rows {
            let e = raw_presence.entry(*tag_id).or_insert((false, false));
            if local_ids.contains(service_id) {
                e.0 = true;
            } else {
                e.1 = true;
            }
        }
        let raw: Vec<i64> = raw_presence.keys().copied().collect();

        let graph = self.relation_graph(&services)?;
        let effective = effective_tags(&raw, graph.siblings(), graph.parents());

        // Seed effective-tag presence from raw tags by canonical form.
        let mut eff_presence: HashMap<i64, (bool, bool)> = HashMap::new();
        for (&tag_id, &(l, p)) in &raw_presence {
            let canon = canonicalize(tag_id, graph.siblings());
            let e = eff_presence.entry(canon).or_insert((false, false));
            e.0 |= l;
            e.1 |= p;
        }
        // Derived tags (parents not directly seeded) inherit the union of all
        // raw presences on this file — they exist because of those mappings.
        let any_local = raw_presence.values().any(|&(l, _)| l);
        let any_pulled = raw_presence.values().any(|&(_, p)| p);

        let mut by_id = self
            .conn
            .prepare("SELECT namespace, subtag FROM tags WHERE id = ?1")?;
        let mut out = Vec::with_capacity(effective.len());
        for id in effective {
            let (l, p) = eff_presence
                .get(&id)
                .copied()
                .unwrap_or((any_local, any_pulled));
            let presence = match (l, p) {
                (true, true) => TagPresence::Both,
                (true, false) => TagPresence::Local,
                (false, true) => TagPresence::Pulled,
                // An effective tag always traces to >=1 raw mapping, so at least
                // one presence bit is set. (false, false) is impossible here.
                (false, false) => {
                    unreachable!("effective tag {id} has no raw presence (file {file_id})")
                }
            };
            let tag = by_id.query_row(params![id], row_to_tag)?;
            out.push(TagWithPresence { tag, presence });
        }
        out.sort_unstable_by(|a, b| {
            a.tag
                .namespace
                .cmp(&b.tag.namespace)
                .then_with(|| a.tag.subtag.cmp(&b.tag.subtag))
        });
        Ok(out)
    }

    /// Returns [`TagDetail`] for each effective tag visible to `file_id` under
    /// `scope`. Each entry carries the tag's presence and `services` — the sorted,
    /// deduplicated display names of every shared service that contributes the
    /// tag for this file. `services` is empty when `presence == Local`.
    ///
    /// # Errors
    /// Returns an error if any query fails.
    pub fn display_tags_detailed(&self, file_id: i64, scope: ReadScope) -> Result<Vec<TagDetail>> {
        let services = self.included_services(scope)?;
        let local_ids: HashSet<i64> = self.local_service_ids()?.into_iter().collect();
        let svc_in = int_list(services.iter().copied());

        let blocks = self.block_matcher()?;
        let reject = self.reject_matcher()?;

        // Include origins in the main batched SELECT so the lookup is already
        // status- and scope-filtered (matching the `status = 'current'` and
        // `service_id IN (…)` predicates). This also fixes canonicalization:
        // mappings store raw interned tag ids, not canonical ids; resolving
        // origin here lets us fold raw→canon below alongside the existing
        // presence aggregation. NOT a hot-path predicate — merges/buckets
        // never call display_tags_detailed.
        let sql = format!(
            "SELECT m.tag_id, m.service_id, o.name
               FROM mappings m
               LEFT JOIN origins o ON o.id = m.origin_id
              WHERE m.file_id = ?1 AND m.status = 'current'
                AND m.service_id IN ({svc_in})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let raw_rows: Vec<(i64, i64, Option<String>)> = stmt
            .query_map(params![file_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(tag_id, service_id, _)| {
                tag_visible(&blocks, Some(&reject), *service_id, file_id, *tag_id)
            })
            .collect();

        let mut raw_presence: HashMap<i64, (bool, bool)> = HashMap::new();
        // raw tag_id -> set of pulled service_ids supplying it
        let mut pull_svc_by_tag: HashMap<i64, BTreeSet<i64>> = HashMap::new();
        // raw tag_id -> first non-None origin name from the scoped/status-filtered rows
        // (first wins; iteration order is stable within the SELECT's output order)
        let mut origin_by_raw: HashMap<i64, String> = HashMap::new();
        for (tag_id, service_id, origin_name) in &raw_rows {
            let e = raw_presence.entry(*tag_id).or_insert((false, false));
            if local_ids.contains(service_id) {
                e.0 = true;
            } else {
                e.1 = true;
                pull_svc_by_tag
                    .entry(*tag_id)
                    .or_default()
                    .insert(*service_id);
            }
            if let Some(name) = origin_name {
                origin_by_raw.entry(*tag_id).or_insert_with(|| name.clone());
            }
        }
        let raw: Vec<i64> = raw_presence.keys().copied().collect();

        let graph = self.relation_graph(&services)?;
        let effective = effective_tags(&raw, graph.siblings(), graph.parents());

        let mut eff_presence: HashMap<i64, (bool, bool)> = HashMap::new();
        // canonical effective tag_id -> set of pulled service_ids (union over siblings)
        let mut pull_svc_by_canon: HashMap<i64, BTreeSet<i64>> = HashMap::new();
        // canonical effective tag_id -> first non-None origin (raw→canon fold;
        // first raw spelling wins, mirroring services' union-over-siblings logic)
        let mut origin_by_canon: HashMap<i64, String> = HashMap::new();
        for (&tag_id, &(l, p)) in &raw_presence {
            let canon = canonicalize(tag_id, graph.siblings());
            let e = eff_presence.entry(canon).or_insert((false, false));
            e.0 |= l;
            e.1 |= p;
            if let Some(name) = origin_by_raw.get(&tag_id) {
                origin_by_canon.entry(canon).or_insert_with(|| name.clone());
            }
        }
        for (tag_id, svc_ids) in &pull_svc_by_tag {
            let canon = canonicalize(*tag_id, graph.siblings());
            pull_svc_by_canon.entry(canon).or_default().extend(svc_ids);
        }
        let any_local = raw_presence.values().any(|&(l, _)| l);
        let any_pulled = raw_presence.values().any(|&(_, p)| p);

        // service_id -> display name (shared services only)
        let svc_names = self.service_name_map()?;

        let mut by_id = self
            .conn
            .prepare("SELECT namespace, subtag FROM tags WHERE id = ?1")?;
        let mut out = Vec::with_capacity(effective.len());
        for id in effective {
            let (l, p) = eff_presence
                .get(&id)
                .copied()
                .unwrap_or((any_local, any_pulled));
            let presence = match (l, p) {
                (true, true) => TagPresence::Both,
                (true, false) => TagPresence::Local,
                (false, true) => TagPresence::Pulled,
                (false, false) => {
                    unreachable!("effective tag {id} has no raw presence (file {file_id})")
                }
            };
            let tag = by_id.query_row(params![id], row_to_tag)?;
            // Collect pulled service names in sorted order (BTreeSet → vec).
            let services = pull_svc_by_canon
                .get(&id)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|sid| svc_names.get(sid))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let origin = origin_by_canon.get(&id).cloned();
            out.push(TagDetail {
                tag,
                presence,
                services,
                relations: graph.has_relations(id),
                origin,
            });
        }
        out.sort_unstable_by(|a, b| {
            a.tag
                .namespace
                .cmp(&b.tag.namespace)
                .then_with(|| a.tag.subtag.cmp(&b.tag.subtag))
        });
        Ok(out)
    }

    /// Relations of `tag` for the detail popover: the canonical form, whether the
    /// file carries it via an alias raw mapping, and capped alias/parent/child
    /// sections.
    ///
    /// **Counts** differ by section. **Alias** rows show their *own* raw
    /// `tag_completion_counts` value — usually 0, since files are stored with the
    /// canonical form, and the UI hides a 0. The informative signal for aliases
    /// is *how many* alternate spellings exist (`aliases.total`), not the
    /// per-alias number. **Parent/child** rows show the merged display count (raw
    /// of the canonical plus the sum of raw counts for all its aliases),
    /// consistent with what tag completions show; the canonical's own top-level
    /// `count` is likewise the merged value.
    ///
    /// `cap` is applied server-side by [`RelationGraph::relations_of`]; each
    /// section's `total` is the true pre-cap count so the client can render "… N
    /// more". Items within each section are ranked count desc then namespace asc
    /// then subtag asc.
    ///
    /// Unknown tags (no interned id) return the query tag as `canonical` with
    /// empty sections and `via_alias = false` — this is not an error.
    ///
    /// `via_alias` is `true` iff `file_id` is `Some` **and** the file carries a
    /// current raw mapping whose tag id is in the sibling preimage of the
    /// canonical tag (i.e. the file was tagged with an alias, not the ideal form).
    ///
    /// # Errors
    /// Returns an error if any database query fails.
    pub fn tag_relations(
        &self,
        tag: &Tag,
        file_id: Option<i64>,
        scope: ReadScope,
        cap: usize,
    ) -> Result<TagRelations> {
        let services = self.included_services(scope)?;
        // Obtain graph and completion overlay in one cache round-trip.
        let (graph, completion) = self.ensure_relation_cache(&services)?;

        // Resolve the query tag to an interned id; return early if unknown.
        let Some(raw_id) = self.tag_id(tag)? else {
            return Ok(TagRelations {
                canonical: tag.clone(),
                count: 0,
                via_alias: false,
                aliases: RelationSection::default(),
                parents: RelationSection::default(),
                children: RelationSection::default(),
            });
        };

        // Canonicalize through siblings to get the ideal id. We need it both for
        // the canonical-name DB lookup below and for relations_of (which
        // re-canonicalizes internally, but we need the id separately here).
        let canonical_id = canonicalize(raw_id, graph.siblings());
        let canonical: Tag = self.conn.query_row(
            "SELECT namespace, subtag FROM tags WHERE id = ?1",
            params![canonical_id],
            row_to_tag,
        )?;

        // Merged display count for the canonical itself.
        let count: i64 = if let Some(mc) = completion.merged_count(canonical_id) {
            mc
        } else {
            self.conn
                .query_row(
                    "SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1",
                    params![canonical_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0)
        };

        // Compute the three sections using merged display counts.
        let sections = graph.relations_of(canonical_id, cap);
        // Aliases show their own raw count (hidden when 0); parents/children show
        // merged display counts. See `resolve_relation_section`.
        let aliases =
            self.resolve_relation_section(&sections.aliases, &graph, &completion, true)?;
        let parents =
            self.resolve_relation_section(&sections.parents, &graph, &completion, false)?;
        let children =
            self.resolve_relation_section(&sections.children, &graph, &completion, false)?;

        // `via_alias`: only meaningful when a file_id is supplied. Use a targeted
        // EXISTS probe rather than fetching all mappings for the file.
        let via_alias = if let Some(fid) = file_id {
            let preimage = graph.sibling_preimage_map();
            if let Some(bad_ids) = preimage.get(&canonical_id) {
                if bad_ids.is_empty() {
                    false
                } else {
                    let in_list = int_list(bad_ids.iter().copied());
                    let sql = format!(
                        "SELECT EXISTS(SELECT 1 FROM mappings \
                         WHERE file_id = ?1 AND status = 'current' AND tag_id IN ({in_list}))"
                    );
                    self.conn
                        .query_row(&sql, params![fid], |r| r.get::<_, bool>(0))?
                }
            } else {
                false
            }
        } else {
            false
        };

        Ok(TagRelations {
            canonical,
            count,
            via_alias,
            aliases,
            parents,
            children,
        })
    }

    /// Resolve a capped id list to a [`RelationSection`]: look up each id's tag
    /// name and count, sort count desc / namespace asc / subtag asc, and carry
    /// `capped.total` through.
    ///
    /// `alias_raw` selects the count semantics:
    /// - **`true` (aliases section):** each row shows its *own* raw
    ///   `tag_completion_counts` value — how many files carry that exact
    ///   spelling. This is almost always 0 (files are stored with the canonical
    ///   form), and the UI hides a 0 count. Using the merged count here instead
    ///   would make every alias row show the identical canonical total, which is
    ///   what the "same amount on every alias" bug looked like.
    /// - **`false` (parents / children):** each id is already canonical; the
    ///   displayed count is its merged display count (raw of the canonical plus
    ///   the sum of raw counts for all its aliases), falling back to the
    ///   canonical's own raw value when no alias merging applies, and to 0 if the
    ///   tag has not been mapped yet.
    fn resolve_relation_section(
        &self,
        capped: &naiad_core::RelationCapped,
        graph: &RelationGraph,
        completion: &RelationCompletion,
        alias_raw: bool,
    ) -> Result<RelationSection> {
        let mut tag_stmt = self
            .conn
            .prepare("SELECT namespace, subtag FROM tags WHERE id = ?1")?;
        let mut count_stmt = self
            .conn
            .prepare("SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1")?;
        let mut items: Vec<RelationTag> = Vec::with_capacity(capped.ids.len());
        for &id in &capped.ids {
            let tag: Tag = tag_stmt.query_row(params![id], row_to_tag)?;
            let count: i64 = if alias_raw {
                // Alias row: its own raw mapping count (usually 0, hidden in UI).
                count_stmt
                    .query_row(params![id], |r| r.get(0))
                    .optional()?
                    .unwrap_or(0)
            } else {
                // Parent/child row: merged display count of the canonical id.
                let canon = canonicalize(id, graph.siblings());
                match completion.merged_count(canon) {
                    Some(mc) => mc,
                    None => count_stmt
                        .query_row(params![canon], |r| r.get(0))
                        .optional()?
                        .unwrap_or(0),
                }
            };
            items.push(RelationTag { tag, count });
        }
        items.sort_unstable_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.tag.namespace.cmp(&b.tag.namespace))
                .then_with(|| a.tag.subtag.cmp(&b.tag.subtag))
        });
        Ok(RelationSection {
            items,
            total: capped.total,
        })
    }

    /// Shared-service `id -> display name` map.
    fn service_name_map(&self) -> Result<HashMap<i64, String>> {
        Ok(self
            .list_shared_services()?
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect())
    }

    /// The interned id for `tag`, if it already exists. Read-only — unlike
    /// [`Db::intern_tag`] this never creates a row, so searching for an unknown
    /// tag does not pollute the dictionary.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn tag_id(&self, tag: &Tag) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM tags WHERE namespace = ?1 AND subtag = ?2",
                params![tag.namespace, tag.subtag],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Files matching `query` under `scope`, merging tag memberships and
    /// relation edges across the included services, as [`FileListing`]s ordered
    /// by hash.
    ///
    /// - [`ReadScope::LocalOnly`] — only the local service's mappings and
    ///   relations are visible.
    /// - [`ReadScope::Merged`] — all services contribute to membership and
    ///   relation expansion (first-wins for siblings, union for parents).
    ///
    /// `expansion` selects relation inference ([`Expansion::Expanded`]) or a
    /// literal match ([`Expansion::Raw`], which evaluates against empty edges).
    ///
    /// # Errors
    /// Returns an error if any query fails.
    pub fn search(
        &self,
        query: &Query,
        scope: ReadScope,
        expansion: Expansion,
    ) -> Result<Vec<FileListing>> {
        let started = Instant::now();
        let services = self.included_services(scope)?;
        // Raw mode evaluates with empty edges: `match_set` then degenerates to the
        // singleton {id} and wildcard matching keeps non-canonical tags — i.e. a
        // purely literal match — while skipping the two merged-edge reads.
        // `Expanded` reads the merged relation graph; `Raw` evaluates the whole
        // query against empty edges. A per-term `=` predicate (MatchMode::Exact)
        // is then routed to `empty` regardless, via `pick` below — so a single
        // term can be literal while the rest of the query still expands.
        let graph: Arc<RelationGraph> = match expansion {
            Expansion::Expanded => self.relation_graph(&services)?,
            Expansion::Raw => Arc::new(RelationGraph::empty()),
        };
        let empty = RelationGraph::empty();
        let blocks = self.block_matcher()?;
        // RejectMatcher is built only for effective (Expanded) searches. Raw
        // search preserves the raw-path maxim (#7): no reject filter on raw paths.
        let reject: Option<RejectMatcher> = match expansion {
            Expansion::Expanded => Some(self.reject_matcher()?),
            Expansion::Raw => None,
        };
        let reject_ref = reject.as_ref();

        // Intersect positive predicates (Tag, Or-group). `None` means "universe".
        let mut matched: Option<HashSet<i64>> = None;
        for pred in &query.predicates {
            let set = match pred {
                Predicate::Tag(t, m) => {
                    let g = pick(*m, &graph, &empty);
                    self.files_matching(t, &services, g, &blocks, reject_ref)?
                }
                Predicate::Or(members) => {
                    let mut union = HashSet::new();
                    for (t, m) in members {
                        let g = pick(*m, &graph, &empty);
                        union.extend(self.files_matching(t, &services, g, &blocks, reject_ref)?);
                    }
                    union
                }
                Predicate::Wild(p, m) => {
                    let g = pick(*m, &graph, &empty);
                    self.wild_files_matching(p, &services, g, &blocks, reject_ref)?
                }
                Predicate::System(p) => match p {
                    SystemPredicate::Origin { name } => {
                        self.origin_files_matching(name.as_deref(), &services, &blocks, reject_ref)?
                    }
                    _ => self.system_files_matching(p)?,
                },
                Predicate::Not(..) | Predicate::NotWild(..) | Predicate::NotSystem(_) => continue,
            };
            matched = Some(match matched {
                Some(acc) => &acc & &set,
                None => set,
            });
        }

        // Seed with all files if the query was only negations (or empty).
        let mut result = match matched {
            Some(set) => set,
            None => self.all_file_ids()?,
        };

        // Subtract negations.
        for pred in &query.predicates {
            match pred {
                Predicate::Not(t, m) => {
                    let g = pick(*m, &graph, &empty);
                    for id in self.files_matching(t, &services, g, &blocks, reject_ref)? {
                        result.remove(&id);
                    }
                }
                Predicate::NotWild(p, m) => {
                    let g = pick(*m, &graph, &empty);
                    for id in self.wild_files_matching(p, &services, g, &blocks, reject_ref)? {
                        result.remove(&id);
                    }
                }
                Predicate::NotSystem(p) => {
                    let ids = match p {
                        SystemPredicate::Origin { name } => self.origin_files_matching(
                            name.as_deref(),
                            &services,
                            &blocks,
                            reject_ref,
                        )?,
                        _ => self.system_files_matching(p)?,
                    };
                    for id in ids {
                        result.remove(&id);
                    }
                }
                _ => {}
            }
        }

        let results = self.listings_for(&result)?;
        tracing::debug!(
            target: "search",
            matched = results.len() as u64,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "search executed",
        );
        Ok(results)
    }

    /// File ids whose current mappings on any of `services` include any tag in
    /// `tag`'s match set.
    fn files_matching(
        &self,
        tag: &Tag,
        services: &[i64],
        graph: &RelationGraph,
        blocks: &BlockMatcher,
        reject: Option<&RejectMatcher>,
    ) -> Result<HashSet<i64>> {
        let Some(id) = self.tag_id(tag)? else {
            return Ok(HashSet::new());
        };
        let ids = graph.match_set(id);
        self.file_ids_with_any_tag(&ids, services, blocks, reject)
    }

    /// File ids whose current mappings on any of `services` include any tag in
    /// `tag_ids`, with suppressed rows filtered out via `blocks`. Empty `tag_ids`
    /// (or empty `services`) yields an empty set (and avoids `tag_id IN ()` /
    /// `service_id IN ()`).
    ///
    /// `reject` is `Some` for effective (expanded) searches and `None` for raw
    /// searches, preserving the raw-path maxim (#7): no `RejectMatcher` is built
    /// or applied on raw paths.
    fn file_ids_with_any_tag(
        &self,
        tag_ids: &BTreeSet<i64>,
        services: &[i64],
        blocks: &BlockMatcher,
        reject: Option<&RejectMatcher>,
    ) -> Result<HashSet<i64>> {
        if tag_ids.is_empty() || services.is_empty() {
            return Ok(HashSet::new());
        }
        let tag_in = int_list(tag_ids.iter().copied());
        let svc_in = int_list(services.iter().copied());
        let sql = format!(
            "SELECT m.file_id, m.tag_id, m.service_id
               FROM mappings m
              WHERE m.service_id IN ({svc_in}) AND m.status = 'current'
                AND m.tag_id IN ({tag_in})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut out = HashSet::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let file_id: i64 = r.get(0)?;
            let tag_id: i64 = r.get(1)?;
            let service_id: i64 = r.get(2)?;
            if tag_visible(blocks, reject, service_id, file_id, tag_id) {
                out.insert(file_id);
            }
        }
        Ok(out)
    }

    /// File ids whose current mappings on any of `services` carry generation origin
    /// `name` (case-insensitive), or — when `name` is `None` (the reserved `manual`
    /// value) — carry no origin (`origin_id IS NULL`). Visibility (`blocks`/`reject`)
    /// and status/scope filtering mirror `file_ids_with_any_tag`. `origin_id` stays
    /// unindexed (ADR 0026 / #151): this is one linear scan of `mappings`, paid only
    /// when the predicate is used.
    fn origin_files_matching(
        &self,
        name: Option<&str>,
        services: &[i64],
        blocks: &BlockMatcher,
        reject: Option<&RejectMatcher>,
    ) -> Result<HashSet<i64>> {
        // Empty scope: avoid `service_id IN ()`.
        if services.is_empty() {
            return Ok(HashSet::new());
        }
        let svc_in = int_list(services.iter().copied());
        let mut out = HashSet::new();
        match name {
            Some(origin_name) => {
                // Resolve name → id against the tiny `origins` table.
                // COLLATE NOCASE is ASCII case-fold, matching the parser's lowercasing.
                let origin_id: Option<i64> = self
                    .conn
                    .query_row(
                        "SELECT id FROM origins WHERE name = ?1 COLLATE NOCASE",
                        params![origin_name],
                        |r| r.get(0),
                    )
                    .optional()?;
                // Unknown name → empty set immediately (no mappings scan, no error).
                let Some(oid) = origin_id else {
                    return Ok(HashSet::new());
                };
                let sql = format!(
                    "SELECT m.file_id, m.tag_id, m.service_id
                       FROM mappings m
                      WHERE m.origin_id = {oid} AND m.service_id IN ({svc_in}) AND m.status = 'current'"
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let file_id: i64 = r.get(0)?;
                    let tag_id: i64 = r.get(1)?;
                    let service_id: i64 = r.get(2)?;
                    if tag_visible(blocks, reject, service_id, file_id, tag_id) {
                        out.insert(file_id);
                    }
                }
            }
            None => {
                // Manual: origin_id IS NULL.
                let sql = format!(
                    "SELECT m.file_id, m.tag_id, m.service_id
                       FROM mappings m
                      WHERE m.origin_id IS NULL AND m.service_id IN ({svc_in}) AND m.status = 'current'"
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let file_id: i64 = r.get(0)?;
                    let tag_id: i64 = r.get(1)?;
                    let service_id: i64 = r.get(2)?;
                    if tag_visible(blocks, reject, service_id, file_id, tag_id) {
                        out.insert(file_id);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Build a [`BlockMatcher`] from the current block rules. Exact-tag and
    /// pattern rules are resolved to tag ids against the live dictionary.
    /// Unknown exact tags (not yet interned) simply contribute nothing.
    fn block_matcher(&self) -> Result<BlockMatcher> {
        let local_service_ids: HashSet<i64> = self.local_service_ids()?.into_iter().collect();
        let mut suppressed_tag_ids = HashSet::new();
        for rule in self.list_block_rules()? {
            match rule.kind {
                BlockKind::Tag => {
                    if let Ok(tag) = Tag::parse(&rule.target) {
                        if let Some(id) = self.tag_id(&tag)? {
                            suppressed_tag_ids.insert(id);
                        }
                    }
                }
                BlockKind::TagPattern => {
                    if let Ok(pat) = TagPattern::parse(&rule.target) {
                        for id in self.tag_ids_matching_pattern(&pat)? {
                            suppressed_tag_ids.insert(id);
                        }
                    }
                }
                BlockKind::Author => {} // author blocks no longer applied
            }
        }
        Ok(BlockMatcher {
            suppressed_tag_ids,
            local_service_ids,
        })
    }

    /// Build a [`RejectMatcher`] from all current mapping rejections. Rebuilt on
    /// each read so a reject/undo takes effect immediately (same discipline as
    /// [`Db::block_matcher`]).
    ///
    /// # Errors
    /// Returns an error if a statement fails.
    pub(crate) fn reject_matcher(&self) -> Result<RejectMatcher> {
        let mut stmt = self
            .conn
            .prepare("SELECT service_id, file_id, tag_id FROM mapping_rejections")?;
        let rejected = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(RejectMatcher { rejected })
    }

    /// Every tag id whose text matches `pattern`. Unlike
    /// [`Db::pattern_canonical_tag_ids`], this does **not** filter to canonical
    /// tags — a block must suppress the literal tag a mapping points at, alias or
    /// not.
    fn tag_ids_matching_pattern(&self, pattern: &TagPattern) -> Result<Vec<i64>> {
        let ids: Vec<i64> = match pattern {
            TagPattern::NamespaceAny { namespace } => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM tags WHERE namespace = ?1")?;
                stmt.query_map(params![namespace], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            TagPattern::NamespaceGlob { namespace, glob } => {
                let like = like_glob(glob);
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM tags WHERE namespace = ?1 AND subtag LIKE ?2 ESCAPE '\\'",
                )?;
                stmt.query_map(params![namespace, like], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            TagPattern::AnyNamespaceGlob { glob } => {
                let like = like_glob(glob);
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM tags WHERE subtag LIKE ?1 ESCAPE '\\'")?;
                stmt.query_map(params![like], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(ids)
    }

    /// Canonical tag ids whose text matches `pattern`. Bad aliases are filtered
    /// out (an alias whose ideal lies elsewhere must not match a namespace/prefix
    /// it only spells), so this matches the same canonical tags that appear in
    /// effective tag sets.
    fn pattern_canonical_tag_ids(
        &self,
        pattern: &TagPattern,
        siblings: &SiblingEdges,
    ) -> Result<Vec<i64>> {
        let ids: Vec<i64> = match pattern {
            TagPattern::NamespaceAny { namespace } => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM tags WHERE namespace = ?1")?;
                stmt.query_map(params![namespace], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            TagPattern::NamespaceGlob { namespace, glob } => {
                let like = like_glob(glob);
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM tags WHERE namespace = ?1 AND subtag LIKE ?2 ESCAPE '\\'",
                )?;
                stmt.query_map(params![namespace, like], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            TagPattern::AnyNamespaceGlob { glob } => {
                let like = like_glob(glob);
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM tags WHERE subtag LIKE ?1 ESCAPE '\\'")?;
                stmt.query_map(params![like], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(ids
            .into_iter()
            .filter(|&id| canonicalize(id, siblings) == id)
            .collect())
    }

    /// File ids matching a wildcard `pattern`: the union of `match_set` over every
    /// canonical tag the pattern matches, resolved through mappings on any of
    /// `services`.
    fn wild_files_matching(
        &self,
        pattern: &TagPattern,
        services: &[i64],
        graph: &RelationGraph,
        blocks: &BlockMatcher,
        reject: Option<&RejectMatcher>,
    ) -> Result<HashSet<i64>> {
        let mut tag_ids = BTreeSet::new();
        for canonical in self.pattern_canonical_tag_ids(pattern, graph.siblings())? {
            tag_ids.extend(graph.match_set(canonical));
        }
        self.file_ids_with_any_tag(&tag_ids, services, blocks, reject)
    }

    /// File ids matching a single system (metadata) predicate, read straight from
    /// the `files` table. Independent of `service_id` — `files` is the content
    /// catalogue and does not vary by tag service. NULL columns never satisfy a
    /// comparison (so a dimensionless file is excluded by `width>0`); negation in
    /// `search` re-includes them by subtraction.
    fn system_files_matching(&self, pred: &SystemPredicate) -> Result<HashSet<i64>> {
        let rows = match pred {
            SystemPredicate::Compare { field, op, value } => {
                // `column` and `sql_op` come from enums — fixed string literals,
                // never user text — so there is no injection surface; only `value`
                // is bound.
                let column = match field {
                    SysField::Size => "size",
                    SysField::Width => "width",
                    SysField::Height => "height",
                    SysField::Duration => "duration_ms",
                };
                let sql_op = match op {
                    CmpOp::Gt => ">",
                    CmpOp::Lt => "<",
                    CmpOp::Ge => ">=",
                    CmpOp::Le => "<=",
                    CmpOp::Eq => "=",
                };
                let sql = format!("SELECT id FROM files WHERE {column} {sql_op} ?1");
                let mut stmt = self.conn.prepare(&sql)?;
                stmt.query_map(params![value], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<HashSet<i64>>>()?
            }
            SystemPredicate::FileType { mime } => {
                let mut stmt = self.conn.prepare("SELECT id FROM files WHERE mime = ?1")?;
                stmt.query_map(params![mime], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<HashSet<i64>>>()?
            }
            // `Origin` is dispatched in `search` before reaching this method;
            // it should never arrive here. Return an empty set defensively.
            SystemPredicate::Origin { .. } => HashSet::new(),
        };
        Ok(rows)
    }

    /// All file content ids.
    fn all_file_ids(&self) -> Result<HashSet<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM files")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        Ok(rows)
    }

    /// Resolve content ids to [`FileListing`]s (one representative location each),
    /// ordered by hash for deterministic output.
    fn listings_for(&self, ids: &HashSet<i64>) -> Result<Vec<FileListing>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let in_list = int_list(ids.iter().copied());
        // Require a present location so search never surfaces files whose every
        // location is missing (deleted from disk, or hidden on unwatch).
        let sql = format!(
            "SELECT f.blake3, f.size, l.path, f.imported_at, l.created_at, l.mtime, f.mime
             FROM files f
             JOIN file_locations l ON l.file_id = f.id AND l.present = 1
             WHERE f.id IN ({in_list})
             GROUP BY f.id
             ORDER BY f.blake3"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_listing)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All current sibling edges for `service_id` as `(bad, ideal)` tag pairs,
    /// ordered by the bad tag.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn list_siblings(&self, service_id: i64) -> Result<Vec<(Tag, Tag)>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.namespace, b.subtag, i.namespace, i.subtag
             FROM tag_siblings s
             JOIN tags b ON b.id = s.bad_tag_id
             JOIN tags i ON i.id = s.ideal_tag_id
             WHERE s.service_id = ?1 AND s.status = 'current'
             ORDER BY b.namespace, b.subtag",
        )?;
        let rows = stmt
            .query_map(params![service_id], |r| {
                Ok((
                    Tag {
                        namespace: r.get(0)?,
                        subtag: r.get(1)?,
                    },
                    Tag {
                        namespace: r.get(2)?,
                        subtag: r.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All current parent edges for `service_id` as `(child, parent)` tag pairs,
    /// ordered by the child tag.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn list_parents(&self, service_id: i64) -> Result<Vec<(Tag, Tag)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.namespace, c.subtag, p.namespace, p.subtag
             FROM tag_parents r
             JOIN tags c ON c.id = r.child_tag_id
             JOIN tags p ON p.id = r.parent_tag_id
             WHERE r.service_id = ?1 AND r.status = 'current'
             ORDER BY c.namespace, c.subtag",
        )?;
        let rows = stmt
            .query_map(params![service_id], |r| {
                Ok((
                    Tag {
                        namespace: r.get(0)?,
                        subtag: r.get(1)?,
                    },
                    Tag {
                        namespace: r.get(2)?,
                        subtag: r.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every current relation edge across all services, with provenance, for the
    /// `relation list` read path. Ordered by kind, then service name, then the
    /// `from` tag. Distinct from [`Db::list_siblings`]/[`Db::list_parents`], which
    /// are single-service and presence-less.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn list_relation_edges(&self) -> Result<Vec<RelationEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, fns, fsub, tns, tsub, service, author FROM (
                 SELECT 'sibling' AS kind, b.namespace AS fns, b.subtag AS fsub,
                        i.namespace AS tns, i.subtag AS tsub,
                        sv.name AS service, s.author AS author
                 FROM tag_siblings s
                 JOIN tags b ON b.id = s.bad_tag_id
                 JOIN tags i ON i.id = s.ideal_tag_id
                 JOIN services sv ON sv.id = s.service_id
                 WHERE s.status = 'current'
                 UNION ALL
                 SELECT 'parent' AS kind, c.namespace AS fns, c.subtag AS fsub,
                        p.namespace AS tns, p.subtag AS tsub,
                        sv.name AS service, r.author AS author
                 FROM tag_parents r
                 JOIN tags c ON c.id = r.child_tag_id
                 JOIN tags p ON p.id = r.parent_tag_id
                 JOIN services sv ON sv.id = r.service_id
                 WHERE r.status = 'current'
             )
             ORDER BY kind, service, fns, fsub",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind = match r.get::<_, String>(0)?.as_str() {
                    "parent" => EdgeKind::Parent,
                    _ => EdgeKind::Sibling,
                };
                Ok(RelationEdgeRow {
                    kind,
                    from: Tag {
                        namespace: r.get(1)?,
                        subtag: r.get(2)?,
                    },
                    to: Tag {
                        namespace: r.get(3)?,
                        subtag: r.get(4)?,
                    },
                    service: r.get(5)?,
                    author: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Per-service relation counts and last-relation-pull time, for the
    /// `relation status` read path. Reports every service, ordered by id (the
    /// seeded local service first).
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn relation_status(&self) -> Result<Vec<ServiceRelationStatus>> {
        let mut stmt = self.conn.prepare(
            "SELECT sv.name,
                    (SELECT COUNT(*) FROM tag_siblings s
                       WHERE s.service_id = sv.id AND s.status = 'current'),
                    (SELECT COUNT(*) FROM tag_parents r
                       WHERE r.service_id = sv.id AND r.status = 'current'),
                    sv.last_relation_pull_at
             FROM services sv
             ORDER BY sv.id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ServiceRelationStatus {
                    service: r.get(0)?,
                    siblings: r.get::<_, i64>(1)? as u64,
                    parents: r.get::<_, i64>(2)? as u64,
                    last_pull: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The id of any service named `name`, if it exists.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn service_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM services WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The SHA-256 hex stored for the file with `hash`, if known.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn sha256_of(&self, hash: &Hash) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT sha256 FROM files WHERE blake3 = ?1")?;
        Ok(stmt
            .query_row(params![hash.to_hex()], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// Create a dedicated local service named `name`, returning its id.
    ///
    /// # Errors
    /// Returns an error if the name is already taken or the statement fails.
    pub fn add_local_service(&self, name: &str, origin: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO services (name, scope, origin) VALUES (?1, 'local', ?2)",
            params![name, origin],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// How many files in the library have no SHA-256 interop hash, regardless
    /// of whether they are currently present on disk.
    ///
    /// This is the **true** count of files that cannot yet participate in a
    /// sha256-domain pull: if a file has no sha256 it derives no bucket key
    /// and its tags are silently absent from the pull result, present or not.
    /// The two sub-populations are:
    ///
    /// * **Backfillable** — file is present (`present = 1`), so naiad can open
    ///   and hash it. Counted by [`Db::count_files_missing_sha256_present`].
    /// * **Offline** — file has no present location; it cannot be hashed until
    ///   the volume comes back online and a rescan fills in the sha256. The
    ///   difference between this count and the present count tells an operator
    ///   how many files are in this state.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn count_files_missing_sha256(&self) -> Result<u64> {
        // No location join needed: a files row with sha256 IS NULL is missing
        // regardless of how many (or zero) locations it has.
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM files WHERE sha256 IS NULL", [], |r| {
                    r.get(0)
                })?;
        Ok(n as u64)
    }

    /// How many **present** files still lack a SHA-256 interop hash.
    ///
    /// The cheap counterpart of [`Db::files_missing_sha256_after`], for the
    /// decision "is a backfill needed at all?" — a full SHA-256-domain pull
    /// asks this on every run and must not materialise a path list to answer
    /// it. Uses the `present = 1` join: a file the client cannot open cannot
    /// be re-hashed, so it is not backfillable work.
    ///
    /// For the full missing count (present + offline) see
    /// [`Db::count_files_missing_sha256`].
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn count_files_missing_sha256_present(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT f.id
                 FROM files f
                 JOIN file_locations l ON l.file_id = f.id AND l.present = 1
                 WHERE f.sha256 IS NULL
                 GROUP BY f.id
             )",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// One page of present files lacking a SHA-256, as `(file_id, path)`:
    /// at most `limit` rows with `id > after_id`, ascending.
    ///
    /// **Paging is by id, not by a bare `LIMIT`, and that is load-bearing.** A
    /// file that stays in the result set because it could not be hashed (an
    /// offline volume, a file held under an exclusive handle) would re-fill
    /// every page of a `LIMIT`-only query and starve the readable files behind
    /// it forever — the caller would loop making no progress. An ascending id
    /// cursor guarantees each page strictly advances regardless of how many
    /// rows the previous page failed to resolve.
    ///
    /// Only present files are returned: an offline file genuinely cannot be
    /// re-hashed until its volume comes back online. For the total (present +
    /// offline) missing count see [`Db::count_files_missing_sha256`].
    ///
    /// Note the `LIMIT` bounds the rows *returned*, not the work SQLite does:
    /// the plan scans `file_locations` and groups into a temp b-tree, so the
    /// engine still touches every matching row per call.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn files_missing_sha256_after(
        &self,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, l.path
             FROM files f
             JOIN file_locations l ON l.file_id = f.id AND l.present = 1
             WHERE f.sha256 IS NULL AND f.id > ?1
             GROUP BY f.id
             ORDER BY f.id
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![after_id, limit as i64], |r| {
                let id: i64 = r.get(0)?;
                let bytes: Vec<u8> = r.get(1)?;
                Ok((id, path_from_bytes(&bytes)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Present files lacking a SHA-256, as `(file_id, path)`.
    ///
    /// Prefer [`Db::files_missing_sha256_after`] for production paths to
    /// avoid loading an unbounded list into memory. This unbounded form exists
    /// for tests that assert exact set membership.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn files_missing_sha256(&self) -> Result<Vec<(i64, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, l.path
             FROM files f
             JOIN file_locations l ON l.file_id = f.id AND l.present = 1
             WHERE f.sha256 IS NULL
             GROUP BY f.id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let id: i64 = r.get(0)?;
                let bytes: Vec<u8> = r.get(1)?;
                Ok((id, path_from_bytes(&bytes)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct SHA-256 hexes of present library files. The key set a
    /// library-scoped import intersects against the source DB — "pull tags for
    /// the files I actually have". Files without a SHA-256 (un-backfilled) are
    /// skipped; run [`Db::files_missing_sha256`] backfill first to include them.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn library_sha256s(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT f.sha256
             FROM files f
             JOIN file_locations l ON l.file_id = f.id AND l.present = 1
             WHERE f.sha256 IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Present library files as `(file_id, sha256-hex)`. Unlike
    /// [`Db::library_sha256s`] this keeps the file id, so a library-scoped import
    /// can apply pulled tags directly to the file (no SHA-256 → file resolve
    /// round-trip). Files without a SHA-256 are skipped.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn library_files_with_sha256(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT f.id, f.sha256
             FROM files f
             JOIN file_locations l ON l.file_id = f.id AND l.present = 1
             WHERE f.sha256 IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Apply imported `(file_id, tag)` mappings to `service_id` in one
    /// transaction, interning tags as needed. Existing rows are left untouched
    /// (`ON CONFLICT DO NOTHING`). Returns the number of *new* rows written.
    ///
    /// Used by the library-scoped import to commit tags in bounded batches so a
    /// long import lands file-by-file and survives interruption — distinct from
    /// the staging/resolve path the full import uses for not-yet-present files.
    ///
    /// # Errors
    /// Returns an error if any statement fails (the transaction is rolled back).
    pub fn apply_hydrus_mappings(&self, service_id: i64, items: &[(i64, Tag)]) -> Result<u64> {
        let started = Instant::now();
        let tx = self.conn.unchecked_transaction()?;
        let now = unix_now();
        let mut applied = 0u64;
        for (file_id, tag) in items {
            let tag_id = self.intern_tag(tag)?;
            let n = self.conn.execute(
                "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
                 VALUES (?1, ?2, ?3, 'current', ?4)
                 ON CONFLICT(file_id, tag_id, service_id) DO NOTHING",
                params![file_id, tag_id, service_id, now],
            )?;
            applied += n as u64;
            tracing::trace!(target: "hydrus", file_id, %tag, "apply hydrus mapping row");
        }
        tx.commit()?;
        tracing::debug!(target: "hydrus", target_service = service_id, applied, submitted = items.len() as u64, elapsed_ms = started.elapsed().as_millis() as u64, "applied hydrus mapping batch");
        Ok(applied)
    }

    /// Set the SHA-256 hex for a file row.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn set_sha256(&self, file_id: i64, sha256: &str) -> Result<()> {
        let sha_lc = sha256.to_lowercase();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE files SET sha256 = ?1 WHERE id = ?2",
            params![sha_lc, file_id],
        )?;
        let needs_stamp: bool = tx.query_row(
            "SELECT sha256 IS NOT NULL AND sha256_seq IS NULL FROM files WHERE id = ?1",
            params![file_id],
            |r| r.get(0),
        )?;
        if needs_stamp {
            let seq = Self::reserve_sha256_seq(&tx, 1)?;
            tx.execute(
                "UPDATE files SET sha256_seq = ?1 WHERE id = ?2",
                params![seq, file_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Stage one imported file→tag record by SHA-256. Idempotent on
    /// `(sha256, tag_id, service_id)`; a later row updates the status.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn stage_mapping(
        &self,
        sha256: &str,
        tag_id: i64,
        service_id: i64,
        status: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO staged_mappings (sha256, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(sha256, tag_id, service_id)
             DO UPDATE SET status = excluded.status, created_at = excluded.created_at",
            params![sha256, tag_id, service_id, status, unix_now()],
        )?;
        Ok(())
    }

    /// Stage a batch of imported file→tag mappings for `service_id` in a
    /// single transaction, interning each tag through `cache` so repeated tags
    /// cost only a [`HashMap`] lookup after the first occurrence.
    ///
    /// SQL is identical to [`Db::stage_mapping`]: `ON CONFLICT … DO UPDATE SET
    /// status` ensures a later entry for the same `(sha256, tag_id, service_id)`
    /// key wins, so within-batch ordering is preserved. Prepared statements are
    /// cached per connection via [`Connection::prepare_cached`].
    ///
    /// The tuple is `(sha256, tag, status)`.
    ///
    /// Returns the number of items processed (equal to `items.len()`). This
    /// matches how the daemon's old `DbSink` counted `mappings_staged` — one
    /// per record submitted, not one per row changed.
    ///
    /// # Errors
    /// Returns an error if any statement fails; the transaction is rolled back.
    pub fn stage_mappings_batch(
        &self,
        service_id: i64,
        items: &[(String, Tag, &str)],
        cache: &mut TagCache,
    ) -> Result<u64> {
        let started = Instant::now();
        let tx = self.conn.unchecked_transaction()?;
        let now = unix_now();
        let mut pending = TagCache::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO staged_mappings (sha256, tag_id, service_id, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(sha256, tag_id, service_id)
                 DO UPDATE SET status = excluded.status, created_at = excluded.created_at",
            )?;
            for (sha256, tag, status) in items {
                let tag_id = self.intern_tag_cached(tag, cache, &mut pending)?;
                stmt.execute(params![sha256, tag_id, service_id, status, now])?;
                tracing::trace!(target: "db", %sha256, %tag, %status, "stage mapping row");
            }
        }
        tx.commit()?;
        cache.0.extend(pending.0);
        tracing::debug!(target: "db", target_service = service_id, staged = items.len() as u64, elapsed_ms = started.elapsed().as_millis() as u64, "staged mapping batch");
        Ok(items.len() as u64)
    }

    /// Batch-apply sibling relations for `service_id` in a single transaction,
    /// interning both tags through `cache`.
    ///
    /// SQL is identical to [`Db::add_sibling`]: `ON CONFLICT … DO UPDATE SET
    /// ideal_tag_id` so re-aliasing replaces the prior ideal for a bad tag.
    /// Items whose bad and ideal tags resolve to the same id (self-relations)
    /// are silently skipped and excluded from the returned count — mirroring
    /// how the daemon's old `DbSink` swallowed [`Error::SelfRelation`].
    ///
    /// Returns the number of items applied (self-relations excluded).
    ///
    /// # Errors
    /// Returns an error if any statement fails; the transaction is rolled back.
    pub fn add_siblings_batch(
        &self,
        service_id: i64,
        items: &[(Tag, Tag)],
        cache: &mut TagCache,
    ) -> Result<u64> {
        let started = Instant::now();
        let tx = self.conn.unchecked_transaction()?;
        let now = unix_now();
        let mut applied = 0u64;
        let mut pending = TagCache::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO tag_siblings
                     (bad_tag_id, ideal_tag_id, service_id, status, created_at)
                 VALUES (?1, ?2, ?3, 'current', ?4)
                 ON CONFLICT(bad_tag_id, service_id)
                 DO UPDATE SET ideal_tag_id = excluded.ideal_tag_id,
                               status = 'current',
                               created_at = excluded.created_at",
            )?;
            for (bad, ideal) in items {
                let bad_id = self.intern_tag_cached(bad, cache, &mut pending)?;
                let ideal_id = self.intern_tag_cached(ideal, cache, &mut pending)?;
                tracing::trace!(target: "db", from = %bad, to = %ideal, "add sibling row");
                if bad_id == ideal_id {
                    continue; // SelfRelation — skip, do not count
                }
                stmt.execute(params![bad_id, ideal_id, service_id, now])?;
                applied += 1;
            }
        }
        tx.commit()?;
        cache.0.extend(pending.0);
        tracing::debug!(target: "db", target_service = service_id, applied, elapsed_ms = started.elapsed().as_millis() as u64, "applied sibling batch");
        Ok(applied)
    }

    /// Batch-apply parent relations for `service_id` in a single transaction,
    /// interning both tags through `cache`.
    ///
    /// SQL is identical to [`Db::add_parent`]: `ON CONFLICT … DO NOTHING`
    /// makes this idempotent — adding an existing `(child, parent)` edge is a
    /// no-op. Items whose child and parent tags resolve to the same id
    /// (self-relations) are silently skipped and excluded from the returned
    /// count, mirroring how the daemon's old `DbSink` swallowed
    /// [`Error::SelfRelation`].
    ///
    /// Returns the number of items applied (self-relations excluded).
    ///
    /// # Errors
    /// Returns an error if any statement fails; the transaction is rolled back.
    pub fn add_parents_batch(
        &self,
        service_id: i64,
        items: &[(Tag, Tag)],
        cache: &mut TagCache,
    ) -> Result<u64> {
        let started = Instant::now();
        let tx = self.conn.unchecked_transaction()?;
        let now = unix_now();
        let mut applied = 0u64;
        let mut pending = TagCache::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO tag_parents
                     (child_tag_id, parent_tag_id, service_id, status, created_at)
                 VALUES (?1, ?2, ?3, 'current', ?4)
                 ON CONFLICT(child_tag_id, parent_tag_id, service_id) DO NOTHING",
            )?;
            for (child, parent) in items {
                let child_id = self.intern_tag_cached(child, cache, &mut pending)?;
                let parent_id = self.intern_tag_cached(parent, cache, &mut pending)?;
                tracing::trace!(target: "db", from = %child, to = %parent, "add parent row");
                if child_id == parent_id {
                    continue; // SelfRelation — skip, do not count
                }
                stmt.execute(params![child_id, parent_id, service_id, now])?;
                applied += 1;
            }
        }
        tx.commit()?;
        cache.0.extend(pending.0);
        tracing::debug!(target: "db", target_service = service_id, applied, elapsed_ms = started.elapsed().as_millis() as u64, "applied parent batch");
        Ok(applied)
    }

    /// Set the SHA-256 hex for a batch of file rows in a single transaction.
    ///
    /// SQL is identical to [`Db::set_sha256`]. Prepared statements are cached
    /// via [`Connection::prepare_cached`].
    ///
    /// Returns the number of items processed (equal to `items.len()`).
    ///
    /// # Errors
    /// Returns an error if any statement fails; the transaction is rolled back.
    pub fn set_sha256_batch(&self, items: &[(i64, String)]) -> Result<u64> {
        let started = Instant::now();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = self
                .conn
                .prepare_cached("UPDATE files SET sha256 = lower(?1) WHERE id = ?2")?;
            for (file_id, sha256) in items {
                stmt.execute(params![sha256, file_id])?;
                tracing::trace!(target: "db", file_id, %sha256, "set sha256 row");
            }
        }
        // Which of this batch's rows just gained a sha256 (seq still NULL)?
        // Collect their ids in a stable order so the reserved range is assigned
        // deterministically low-id → low-seq.
        let mut to_stamp: Vec<i64> = Vec::new();
        {
            let mut q = self.conn.prepare_cached(
                "SELECT id FROM files WHERE id = ?1 AND sha256 IS NOT NULL AND sha256_seq IS NULL",
            )?;
            for (file_id, _) in items {
                if let Some(id) = q
                    .query_row(params![file_id], |r| r.get::<_, i64>(0))
                    .optional()?
                {
                    to_stamp.push(id);
                }
            }
        }
        to_stamp.sort_unstable();
        to_stamp.dedup();
        if !to_stamp.is_empty() {
            // One reservation for the whole batch (see spec §"The counter
            // itself"): a single UPDATE … + n … RETURNING, not n probes.
            let n = to_stamp.len() as i64;
            let hi = Self::reserve_sha256_seq(&tx, n)?;
            let base = hi - n + 1;
            let mut up = self
                .conn
                .prepare_cached("UPDATE files SET sha256_seq = ?1 WHERE id = ?2")?;
            for (i, id) in to_stamp.iter().enumerate() {
                up.execute(params![base + i as i64, id])?;
            }
        }
        tx.commit()?;
        tracing::debug!(target: "db", rows = items.len() as u64, elapsed_ms = started.elapsed().as_millis() as u64, "set sha256 batch");
        Ok(items.len() as u64)
    }

    /// Resolve staged rows for `service_id` whose SHA-256 matches a known file:
    /// `current` rows become `mappings`, `deleted` rows retract them. Resolved
    /// rows are deleted from staging. Returns the number of staged rows applied.
    ///
    /// `domain` names the provenance the resolved rows carry (migration 0034).
    /// Staged rows are SHA-256-keyed (`stage_mapping` keys on `sha256`), so in
    /// practice this is `"sha256"`, but the bit is threaded rather than
    /// hard-coded so the function stays honest about what it stamps — and so the
    /// SHA-256 leg's merge can actually reap these rows (it only ever clears the
    /// sha256 bit; a row mislabelled with the blake3 DEFAULT would leak).
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn resolve_staged_mappings(&self, service_id: i64, domain: &str) -> Result<u64> {
        let started = Instant::now();
        let bit = domain_bit(domain);
        self.conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at, domains)
             SELECT f.id, s.tag_id, s.service_id, 'current', s.created_at, ?2
             FROM staged_mappings s
             JOIN files f ON f.sha256 = s.sha256
             WHERE s.service_id = ?1 AND s.status = 'current'
             ON CONFLICT(file_id, tag_id, service_id) DO UPDATE SET
                 status = 'current',
                 domains = domains | ?2",
            params![service_id, bit],
        )?;
        // Retract this domain's claim on staged deletes, then reap rows whose
        // mask reached 0 — mirroring merge_mapping_delta's delete path so a row
        // both domains supply survives a sha256 retraction with its blake3 bit.
        self.conn.execute(
            "UPDATE mappings SET domains = domains & ~?2
             WHERE service_id = ?1
               AND (file_id, tag_id) IN (
                   SELECT f.id, s.tag_id
                   FROM staged_mappings s
                   JOIN files f ON f.sha256 = s.sha256
                   WHERE s.service_id = ?1 AND s.status = 'deleted'
               )",
            params![service_id, bit],
        )?;
        self.conn.execute(
            "DELETE FROM mappings WHERE service_id = ?1 AND domains = 0",
            params![service_id],
        )?;
        let drained = self.conn.execute(
            "DELETE FROM staged_mappings
             WHERE service_id = ?1
               AND sha256 IN (SELECT sha256 FROM files WHERE sha256 IS NOT NULL)",
            params![service_id],
        )?;
        let applied = drained as u64;
        tracing::debug!(target: "db", service_id, domain, applied, elapsed_ms = started.elapsed().as_millis() as u64, "resolved staged mappings");
        Ok(applied)
    }

    /// Ranked tag completions for the current search `token`. A `namespace:subtag`
    /// token filters to that namespace; a bare token matches the subtag across all
    /// namespaces. Ranked by current-mapping count, descending, capped at `limit`.
    /// Unfiltered by trust/block/local-only — a typeahead, not a search.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn complete_tags(
        &self,
        token: &str,
        limit: usize,
        mode: CompletionMode,
    ) -> Result<Vec<TagSuggestion>> {
        // Double quotes are phrase-grouping delimiters, not literal characters
        // (mirrors `core::tokenize`). A quoted completion fragment such as
        // `"some` or `creator:"some` must match the bare tag, so strip quotes
        // before splitting/matching — otherwise typeahead for spaced tags is
        // dead on the opening quote. (#49)
        let unquoted = strip_completion_quotes(token);
        let token = unquoted.as_str();
        if token.is_empty() {
            return Ok(Vec::new());
        }
        let (ns, sub) = split_completion_token(token);
        let pat = match mode {
            CompletionMode::Prefix => format!("{}%", escape_like(&sub)),
            CompletionMode::Substring => format!("%{}%", escape_like(&sub)),
        };

        let services = self.included_services(ReadScope::Merged)?;
        let overlay = self.relation_completion(&services)?;

        // Fast path: no sibling relations → plain indexed scan, no merge overhead.
        if overlay.is_empty() {
            let scan_limit = limit as i64;
            let mut out = Vec::new();
            match ns {
                Some(ns) => {
                    let mut stmt = self.conn.prepare(
                        "SELECT t.namespace, t.subtag, c.current_count
                         FROM tags t
                         JOIN tag_completion_counts c ON c.tag_id = t.id
                         WHERE t.namespace = ?1 AND t.subtag LIKE ?2 ESCAPE '\\'
                         ORDER BY c.current_count DESC, t.namespace ASC, t.subtag ASC
                         LIMIT ?3",
                    )?;
                    let rows = stmt.query_map(params![ns, pat, scan_limit], |r| {
                        Ok(TagSuggestion {
                            namespace: r.get(0)?,
                            subtag: r.get(1)?,
                            count: r.get(2)?,
                            alias_source: None,
                        })
                    })?;
                    for row in rows {
                        out.push(row?);
                    }
                }
                None => {
                    let mut stmt = self.conn.prepare(
                        "SELECT t.namespace, t.subtag, c.current_count
                         FROM tags t
                         JOIN tag_completion_counts c ON c.tag_id = t.id
                         WHERE t.subtag LIKE ?1 ESCAPE '\\'
                         ORDER BY c.current_count DESC, t.namespace ASC, t.subtag ASC
                         LIMIT ?2",
                    )?;
                    let rows = stmt.query_map(params![pat, scan_limit], |r| {
                        Ok(TagSuggestion {
                            namespace: r.get(0)?,
                            subtag: r.get(1)?,
                            count: r.get(2)?,
                            alias_source: None,
                        })
                    })?;
                    for row in rows {
                        out.push(row?);
                    }
                }
            }
            return Ok(out);
        }

        // Merge path: sibling relations exist → widen the base scan to fill
        // `limit` after alias rows are dropped, then merge counts.
        let scan_limit = (limit.max(20).saturating_mul(3)).min(200) as i64;
        let sub_str: &str = &sub;

        // Base scan: same SQL but also selects t.id for overlay lookup.
        let mut base: Vec<(i64, TagSuggestion)> = Vec::new();
        match ns {
            Some(ns) => {
                let mut stmt = self.conn.prepare(
                    "SELECT t.id, t.namespace, t.subtag, c.current_count
                     FROM tags t
                     JOIN tag_completion_counts c ON c.tag_id = t.id
                     WHERE t.namespace = ?1 AND t.subtag LIKE ?2 ESCAPE '\\'
                     ORDER BY c.current_count DESC, t.namespace ASC, t.subtag ASC
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![ns, pat, scan_limit], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        TagSuggestion {
                            namespace: r.get(1)?,
                            subtag: r.get(2)?,
                            count: r.get(3)?,
                            alias_source: None,
                        },
                    ))
                })?;
                for row in rows {
                    base.push(row?);
                }
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT t.id, t.namespace, t.subtag, c.current_count
                     FROM tags t
                     JOIN tag_completion_counts c ON c.tag_id = t.id
                     WHERE t.subtag LIKE ?1 ESCAPE '\\'
                     ORDER BY c.current_count DESC, t.namespace ASC, t.subtag ASC
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![pat, scan_limit], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        TagSuggestion {
                            namespace: r.get(1)?,
                            subtag: r.get(2)?,
                            count: r.get(3)?,
                            alias_source: None,
                        },
                    ))
                })?;
                for row in rows {
                    base.push(row?);
                }
            }
        }

        // Keyed by canonical id; alias rows are suppressed, canonicals get
        // their merged count (raw(canonical) + Σ raw(aliases)).
        let mut emitted: HashMap<i64, TagSuggestion> = HashMap::new();
        // Canonical ids surfaced by alias matches — need injection if not yet emitted.
        let mut surfaced: HashSet<i64> = HashSet::new();
        // Best alias candidate seen for a surfaced canonical: (raw_count, formatted).
        // Higher raw_count wins; ties break on the formatted name ascending.
        let mut best_alias: HashMap<i64, (i64, String)> = HashMap::new();
        fn consider(best: &mut HashMap<i64, (i64, String)>, canon: i64, raw: i64, name: String) {
            let replace = match best.get(&canon) {
                None => true,
                Some((cr, cn)) => raw > *cr || (raw == *cr && name < *cn),
            };
            if replace {
                best.insert(canon, (raw, name));
            }
        }

        // Step 1: classify every base-scan row.
        for (tag_id, suggestion) in base {
            if let Some(canon) = overlay.canonical_of(tag_id) {
                // Bad alias — suppress its row, surface the canonical instead.
                let raw = suggestion.count;
                let alias_fmt = Tag {
                    namespace: suggestion.namespace,
                    subtag: suggestion.subtag,
                }
                .to_string();
                consider(&mut best_alias, canon, raw, alias_fmt);
                surfaced.insert(canon);
            } else {
                // Canonical (or unrelated tag with no alias): emit once.
                let count = overlay.merged_count(tag_id).unwrap_or(suggestion.count);
                emitted.entry(tag_id).or_insert(TagSuggestion {
                    namespace: suggestion.namespace,
                    subtag: suggestion.subtag,
                    count,
                    alias_source: None,
                });
            }
        }

        // Step 2: alias spellings — any alias whose name matches the fragment
        // surfaces its canonical, even if the alias wasn't in the base scan.
        for (canon, alias_name) in overlay.alias_names_iter() {
            if fragment_matches(alias_name, ns, sub_str, mode) {
                consider(&mut best_alias, canon, 0, alias_name.to_string());
                surfaced.insert(canon);
            }
        }

        // Step 3: ideal spellings — a canonical whose ideal name matches the
        // fragment is injected directly (covers zero-raw ideals).
        for (canon, ideal_name) in overlay.ideal_names_iter() {
            if fragment_matches(ideal_name, ns, sub_str, mode) && !emitted.contains_key(&canon) {
                if let Some(count) = overlay.merged_count(canon) {
                    emitted.insert(
                        canon,
                        TagSuggestion {
                            namespace: ideal_name.namespace.clone(),
                            subtag: ideal_name.subtag.clone(),
                            count,
                            alias_source: None,
                        },
                    );
                }
            }
        }

        // Step 4: surfaced canonicals not yet emitted — inject via ideal_name.
        for &canon in &surfaced {
            if let std::collections::hash_map::Entry::Vacant(e) = emitted.entry(canon) {
                if let (Some(name), Some(count)) =
                    (overlay.ideal_name(canon), overlay.merged_count(canon))
                {
                    e.insert(TagSuggestion {
                        namespace: name.namespace.clone(),
                        subtag: name.subtag.clone(),
                        count,
                        alias_source: best_alias.get(&canon).map(|(_, n)| n.clone()),
                    });
                }
            }
        }

        // Sort: count desc, namespace asc, subtag asc; truncate to limit.
        let mut out: Vec<TagSuggestion> = emitted.into_values().collect();
        out.sort_unstable_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.namespace.cmp(&b.namespace))
                .then_with(|| a.subtag.cmp(&b.subtag))
        });
        out.truncate(limit);
        Ok(out)
    }

    /// Namespaces whose name prefix-matches `prefix`, ranked by how many distinct
    /// **mapped** tags they hold (tags with at least one current mapping;
    /// relation-only dictionary entries are excluded), descending, capped at
    /// `limit`. Excludes the unnamespaced `''` namespace and any namespace
    /// whose tags are all relation-only (zero current mappings). An empty
    /// `prefix` matches all namespaces (unlike [`Db::complete_tags`], which
    /// returns empty for an empty token); callers gate on non-empty input.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn complete_namespaces(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<NamespaceSuggestion>> {
        // Strip quote delimiters (see `complete_tags`) so a namespaced quoted
        // fragment like `creator:"some` still resolves its namespace. (#49)
        let pat = format!("{}%", escape_like(&strip_completion_quotes(prefix)));
        // Reads the trigger-maintained `tag_namespace_counts` (one row per
        // namespace) rather than scanning the full `tags` table — the 1.07M-row
        // scan cost ~125s on a cold cache (#70).
        let mut stmt = self.conn.prepare(
            "SELECT namespace, tag_count
             FROM tag_namespace_counts
             WHERE namespace LIKE ?1 ESCAPE '\\'
             ORDER BY tag_count DESC, namespace ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pat, limit as i64], |r| {
            Ok(NamespaceSuggestion {
                namespace: r.get(0)?,
                tag_count: r.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Stream current local-service mappings of active files, calling `f` once
    /// per (blake3_hex, tag_string). Read-only; constant memory.
    ///
    /// Only rows where `mappings.status = 'current'`, `files.state = 'active'`,
    /// and `services.scope = 'local'` are emitted. The tag string is built via
    /// [`naiad_core::Tag`]`::to_string` (not SQL concat) so canonical form
    /// handles unnamespaced and leading-colon edge cases correctly.
    ///
    /// # Errors
    /// Returns an error if the query fails or if `f` returns an error.
    pub fn for_each_active_local_mapping(
        &self,
        mut f: impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT f.blake3, t.namespace, t.subtag
             FROM mappings m
             JOIN files f ON f.id = m.file_id
             JOIN tags t ON t.id = m.tag_id
             JOIN services s ON s.id = m.service_id
             WHERE m.status = 'current' AND f.state = 'active' AND s.scope = 'local'
             ORDER BY f.blake3, t.namespace, t.subtag",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let blake3: String = row.get(0)?;
            let namespace: String = row.get(1)?;
            let subtag: String = row.get(2)?;
            let tag_str = Tag { namespace, subtag }.to_string();
            f(&blake3, &tag_str)?;
        }
        Ok(())
    }

    /// Assert that `path` refers to the client library database, not a repo
    /// database. Call this after [`Db::open_readonly`] as a preflight guard.
    ///
    /// A naiad repo database has a `repo_mappings` table but no `files` table.
    /// If such a file is detected, a self-explanatory error is returned before
    /// any further work is done — matching the style of the server's guard in
    /// `crates/server/src/store.rs`.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] if the database is identified as a repo store,
    /// or a [`Error::Sqlite`] if a schema query fails.
    pub fn assert_client_library(&self, path: &Path) -> Result<()> {
        let table_exists = |name: &str| -> rusqlite::Result<bool> {
            self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [name],
                |r| r.get(0),
            )
        };
        let has_repo_mappings = table_exists("repo_mappings")?;
        let has_files = table_exists("files")?;
        if has_repo_mappings && !has_files {
            return Err(Error::Invalid(format!(
                "{} is a naiad repo database, not a client library; \
                 point --db at the client's naiad.db",
                path.display()
            )));
        }
        Ok(())
    }
}

/// A listing row for the CLI `List` command: content identity plus one of its
/// on-disk paths. (Distinct from [`naiad_core::FileContent`], which omits the
/// path, and from [`naiad_core::Location`], which omits the hash.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListing {
    /// Content hash.
    pub hash: Hash,
    /// Size in bytes.
    pub size: u64,
    /// A representative on-disk path for this content.
    pub path: std::path::PathBuf,
    /// Unix-seconds when this content was first imported.
    pub imported_at: i64,
    /// Best-effort filesystem creation time for the representative location.
    pub created_at: Option<i64>,
    /// Filesystem modification time for the representative location.
    pub modified_at: Option<i64>,
    /// Extracted MIME type, if metadata has been populated.
    pub mime: Option<String>,
}

/// Map a listing row to a [`FileListing`].
fn row_to_listing(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileListing> {
    let hex: String = row.get(0)?;
    let hash = hex.parse::<Hash>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let size: i64 = row.get(1)?;
    let path_bytes: Vec<u8> = row.get(2)?;
    let imported_at: i64 = row.get(3)?;
    let created_at: Option<i64> = row.get(4)?;
    let modified_at: Option<i64> = row.get(5)?;
    let mime: Option<String> = row.get(6)?;
    Ok(FileListing {
        hash,
        size: size as u64,
        path: path_from_bytes(&path_bytes),
        imported_at,
        created_at,
        modified_at,
        mime,
    })
}

/// Map a full `files` row to a [`FileContent`].
fn row_to_content(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileContent> {
    let id: i64 = row.get(0)?;
    let hex: String = row.get(1)?;
    let hash = hex.parse::<Hash>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let size: i64 = row.get(2)?;
    let mime: Option<String> = row.get(3)?;
    let width: Option<i64> = row.get(4)?;
    let height: Option<i64> = row.get(5)?;
    let duration_ms: Option<i64> = row.get(6)?;
    let state_str: String = row.get(7)?;
    let state = state_str.parse::<FileState>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let imported_at: i64 = row.get(8)?;
    Ok(FileContent {
        id,
        hash,
        size: size as u64,
        mime,
        width: width.map(|w| w as u32),
        height: height.map(|h| h as u32),
        duration_ms,
        state,
        imported_at,
    })
}

/// Map a `(path-blob, mtime, created_at, present, last_seen)` row to a [`Location`].
fn row_to_location(row: &rusqlite::Row<'_>) -> rusqlite::Result<Location> {
    let path_bytes: Vec<u8> = row.get(0)?;
    let mtime: Option<i64> = row.get(1)?;
    let created_at: Option<i64> = row.get(2)?;
    let present: i64 = row.get(3)?;
    let last_seen: i64 = row.get(4)?;
    Ok(Location {
        path: path_from_bytes(&path_bytes),
        mtime,
        created_at,
        present: present != 0,
        last_seen,
    })
}

/// Map a `(namespace, subtag)` row to a [`Tag`]. The stored values are already
/// normalized (interned via `Tag::parse`), so this constructs directly.
fn row_to_tag(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tag> {
    Ok(Tag {
        namespace: row.get(0)?,
        subtag: row.get(1)?,
    })
}

/// Format an iterator of ids as a comma-separated SQL integer list, e.g. `1,2,3`.
/// Safe to interpolate: the values are `i64` from our own database, not user text,
/// so this sidesteps SQLite's bound-variable limit without an injection risk.
fn int_list(ids: impl Iterator<Item = i64>) -> String {
    ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
}

/// Build a `WHERE` predicate matching a `path` column against `root` and every
/// location beneath it, plus the matching parameter blobs. Bind placeholders
/// start at `start_idx` (so callers can reserve lower indices for other params).
///
/// A descendant is any path beginning with `root + separator`. Both platform
/// separators are matched: on Windows `\` and `/` both divide components, on
/// Unix only `/`. Separators are encoded the same way paths are (UTF-16LE on
/// Windows, raw bytes on Unix), so the comparison is on the stored blob bytes.
fn subtree_predicate(root: &Path, start_idx: usize) -> (String, Vec<Vec<u8>>) {
    let exact = path_to_bytes(root);

    #[cfg(windows)]
    let seps: &[&str] = &["\\", "/"];
    #[cfg(not(windows))]
    let seps: &[&str] = &["/"];

    let mut sql = format!("path = ?{start_idx}");
    let mut blobs: Vec<Vec<u8>> = vec![exact.clone()];
    for sep in seps {
        let mut prefix = exact.clone();
        prefix.extend_from_slice(&path_to_bytes(Path::new(sep)));
        // `prefix.len()` is computed here, never user input — safe to inline.
        let len = prefix.len();
        let idx = start_idx + blobs.len();
        blobs.push(prefix);
        sql.push_str(&format!(" OR substr(path, 1, {len}) = ?{idx}"));
    }
    (sql, blobs)
}

/// Translate a tag glob (where `*` is the wildcard, anywhere in the string) into a
/// SQLite `LIKE` pattern: each `*` becomes `%`, and the literal `LIKE`
/// metacharacters `\`, `%`, and `_` (underscores are common in tags) are escaped,
/// for use with `... LIKE ?x ESCAPE '\'`.
fn like_glob(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len() + 1);
    for ch in glob.chars() {
        match ch {
            '*' => out.push('%'),
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Escape the `LIKE` metacharacters `\`, `%`, `_` so user text is matched
/// literally (for use with `... LIKE ?x ESCAPE '\'`). Unlike `like_glob`, `*` is
/// left literal — completion is a prefix/substring match, not a glob.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 1);
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Strip double-quote phrase delimiters from a completion fragment and trim,
/// mirroring `core::tokenize` which treats `"` as grouping-only rather than a
/// literal character. So `"some` and `creator:"some` become `some` and
/// `creator:some`, making quoted multi-word tags discoverable via typeahead. A
/// bare or unterminated quote collapses to its inner text. (#49)
fn strip_completion_quotes(s: &str) -> String {
    s.replace('"', "").trim().to_string()
}

/// Split a (possibly partial) completion token the same way [`Tag::parse`]
/// decides namespace vs subtag, without failing on empties.
///
/// `None` namespace means "match the subtag across all namespaces". This
/// mirrors [`Tag::parse`]'s leading-colon rule (#77): a token that starts
/// with `:` is always unnamespaced, regardless of further colons. The
/// returned subtag is normalized (whitespace collapsed, lowercased) so the
/// LIKE pattern is consistent with what Tag::parse would produce.
fn split_completion_token(token: &str) -> (Option<&str>, std::borrow::Cow<'_, str>) {
    if let Some(rest) = token.strip_prefix(':') {
        if rest.contains(':') {
            // e.g. "::)" typed → match subtag ":)" (the double-colon form)
            (None, tag_normalize(rest).into())
        } else {
            // e.g. ":)" typed → match subtag ":)"
            (None, tag_normalize(token).into())
        }
    } else {
        match token.split_once(':') {
            Some((n, s)) => (Some(n), tag_normalize(s).into()),
            None => (None, tag_normalize(token).into()),
        }
    }
}

/// In-memory equivalent of the completion `LIKE` scan for a single tag name:
/// namespace-exact when the fragment is namespaced, subtag prefix/substring per
/// `mode`. `sub` is already normalized and stored subtags are normalized, so a
/// byte-level `starts_with`/`contains` matches the SQL semantics.
fn fragment_matches(name: &Tag, ns: Option<&str>, sub: &str, mode: CompletionMode) -> bool {
    if let Some(ns) = ns {
        if name.namespace != ns {
            return false;
        }
    }
    match mode {
        CompletionMode::Prefix => name.subtag.starts_with(sub),
        CompletionMode::Substring => name.subtag.contains(sub),
    }
}

/// Current time as Unix seconds.
fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_core::{bucket_upper, hash_bytes};
    use std::path::PathBuf;

    fn rec(content: &[u8], path: &str) -> FileRecord {
        FileRecord::new(
            hash_bytes(content),
            PathBuf::from(path),
            content.len() as u64,
            Some(42),
        )
        .with_created_at(Some(41))
    }

    fn rec_with_hash(hash: Hash, name: &str) -> FileRecord {
        FileRecord::new(hash, PathBuf::from(format!("/lib/{name}.jpg")), 1, Some(1))
    }

    /// Produce a distinct [`Hash`] for each small integer, useful for seeding
    /// test files without constructing real content.
    fn h(n: u8) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[0] = n;
        Hash::from_bytes(bytes)
    }

    // ── sha256_seq test constants ─────────────────────────────────────────────
    const SHA_A: &str = "aa00000000000000000000000000000000000000000000000000000000000000";
    const SHA_B: &str = "bb00000000000000000000000000000000000000000000000000000000000000";
    const SHA_C: &str = "cc00000000000000000000000000000000000000000000000000000000000000";

    /// Convenience helper: add a shared service and return its id. Mirrors the
    /// setup used in sibling delta-merge tests.
    fn make_service(db: &Db) -> i64 {
        db.add_shared_service("ptr", "http://repo", None).unwrap()
    }

    #[test]
    fn migrations_are_valid() {
        assert!(MIGRATIONS.validate().is_ok());
    }

    #[test]
    fn vacuum_into_copies_rows_and_rejects_existing_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("source.db");
        let dest_path = tmp.path().join("backup.db");

        let db = Db::open(&db_path).unwrap();
        // Insert a root and a shared service so there are meaningful rows to verify.
        db.add_root(Path::new("/lib/photos")).unwrap();
        db.add_shared_service("ptr", "http://repo", None).unwrap();

        // Backup succeeds on a fresh destination.
        db.vacuum_into(&dest_path).unwrap();
        assert!(dest_path.exists(), "backup file must be created");

        // Reopen the backup read-only and verify the data made it across.
        let backup = Db::open_readonly(&dest_path).unwrap();
        let roots = backup.list_roots().unwrap();
        assert!(
            roots.iter().any(|r| r == Path::new("/lib/photos")),
            "backup must contain the root that was added before the vacuum"
        );

        // A second call must fail because the destination already exists.
        let err = db.vacuum_into(&dest_path).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected 'already exists' error, got: {err}"
        );
    }

    #[test]
    fn relation_cursor_is_null_until_set() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://x", None).unwrap();
        assert_eq!(db.relation_cursor(svc).unwrap(), None);
    }

    #[test]
    fn mapping_cursor_and_marker_start_null_and_can_be_stored() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://repo", None).unwrap();
        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), None);
        assert_eq!(db.last_pull_file_marker(svc, "blake3").unwrap(), None);
        db.set_mapping_pull_state(svc, "blake3", 12, 99).unwrap();
        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), Some(12));
        assert_eq!(db.last_pull_file_marker(svc, "blake3").unwrap(), Some(99));
        db.clear_mapping_pull_state(svc, "blake3").unwrap();
        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), None);
        assert_eq!(db.last_pull_file_marker(svc, "blake3").unwrap(), None);
    }

    #[test]
    fn roots_add_list_remove() {
        let db = Db::open_in_memory().unwrap();

        // add is idempotent
        db.add_root(Path::new("/lib/a")).unwrap();
        db.add_root(Path::new("/lib/a")).unwrap();
        db.add_root(Path::new("/lib/b")).unwrap();

        let roots = db.list_roots().unwrap();
        assert_eq!(
            roots,
            vec![PathBuf::from("/lib/a"), PathBuf::from("/lib/b")]
        );

        // remove returns whether a row went away
        assert!(db.remove_root(Path::new("/lib/a")).unwrap());
        assert!(!db.remove_root(Path::new("/lib/a")).unwrap());
        assert_eq!(db.list_roots().unwrap(), vec![PathBuf::from("/lib/b")]);
    }

    #[test]
    fn mark_missing_path_hits_exact_and_descendants_only() {
        let db = Db::open_in_memory().unwrap();

        let seed = |p: &str, bytes: &[u8]| {
            db.insert_file(&rec(bytes, p), db.next_scan_marker().unwrap())
                .unwrap();
        };
        seed("/a/b", b"one"); // exact target (a file literally named b)
        seed("/a/b/c.txt", b"two"); // descendant of /a/b
        seed("/a/bc", b"three"); // sibling sharing a prefix but NOT a path boundary

        let n = db.mark_missing_path(Path::new("/a/b")).unwrap();
        assert_eq!(
            n, 2,
            "exact /a/b and its descendant /a/b/c.txt, but not /a/bc"
        );

        let present = |bytes: &[u8]| db.locations_of(&hash_bytes(bytes)).unwrap()[0].present;
        assert!(!present(b"one")); // /a/b       -> missing
        assert!(!present(b"two")); // /a/b/c.txt -> missing
        assert!(present(b"three")); // /a/bc     -> still present
    }

    #[test]
    fn tag_schema_applies_with_seeded_local_service() {
        let db = Db::open_in_memory().unwrap();
        // Seeded local service exists with the expected identity.
        let (id, name, scope): (i64, String, String) = db
            .conn
            .query_row("SELECT id, name, scope FROM services", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((id, name.as_str(), scope.as_str()), (1, "my tags", "local"));
        // Every tag table exists and starts empty.
        for table in ["tags", "mappings", "tag_siblings", "tag_parents"] {
            let n: i64 = db
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} should start empty");
        }
        // The interop column exists on files (NULL by default).
        let sha: Option<String> = db
            .conn
            .query_row("SELECT sha256 FROM files LIMIT 1", [], |r| r.get(0))
            .optional()
            .unwrap()
            .flatten();
        assert_eq!(sha, None);
    }

    #[test]
    fn shared_service_is_created_and_looked_up_by_name() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .add_shared_service("ptr", "http://127.0.0.1:9090", None)
            .unwrap();
        assert!(
            id > 1,
            "shared service id is distinct from the seeded local one"
        );

        let found = db.shared_service_by_name("ptr").unwrap().unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.name, "ptr");
        assert_eq!(found.url, "http://127.0.0.1:9090");

        assert!(db.shared_service_by_name("absent").unwrap().is_none());

        let all = db.list_shared_services().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "ptr");
    }

    #[test]
    fn owned_hashes_lists_every_files_row() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let a = hash_bytes(b"a");
        let b = hash_bytes(b"b");
        db.insert_file(&FileRecord::new(a, "/lib/a".into(), 1, Some(1)), 1)
            .unwrap();
        db.insert_file(&FileRecord::new(b, "/lib/b".into(), 1, Some(1)), 1)
            .unwrap();
        let mut got = db.owned_hashes().unwrap();
        got.sort_by_key(|h| h.to_hex());
        let mut want = vec![a, b];
        want.sort_by_key(|h| h.to_hex());
        assert_eq!(got, want);
    }

    #[test]
    fn merge_pulled_mappings_tags_owned_files_skips_unowned_and_is_idempotent() {
        use naiad_core::{Hash, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let service_id = db.add_shared_service("ptr", "http://x", None).unwrap();

        // The library owns exactly one file, at hash H.
        let owned: Hash = hash_bytes(b"owned-bytes");
        db.insert_file(
            &naiad_core::FileRecord::new(owned, "/lib/a.txt".into(), 11, Some(1)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let owned_file_id = db.file_id_by_hash(&owned).unwrap().unwrap();

        // Snapshot carries tags for the owned hash AND an unowned hash.
        let unowned: Hash = hash_bytes(b"not-in-library");
        let entries = vec![
            (
                owned,
                vec![
                    Tag::parse("character:samus").unwrap(),
                    Tag::parse("series:metroid").unwrap(),
                ],
            ),
            (unowned, vec![Tag::parse("creator:nintendo").unwrap()]),
        ];

        let stats = db.merge_pulled_mappings(service_id, &entries).unwrap();
        assert_eq!(stats.matched_files, 1, "only the owned hash matches");
        assert_eq!(stats.mappings, 2, "two tags stored for the owned file");

        // The owned file now carries the two pulled tags (raw stored mappings).
        let tags = db.tags_of(owned_file_id).unwrap();
        let texts: Vec<String> = tags.iter().map(ToString::to_string).collect();
        assert!(texts.contains(&"character:samus".to_string()));
        assert!(texts.contains(&"series:metroid".to_string()));
        assert!(
            !texts.contains(&"creator:nintendo".to_string()),
            "tags for an unowned hash are never stored"
        );

        // Re-pulling the same snapshot: authoritative replace keeps 2 mappings.
        let again = db.merge_pulled_mappings(service_id, &entries).unwrap();
        assert_eq!(again.matched_files, 1);
        assert_eq!(again.mappings, 2, "re-pull holds the same two mappings");
    }

    #[test]
    fn pull_merge_replaces_authoritatively_and_records_author() {
        use naiad_core::{FileRecord, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let hash = hash_bytes(b"owned");
        db.insert_file(
            &FileRecord::new(hash, "/lib/a.txt".into(), 5, Some(1)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let file_id = db.file_id_by_hash(&hash).unwrap().unwrap();
        let svc = db.add_shared_service("ptr", "http://x", None).unwrap();

        let entries = vec![(
            hash,
            vec![
                Tag::parse("character:samus").unwrap(),
                Tag::parse("series:metroid").unwrap(),
            ],
        )];
        let stats = db.merge_pulled_mappings(svc, &entries).unwrap();
        assert_eq!(stats.matched_files, 1);
        assert_eq!(stats.mappings, 2);

        let entries = vec![(hash, vec![Tag::parse("character:samus").unwrap()])];
        db.merge_pulled_mappings(svc, &entries).unwrap();
        let tags: Vec<String> = db
            .tags_of(file_id)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            tags,
            vec!["character:samus".to_string()],
            "replace pruned the removed tag"
        );
    }

    #[test]
    fn merge_mapping_delta_upserts_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://repo", None).unwrap();
        let h = hash_bytes(b"owned");
        db.insert_file(&rec_with_hash(h, "a"), db.next_scan_marker().unwrap())
            .unwrap();
        let tag = Tag::parse("series:metroid").unwrap();
        let input = MappingDeltaInput {
            hash: h,
            tag: tag.clone(),
            status: MappingDeltaStatus::Current,
            seq: 1,
            origin: None,
        };
        db.merge_mapping_delta(svc, "blake3", &[input], &[], 1, db.max_file_id().unwrap())
            .unwrap();
        let file = db.file_id_by_hash(&h).unwrap().unwrap();
        assert_eq!(db.tags_of(file).unwrap(), vec![tag.clone()]);

        // A second delta for the same key is idempotent (upsert).
        let update = MappingDeltaInput {
            hash: h,
            tag: tag.clone(),
            status: MappingDeltaStatus::Current,
            seq: 2,
            origin: None,
        };
        db.merge_mapping_delta(svc, "blake3", &[update], &[], 2, db.max_file_id().unwrap())
            .unwrap();
        assert_eq!(db.tags_of(file).unwrap(), vec![tag.clone()]);

        // A Deleted delta removes the mapping.
        let delete = MappingDeltaInput {
            hash: h,
            tag,
            status: MappingDeltaStatus::Deleted,
            seq: 3,
            origin: None,
        };
        db.merge_mapping_delta(svc, "blake3", &[delete], &[], 3, db.max_file_id().unwrap())
            .unwrap();
        assert!(db.tags_of(file).unwrap().is_empty());
    }

    #[test]
    fn merge_mapping_delta_bucket_scoped_replace_only_deletes_matching_bucket() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://repo", None).unwrap();
        let lower = Hash::from_bytes([0x10; 32]);
        let upper = Hash::from_bytes([0x90; 32]);
        db.insert_file(
            &rec_with_hash(lower, "lower"),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.insert_file(
            &rec_with_hash(upper, "upper"),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.merge_pulled_mappings(
            svc,
            &[
                (lower, vec![Tag::parse("old:lower").unwrap()]),
                (upper, vec![Tag::parse("old:upper").unwrap()]),
            ],
        )
        .unwrap();

        let lo = "10".to_string() + &"00".repeat(31);
        let hi = "20".to_string() + &"00".repeat(31);
        db.merge_mapping_delta(
            svc,
            "blake3",
            &[],
            &[(lo, hi)],
            4,
            db.max_file_id().unwrap(),
        )
        .unwrap();

        let lower_file = db.file_id_by_hash(&lower).unwrap().unwrap();
        let upper_file = db.file_id_by_hash(&upper).unwrap().unwrap();
        assert!(db.tags_of(lower_file).unwrap().is_empty());
        assert_eq!(db.tags_of(upper_file).unwrap()[0].to_string(), "old:upper");
    }

    #[test]
    fn merge_mapping_delta_sha256_clear_preserves_blake3_bit() {
        let db = test_db();
        // A file with both a blake3 identity and a sha256 in the same bucket range.
        db.insert_file(
            &rec_with_hash(h(1), "a").with_sha256(SHA_A.to_string()),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let svc = make_service(&db);
        let tag_id = db.intern_tag(&"foo".parse().unwrap()).unwrap();
        // Row currently supplied by BOTH domains (mask = 3).
        db.raw_conn_for_test()
            .execute(
                "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at, domains)
             SELECT id, ?2, ?3, 'current', 0, 3 FROM files WHERE blake3 = ?1",
                rusqlite::params![h(1).to_hex(), tag_id, svc],
            )
            .unwrap();
        // SHA-256 full-bucket clear over SHA_A's bucket, supplying NO changes for it.
        let lo = bucket_key(&SHA_A.parse::<Hash>().unwrap(), 8);
        let hi = bucket_upper(&SHA_A.parse::<Hash>().unwrap(), 8);
        db.merge_mapping_delta(svc, "sha256", &[], &[(lo, hi)], 5, 0)
            .unwrap();
        let mask: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT domains FROM mappings WHERE service_id = ?1",
                [svc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            mask,
            domain_bit("blake3"),
            "sha256 clear must retract only the sha256 bit, leaving blake3"
        );
    }

    #[test]
    fn new_owned_bucket_keys_after_marker_uses_files_id() {
        let db = Db::open_in_memory().unwrap();
        let h1 = Hash::from_bytes([0x10; 32]);
        db.insert_file(&rec_with_hash(h1, "one"), db.next_scan_marker().unwrap())
            .unwrap();
        let marker = db.max_file_id().unwrap();
        let h2 = Hash::from_bytes([0x90; 32]);
        db.insert_file(&rec_with_hash(h2, "two"), db.next_scan_marker().unwrap())
            .unwrap();
        let keys = db.owned_bucket_keys_after_file_id(1, marker).unwrap();
        assert_eq!(keys, vec![bucket_key(&h2, 1)]);
    }

    #[test]
    fn drop_service_purges_its_mappings_and_the_row() {
        use naiad_core::{Hash, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let service_id = db.add_shared_service("ptr", "http://x", None).unwrap();
        let owned: Hash = hash_bytes(b"owned-bytes");
        db.insert_file(
            &naiad_core::FileRecord::new(owned, "/lib/a.txt".into(), 11, Some(1)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.merge_pulled_mappings(service_id, &[(owned, vec![Tag::parse("x:y").unwrap()])])
            .unwrap();
        let file_id = db.file_id_by_hash(&owned).unwrap().unwrap();
        assert_eq!(db.tags_of(file_id).unwrap().len(), 1);

        db.drop_service(service_id).unwrap();

        assert!(db.shared_service_by_name("ptr").unwrap().is_none());
        assert!(
            db.tags_of(file_id).unwrap().is_empty(),
            "the service's mappings are purged with it"
        );
    }

    #[test]
    fn merge_pulled_relations_stores_and_authoritatively_replaces() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("ptr", "http://x", None).unwrap();
        let svc = db.shared_service_by_name("ptr").unwrap().unwrap().id;

        let sib = |a: &str, b: &str| {
            (
                Tag::parse(a).unwrap(),
                Tag::parse(b).unwrap(),
                "ab".repeat(32),
            )
        };

        // First pull: one sibling, one parent.
        let stats = db
            .merge_pulled_relations(
                svc,
                &[sib("character:samus_aran", "character:samus")],
                &[sib("character:samus", "series:metroid")],
            )
            .unwrap();
        assert_eq!(stats.siblings, 1);
        assert_eq!(stats.parents, 1);
        assert_eq!(db.list_siblings(svc).unwrap().len(), 1);
        assert_eq!(db.list_parents(svc).unwrap().len(), 1);

        // Second pull with the sibling removed: authoritative replace drops it.
        let stats = db
            .merge_pulled_relations(svc, &[], &[sib("character:samus", "series:metroid")])
            .unwrap();
        assert_eq!(stats.siblings, 0);
        assert!(
            db.list_siblings(svc).unwrap().is_empty(),
            "removed edge gone"
        );
        assert_eq!(db.list_parents(svc).unwrap().len(), 1, "parent survives");
    }

    #[test]
    fn merge_pulled_relations_stamps_last_pull_and_leaves_others_null() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        let pulled = db.add_shared_service("ptr", "http://x", None).unwrap();
        let other = db.add_shared_service("other", "http://y", None).unwrap();

        let author = "aa".repeat(32);
        db.merge_pulled_relations(
            pulled,
            &[(
                Tag::parse("samus").unwrap(),
                Tag::parse("character:samus").unwrap(),
                author.clone(),
            )],
            &[],
        )
        .unwrap();

        // The pulled service got a timestamp; an untouched service stays NULL.
        assert!(db.last_relation_pull_at(pulled).unwrap().is_some());
        assert!(db.last_relation_pull_at(other).unwrap().is_none());
    }

    #[test]
    fn list_relation_edges_returns_all_edges_with_provenance() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ptr = db.add_shared_service("ptr", "http://x", None).unwrap();

        // A local sibling (author NULL) ...
        let bad = db.intern_tag(&Tag::parse("sammus").unwrap()).unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, local).unwrap();

        // ... and a pulled sibling + parent (author set) on ptr.
        let author = "aa".repeat(32);
        db.merge_pulled_relations(
            ptr,
            &[(
                Tag::parse("samus").unwrap(),
                Tag::parse("character:samus").unwrap(),
                author.clone(),
            )],
            &[(
                Tag::parse("character:samus").unwrap(),
                Tag::parse("series:metroid").unwrap(),
                author.clone(),
            )],
        )
        .unwrap();

        let edges = db.list_relation_edges().unwrap();
        assert_eq!(edges.len(), 3, "two siblings + one parent; got {edges:?}");

        // Ordered kind, then service, then from-tag: parents before siblings.
        assert_eq!(edges[0].kind, EdgeKind::Parent);
        assert_eq!(edges[0].service, "ptr");
        assert_eq!(edges[0].author.as_deref(), Some(author.as_str()));

        // The local sibling carries no author; the pulled one carries the hex.
        // (The seeded local service is named "my tags" — see 0002_tags.sql.)
        let local_sib = edges
            .iter()
            .find(|e| e.kind == EdgeKind::Sibling && e.service == "my tags")
            .unwrap();
        assert_eq!(local_sib.from.to_string(), "sammus");
        assert_eq!(local_sib.author, None);

        let pulled_sib = edges
            .iter()
            .find(|e| e.kind == EdgeKind::Sibling && e.service == "ptr")
            .unwrap();
        assert_eq!(pulled_sib.author.as_deref(), Some(author.as_str()));
    }

    #[test]
    fn relation_status_counts_edges_per_service() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        let ptr = db.add_shared_service("ptr", "http://x", None).unwrap();

        let author = "aa".repeat(32);
        db.merge_pulled_relations(
            ptr,
            &[(
                Tag::parse("samus").unwrap(),
                Tag::parse("character:samus").unwrap(),
                author.clone(),
            )],
            &[(
                Tag::parse("character:samus").unwrap(),
                Tag::parse("series:metroid").unwrap(),
                author,
            )],
        )
        .unwrap();

        let status = db.relation_status().unwrap();
        // Every service is reported, local first (lowest id). The seeded local
        // service is named "my tags" (0002_tags.sql).
        assert_eq!(status[0].service, "my tags");
        assert_eq!(status[0].siblings, 0);
        assert_eq!(status[0].parents, 0);
        assert_eq!(status[0].last_pull, None);

        let ptr_row = status.iter().find(|s| s.service == "ptr").unwrap();
        assert_eq!(ptr_row.siblings, 1);
        assert_eq!(ptr_row.parents, 1);
        assert!(ptr_row.last_pull.is_some());
    }

    #[test]
    fn merge_pulled_relations_collapses_conflicting_siblings_and_skips_self_edges() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("ptr", "http://x", None).unwrap();
        let svc = db.shared_service_by_name("ptr").unwrap().unwrap().id;

        let edge = |a: &str, b: &str| {
            (
                Tag::parse(a).unwrap(),
                Tag::parse(b).unwrap(),
                "ab".repeat(32),
            )
        };

        // Same `from`, two ideals → keep the lexicographically-smallest `to`.
        // Plus a self-edge that must be skipped.
        let stats = db
            .merge_pulled_relations(
                svc,
                &[
                    edge("samus", "character:samus_aran"),
                    edge("samus", "character:samus"),
                    edge("self:tag", "self:tag"),
                ],
                &[],
            )
            .unwrap();
        assert_eq!(
            stats.siblings, 1,
            "one collapsed sibling, self-edge skipped"
        );

        let sibs = db.list_siblings(svc).unwrap();
        assert_eq!(sibs.len(), 1);
        // "character:samus" < "character:samus_aran" lexicographically.
        assert_eq!(sibs[0].1, Tag::parse("character:samus").unwrap());
    }

    #[test]
    fn merged_sibling_applies_through_effective_tags() {
        use naiad_core::Tag;
        // Proves the stored shared-service edges are shaped so a future display
        // merge lights up: load the edges and resolve through them directly.
        let db = Db::open_in_memory().unwrap();
        db.add_shared_service("ptr", "http://x", None).unwrap();
        let svc = db.shared_service_by_name("ptr").unwrap().unwrap().id;

        let alias = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();

        db.merge_pulled_relations(
            svc,
            &[(
                Tag::parse("samus").unwrap(),
                Tag::parse("character:samus").unwrap(),
                "ab".repeat(32),
            )],
            &[],
        )
        .unwrap();

        let sib_edges = db.load_sibling_edges(svc).unwrap();
        let par_edges = db.load_parent_edges(svc).unwrap();
        let canon = naiad_core::canonicalize(alias, &sib_edges);
        assert_eq!(canon, ideal, "alias canonicalizes to the ideal");
        // effective_tags over the canonicalized input expands parents (none here).
        let eff = naiad_core::effective_tags(&[alias], &sib_edges, &par_edges);
        assert!(eff.contains(&ideal), "ideal is in the effective set");
    }

    #[test]
    fn insert_list_get_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"alpha", "/lib/a.png"), m).unwrap();
        db.insert_file(&rec(b"beta", "/lib/b.png"), m).unwrap();

        assert_eq!(db.file_count().unwrap(), 2);

        let listed = db.list_files().unwrap();
        assert_eq!(listed.len(), 2);
        // Ordered by path: a.png before b.png.
        assert_eq!(listed[0].path, PathBuf::from("/lib/a.png"));
        assert_eq!(listed[0].hash, hash_bytes(b"alpha"));
        assert_eq!(listed[0].size, 5);
        assert_eq!(listed[0].modified_at, Some(42));
        assert_eq!(listed[0].created_at, Some(41));
        assert!(listed[0].imported_at > 0);
        assert_eq!(listed[0].mime, None);

        let got = db.get_by_hash(&hash_bytes(b"alpha")).unwrap().unwrap();
        assert_eq!(got.hash, hash_bytes(b"alpha"));
        assert_eq!(got.size, 5);
        assert!(db.get_by_hash(&hash_bytes(b"missing")).unwrap().is_none());
    }

    #[test]
    fn new_content_defaults_to_active_state() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"x", "/lib/x.png"), m).unwrap();
        let got = db.get_by_hash(&hash_bytes(b"x")).unwrap().unwrap();
        assert_eq!(got.state, FileState::Active);
        assert_eq!(got.mime, None);
        assert_eq!(got.width, None);
    }

    #[test]
    fn reimport_same_path_does_not_duplicate() {
        let db = Db::open_in_memory().unwrap();
        let r = rec(b"same", "/lib/p.png");
        let updated =
            FileRecord::new(r.hash, r.path.clone(), r.size, r.mtime).with_created_at(Some(99));
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&r, m).unwrap();
        db.insert_file(&updated, m).unwrap();

        assert_eq!(db.file_count().unwrap(), 1);
        let locations = db.locations_of(&r.hash).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].created_at, Some(99));
    }

    #[test]
    fn locations_of_exposes_created_at() {
        let db = Db::open_in_memory().unwrap();
        let r = rec(b"created", "/lib/created.png");
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&r, m).unwrap();

        let locations = db.locations_of(&r.hash).unwrap();
        assert_eq!(locations[0].created_at, Some(41));
    }

    #[test]
    fn same_content_two_paths_is_one_file_two_locations() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"dup", "/lib/one.png"), m).unwrap();
        db.insert_file(&rec(b"dup", "/lib/two.png"), m).unwrap();

        // One content row...
        assert_eq!(db.file_count().unwrap(), 1);
        // ...with two locations.
        let mut locs = db.locations_of(&hash_bytes(b"dup")).unwrap();
        locs.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[0].path, PathBuf::from("/lib/one.png"));
        assert_eq!(locs[1].path, PathBuf::from("/lib/two.png"));
        assert!(locs.iter().all(|l| l.present));
    }

    #[test]
    fn update_metadata_sets_columns() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"img", "/lib/p.gif"), m).unwrap();

        let meta = FileMetadata {
            mime: Some("image/gif".to_string()),
            width: Some(4),
            height: Some(7),
        };
        db.update_metadata(&hash_bytes(b"img"), &meta).unwrap();

        let got = db.get_by_hash(&hash_bytes(b"img")).unwrap().unwrap();
        assert_eq!(got.mime.as_deref(), Some("image/gif"));
        assert_eq!(got.width, Some(4));
        assert_eq!(got.height, Some(7));
    }

    #[test]
    fn list_files_exposes_mime_after_metadata_update() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"img", "/lib/p.gif"), m).unwrap();
        let meta = FileMetadata {
            mime: Some("image/gif".to_string()),
            width: Some(4),
            height: Some(7),
        };
        db.update_metadata(&hash_bytes(b"img"), &meta).unwrap();

        let listed = db.list_files().unwrap();
        assert_eq!(listed[0].mime.as_deref(), Some("image/gif"));
    }

    #[test]
    fn mark_missing_flips_unseen_locations_but_keeps_content() {
        let db = Db::open_in_memory().unwrap();
        // First scan: both files seen, stamped with marker1.
        let marker1 = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"keep", "/lib/keep.png"), marker1)
            .unwrap();
        db.insert_file(&rec(b"gone", "/lib/gone.png"), marker1)
            .unwrap();

        // Second scan: only "keep" is seen again, stamped with marker2 > marker1.
        let marker2 = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"keep", "/lib/keep.png"), marker2)
            .unwrap();

        let marked = db
            .mark_missing_under_before(Path::new("/lib"), marker2)
            .unwrap();
        assert_eq!(marked, 1); // only gone.png was stale

        // Content rows survive; count unchanged.
        assert_eq!(db.file_count().unwrap(), 2);

        let gone = db.locations_of(&hash_bytes(b"gone")).unwrap();
        assert_eq!(gone.len(), 1);
        assert!(!gone[0].present); // marked missing, not deleted

        let keep = db.locations_of(&hash_bytes(b"keep")).unwrap();
        assert!(keep[0].present); // still present
    }

    #[test]
    fn mark_missing_under_before_ignores_other_roots() {
        let db = Db::open_in_memory().unwrap();
        // A file under /other was last seen long ago (marker1) and never
        // rescanned — but it lives outside the root being reconciled.
        let marker1 = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"other", "/other/keep.png"), marker1)
            .unwrap();
        // A stale file under /lib (also marker1, never re-seen).
        db.insert_file(&rec(b"stale", "/lib/gone.png"), marker1)
            .unwrap();

        // Reconcile only /lib at a newer marker.
        let marker2 = db.next_scan_marker().unwrap();
        let marked = db
            .mark_missing_under_before(Path::new("/lib"), marker2)
            .unwrap();
        assert_eq!(marked, 1, "only the stale /lib location should flip");

        // /lib's stale file flips; /other's older file is untouched.
        assert!(!db.locations_of(&hash_bytes(b"stale")).unwrap()[0].present);
        assert!(
            db.locations_of(&hash_bytes(b"other")).unwrap()[0].present,
            "a location outside the scanned root must not be reconciled"
        );
    }

    #[test]
    fn present_fingerprints_snapshots_size_and_mtime_of_present_only() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        // Two present files (different content/size) and one that goes missing.
        db.insert_file(&rec(b"alpha", "/lib/a.png"), m).unwrap();
        db.insert_file(&rec(b"betabeta", "/lib/b.png"), m).unwrap();
        db.insert_file(&rec(b"gone", "/lib/gone.png"), m).unwrap();
        db.mark_missing_path(Path::new("/lib/gone.png")).unwrap();

        let fps = db.present_fingerprints().unwrap();
        assert_eq!(fps.len(), 2, "missing locations are excluded");
        // `rec(bytes, path)` stores size = bytes.len(); mtime is the rec's mtime.
        assert_eq!(fps.get(Path::new("/lib/a.png")).unwrap().0, 5);
        assert_eq!(fps.get(Path::new("/lib/b.png")).unwrap().0, 8);
        assert!(!fps.contains_key(Path::new("/lib/gone.png")));
    }

    #[test]
    fn touch_location_restamps_only_when_stale() {
        let db = Db::open_in_memory().unwrap();
        let m1 = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"x", "/lib/x.png"), m1).unwrap();

        let m2 = db.next_scan_marker().unwrap();
        assert!(
            db.touch_location(Path::new("/lib/x.png"), m2, Some(41))
                .unwrap()
        );
        // Already at m2: a second touch at m2 is a no-op (last_seen < marker fails).
        assert!(
            !db.touch_location(Path::new("/lib/x.png"), m2, Some(41))
                .unwrap()
        );
        // A path we don't have never matches.
        assert!(
            !db.touch_location(Path::new("/lib/nope.png"), m2, Some(41))
                .unwrap()
        );

        // A previously-missing location comes back present on touch.
        db.mark_missing_path(Path::new("/lib/x.png")).unwrap();
        let m3 = db.next_scan_marker().unwrap();
        assert!(
            db.touch_location(Path::new("/lib/x.png"), m3, Some(41))
                .unwrap()
        );
        assert!(db.locations_of(&hash_bytes(b"x")).unwrap()[0].present);
    }

    #[test]
    fn touch_location_backfills_missing_created_at() {
        let db = Db::open_in_memory().unwrap();
        let m1 = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"x", "/lib/x.png"), m1).unwrap();
        db.conn
            .execute("UPDATE file_locations SET created_at = NULL", [])
            .unwrap();

        let m2 = db.next_scan_marker().unwrap();
        assert!(
            db.touch_location(Path::new("/lib/x.png"), m2, Some(77))
                .unwrap()
        );
        assert_eq!(
            db.locations_of(&hash_bytes(b"x")).unwrap()[0].created_at,
            Some(77)
        );

        let m3 = db.next_scan_marker().unwrap();
        assert!(
            db.touch_location(Path::new("/lib/x.png"), m3, Some(88))
                .unwrap()
        );
        assert_eq!(
            db.locations_of(&hash_bytes(b"x")).unwrap()[0].created_at,
            Some(77),
            "existing creation time is preserved"
        );
    }

    #[test]
    fn touch_location_uses_the_path_index() {
        // The startup rescan touches every file by path; without a path-only
        // index this UPDATE full-scans file_locations and a 100k-file library
        // takes ~30 minutes per launch (#65).
        let db = Db::open_in_memory().unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {}", Db::TOUCH_LOCATION_SQL);
        let mut stmt = db.conn.prepare(&sql).unwrap();
        let plan: Vec<String> = stmt
            .query_map(
                // dummy binds — EXPLAIN QUERY PLAN never evaluates them
                params![vec![0u8; 2], 1i64, 2i64],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("idx_locations_path"),
            "touch_location must use the path index, got plan:\n{plan}"
        );
    }

    #[test]
    fn intern_tag_dedupes() {
        let db = Db::open_in_memory().unwrap();
        let t = Tag::parse("character:samus").unwrap();
        let id1 = db.intern_tag(&t).unwrap();
        let id2 = db.intern_tag(&t).unwrap();
        assert_eq!(id1, id2);
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn resolves_file_by_hash_and_path() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"alpha", "/lib/a.png"), m).unwrap();

        let by_hash = db.file_id_by_hash(&hash_bytes(b"alpha")).unwrap();
        assert!(by_hash.is_some());
        let by_path = db
            .file_id_by_path(std::path::Path::new("/lib/a.png"))
            .unwrap();
        assert_eq!(by_path, by_hash);

        assert!(db.file_id_by_hash(&hash_bytes(b"nope")).unwrap().is_none());
        assert!(
            db.file_id_by_path(std::path::Path::new("/lib/nope.png"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn add_list_remove_mapping() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"alpha", "/lib/a.png"), m).unwrap();
        let file_id = db.file_id_by_hash(&hash_bytes(b"alpha")).unwrap().unwrap();
        let svc = db.local_service_id().unwrap();
        assert_eq!(svc, 1);

        let samus = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let creator = db
            .intern_tag(&Tag::parse("creator:nintendo").unwrap())
            .unwrap();
        db.add_mapping(file_id, samus, svc).unwrap();
        db.add_mapping(file_id, creator, svc).unwrap();
        db.add_mapping(file_id, samus, svc).unwrap(); // idempotent

        let shown: Vec<String> = db
            .tags_of(file_id)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        // Ordered by namespace, subtag: 'character' < 'creator'.
        assert_eq!(shown, vec!["character:samus", "creator:nintendo"]);

        db.remove_mapping(file_id, creator, svc).unwrap();
        let after: Vec<String> = db
            .tags_of(file_id)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(after, vec!["character:samus"]);
        // Removing an absent mapping is a no-op.
        db.remove_mapping(file_id, creator, svc).unwrap();
    }

    #[test]
    fn one_tag_on_two_files_is_one_tag_row_two_mappings() {
        let db = Db::open_in_memory().unwrap();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec(b"alpha", "/lib/a.png"), m).unwrap();
        db.insert_file(&rec(b"beta", "/lib/b.png"), m).unwrap();
        let fa = db.file_id_by_hash(&hash_bytes(b"alpha")).unwrap().unwrap();
        let fb = db.file_id_by_hash(&hash_bytes(b"beta")).unwrap().unwrap();
        let svc = db.local_service_id().unwrap();

        let shared = db.intern_tag(&Tag::parse("rating:safe").unwrap()).unwrap();
        db.add_mapping(fa, shared, svc).unwrap();
        db.add_mapping(fb, shared, svc).unwrap();

        let ntags: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        let nmaps: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM mappings", [], |r| r.get(0))
            .unwrap();
        assert_eq!((ntags, nmaps), (1, 2));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_persists_round_trip() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let db = Db::open_in_memory().unwrap();
        let weird = PathBuf::from(OsString::from_vec(vec![b'/', b't', 0x80, b'x']));
        let r = FileRecord::new(hash_bytes(b"nb"), weird.clone(), 2, None);
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&r, m).unwrap();

        let locs = db.locations_of(&hash_bytes(b"nb")).unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path, weird);
    }

    #[test]
    fn sibling_add_load_remove_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let bad = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();

        db.add_sibling(bad, ideal, svc).unwrap();
        let edges = db.load_sibling_edges(svc).unwrap();
        assert_eq!(edges.get(&bad), Some(&ideal));

        db.remove_sibling(bad, svc).unwrap();
        assert!(db.load_sibling_edges(svc).unwrap().is_empty());
    }

    #[test]
    fn add_sibling_replaces_prior_ideal() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let bad = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        let ideal1 = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let ideal2 = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();

        db.add_sibling(bad, ideal1, svc).unwrap();
        db.add_sibling(bad, ideal2, svc).unwrap();

        let edges = db.load_sibling_edges(svc).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges.get(&bad), Some(&ideal2));
    }

    #[test]
    fn parent_add_load_remove_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let child = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();

        db.add_parent(child, parent, svc).unwrap();
        let edges = db.load_parent_edges(svc).unwrap();
        assert_eq!(edges.get(&child), Some(&vec![parent]));

        db.remove_parent(child, parent, svc).unwrap();
        assert!(db.load_parent_edges(svc).unwrap().is_empty());
    }

    #[test]
    fn add_parent_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let child = db.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        let parent = db.intern_tag(&Tag::parse("b").unwrap()).unwrap();
        db.add_parent(child, parent, svc).unwrap();
        db.add_parent(child, parent, svc).unwrap();
        assert_eq!(
            db.load_parent_edges(svc).unwrap().get(&child),
            Some(&vec![parent])
        );
    }

    #[test]
    fn parent_accumulates_multiple_parents_for_one_child() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let child = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent1 = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        let parent2 = db
            .intern_tag(&Tag::parse("creator:nintendo").unwrap())
            .unwrap();

        db.add_parent(child, parent1, svc).unwrap();
        db.add_parent(child, parent2, svc).unwrap();

        let edges = db.load_parent_edges(svc).unwrap();
        let mut parents = edges.get(&child).unwrap().clone();
        parents.sort_unstable();
        let mut expected = vec![parent1, parent2];
        expected.sort_unstable();
        assert_eq!(parents, expected);
    }

    #[test]
    fn self_edges_are_rejected() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let t = db.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        assert!(matches!(
            db.add_sibling(t, t, svc),
            Err(Error::SelfRelation)
        ));
        assert!(matches!(db.add_parent(t, t, svc), Err(Error::SelfRelation)));
    }

    #[test]
    fn remove_absent_edge_is_noop() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let t = db.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        db.remove_sibling(t, svc).unwrap();
        db.remove_parent(t, t + 1, svc).unwrap();
    }

    #[test]
    fn display_tags_of_canonicalizes_and_expands() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();

        // A file in the library.
        db.insert_file(&rec(b"img", "a.png"), 1).unwrap();
        let file_id = db
            .file_id_by_path(std::path::Path::new("a.png"))
            .unwrap()
            .unwrap();

        // Relations: samus -> character:samus aran -> series:metroid.
        let bad = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();
        db.add_parent(ideal, parent, svc).unwrap();

        // Raw-tag the file with only the bad tag.
        db.add_mapping(file_id, bad, svc).unwrap();

        // Computed view: the ideal + its implied parent, sorted.
        let display: Vec<Tag> = db
            .display_tags_of(file_id, ReadScope::Merged)
            .unwrap()
            .into_iter()
            .map(|t| t.tag)
            .collect();
        let display: Vec<String> = display.iter().map(ToString::to_string).collect();
        assert_eq!(display, vec!["character:samus aran", "series:metroid"]);

        // Raw view: just the literal mapping.
        let raw: Vec<String> = db
            .tags_of(file_id)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(raw, vec!["samus"]);
    }

    #[test]
    fn display_tags_of_merges_services_with_provenance() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let pulled = db.add_shared_service("p", "http://p/", None).unwrap();

        let r = rec(b"bb", "bb.png");
        db.insert_file(&r, 1).unwrap();
        let file_id = db
            .file_id_by_path(std::path::Path::new("bb.png"))
            .unwrap()
            .unwrap();

        let shared = db.intern_tag(&Tag::parse("shared:tag").unwrap()).unwrap();
        let only_local = db.intern_tag(&Tag::parse("local:tag").unwrap()).unwrap();
        let only_pulled = db.intern_tag(&Tag::parse("pulled:tag").unwrap()).unwrap();
        db.add_mapping(file_id, shared, local).unwrap();
        db.add_mapping(file_id, shared, pulled).unwrap();
        db.add_mapping(file_id, only_local, local).unwrap();
        db.add_mapping(file_id, only_pulled, pulled).unwrap();

        let merged = db.display_tags_of(file_id, ReadScope::Merged).unwrap();
        let by_text = |s: &str| {
            merged
                .iter()
                .find(|t| t.tag.to_string() == s)
                .map(|t| t.presence)
        };
        assert_eq!(by_text("shared:tag"), Some(TagPresence::Both));
        assert_eq!(by_text("local:tag"), Some(TagPresence::Local));
        assert_eq!(by_text("pulled:tag"), Some(TagPresence::Pulled));

        // LocalOnly hides the pulled-only tag and downgrades shared to Local.
        let local_only = db.display_tags_of(file_id, ReadScope::LocalOnly).unwrap();
        assert!(local_only.iter().all(|t| t.tag.to_string() != "pulled:tag"));
        assert_eq!(
            local_only
                .iter()
                .find(|t| t.tag.to_string() == "shared:tag")
                .map(|t| t.presence),
            Some(TagPresence::Local)
        );
    }

    /// Insert a file, return its id. (Local helper for search tests.)
    fn insert_named(db: &Db, content: &[u8], name: &str) -> i64 {
        db.insert_file(&rec(content, name), 1).unwrap();
        db.file_id_by_path(std::path::Path::new(name))
            .unwrap()
            .unwrap()
    }

    fn tag_file(db: &Db, file_id: i64, svc: i64, tag: &str) {
        let id = db.intern_tag(&Tag::parse(tag).unwrap()).unwrap();
        db.add_mapping(file_id, id, svc).unwrap();
    }

    /// The set of result hashes (hex) for a query, for order-independent asserts.
    fn result_hashes(db: &Db, q: &Query) -> std::collections::BTreeSet<String> {
        db.search(q, ReadScope::Merged, Expansion::Expanded)
            .unwrap()
            .iter()
            .map(|f| f.hash.to_string())
            .collect()
    }

    #[test]
    fn tag_id_is_read_only_lookup() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.tag_id(&Tag::parse("nope").unwrap()).unwrap(), None);
        let id = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        assert_eq!(
            db.tag_id(&Tag::parse("character:samus").unwrap()).unwrap(),
            Some(id)
        );
        // Looking up an absent tag did not create a row.
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn search_applies_siblings_and_parents() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");

        // a is raw-tagged with the alias `samus`; b with `creator:nintendo`.
        tag_file(&db, a, svc, "samus");
        tag_file(&db, b, svc, "creator:nintendo");

        // samus -> character:samus aran -> series:metroid
        let bad = db.tag_id(&Tag::parse("samus").unwrap()).unwrap().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();
        db.add_parent(ideal, parent, svc).unwrap();

        let a_hash = db
            .search(
                &Query {
                    predicates: vec![Predicate::Tag(
                        Tag::parse("series:metroid").unwrap(),
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(a_hash.len(), 1);
        assert_eq!(
            a_hash[0].path.file_name().unwrap().to_str().unwrap(),
            "a.png"
        );

        // Searching the alias finds the same file as the canonical.
        assert_eq!(
            result_hashes(
                &db,
                &Query {
                    predicates: vec![Predicate::Tag(
                        Tag::parse("samus").unwrap(),
                        MatchMode::Expanded
                    )]
                }
            ),
            result_hashes(
                &db,
                &Query {
                    predicates: vec![Predicate::Tag(
                        Tag::parse("character:samus aran").unwrap(),
                        MatchMode::Expanded
                    )]
                }
            ),
        );
    }

    #[test]
    fn search_raw_bypasses_sibling_and_parent_expansion() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"raw", "raw.png");

        // f is literally tagged with the alias `samus` only.
        tag_file(&db, f, svc, "samus");

        // samus -> character:samus aran (sibling), character:samus aran -> series:metroid (parent)
        let bad = db.tag_id(&Tag::parse("samus").unwrap()).unwrap().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();
        db.add_parent(ideal, parent, svc).unwrap();

        let q = |t: &str| Query {
            predicates: vec![Predicate::Tag(Tag::parse(t).unwrap(), MatchMode::Expanded)],
        };

        // Expanded: the canonical tag and the implied parent both find f.
        assert_eq!(
            db.search(
                &q("character:samus aran"),
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap()
            .len(),
            1,
            "expanded search resolves the alias to its canonical"
        );
        assert_eq!(
            db.search(&q("series:metroid"), ReadScope::Merged, Expansion::Expanded,)
                .unwrap()
                .len(),
            1,
            "expanded search applies the parent implication"
        );

        // Raw: only the literal alias matches; canonical and parent do not.
        assert!(
            db.search(
                &q("character:samus aran"),
                ReadScope::Merged,
                Expansion::Raw,
            )
            .unwrap()
            .is_empty(),
            "raw search does not follow the sibling chain"
        );
        assert!(
            db.search(&q("series:metroid"), ReadScope::Merged, Expansion::Raw,)
                .unwrap()
                .is_empty(),
            "raw search does not apply parent implication"
        );
        assert_eq!(
            db.search(&q("samus"), ReadScope::Merged, Expansion::Raw)
                .unwrap()
                .len(),
            1,
            "raw search matches the literal tag"
        );
    }

    #[test]
    fn per_term_exact_matches_literally_while_rest_expands() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let lit = insert_named(&db, b"lit", "lit.png");
        let alias = insert_named(&db, b"alias", "alias.png");
        tag_file(&db, lit, svc, "character:samus"); // literal canonical
        tag_file(&db, alias, svc, "samus"); // literal alias

        // sibling: samus -> character:samus
        let bad = db.tag_id(&Tag::parse("samus").unwrap()).unwrap().unwrap();
        let ideal = db
            .tag_id(&Tag::parse("character:samus").unwrap())
            .unwrap()
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();

        let expanded = naiad_core::parse_query(&["character:samus".to_string()]).unwrap();
        let exact = naiad_core::parse_query(&["=character:samus".to_string()]).unwrap();

        // Expanded finds both the literal canonical and the alias-tagged file.
        assert_eq!(
            db.search(&expanded, ReadScope::Merged, Expansion::Expanded,)
                .unwrap()
                .len(),
            2,
            "expanded matches the canonical and, via the sibling, the alias file"
        );
        // Exact finds only the literally character:samus-tagged file.
        assert_eq!(
            db.search(&exact, ReadScope::Merged, Expansion::Expanded)
                .unwrap()
                .len(),
            1,
            "exact matches only the literally-tagged canonical file"
        );
    }

    #[test]
    fn mixed_query_exact_term_is_literal_other_term_expands() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"f", "f.png");
        tag_file(&db, f, svc, "character:samus"); // child only

        // parent: character:samus -> series:metroid
        let child = db
            .tag_id(&Tag::parse("character:samus").unwrap())
            .unwrap()
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_parent(child, parent, svc).unwrap();

        // `=character:samus series:metroid`: 1st literal (f has it), 2nd expanded
        // (f matches series:metroid via the parent edge). Both AND-true -> f.
        let q = naiad_core::parse_query(&[
            "=character:samus".to_string(),
            "series:metroid".to_string(),
        ])
        .unwrap();
        assert_eq!(
            db.search(&q, ReadScope::Merged, Expansion::Expanded)
                .unwrap()
                .len(),
            1,
            "exact child term and expanded parent term both match f"
        );

        // Sanity: `=series:metroid` alone is literal, and f is not literally
        // tagged it (only via the parent), so exact matches nothing.
        let q2 = naiad_core::parse_query(&["=series:metroid".to_string()]).unwrap();
        assert!(
            db.search(&q2, ReadScope::Merged, Expansion::Expanded)
                .unwrap()
                .is_empty(),
            "exact series:metroid finds no literally-tagged file"
        );
    }

    #[test]
    fn search_and_or_not() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        tag_file(&db, a, svc, "x");
        tag_file(&db, a, svc, "y");
        tag_file(&db, b, svc, "y");

        let tag = |t: &str| Predicate::Tag(Tag::parse(t).unwrap(), MatchMode::Expanded);

        // AND: only files with both x and y -> a.
        let r = db
            .search(
                &Query {
                    predicates: vec![tag("x"), tag("y")],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");

        // OR: x or z -> a (z matches nothing).
        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Or(vec![
                        (Tag::parse("x").unwrap(), MatchMode::Expanded),
                        (Tag::parse("z").unwrap(), MatchMode::Expanded),
                    ])],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");

        // NOT: y AND NOT x -> b.
        let r = db
            .search(
                &Query {
                    predicates: vec![
                        tag("y"),
                        Predicate::Not(Tag::parse("x").unwrap(), MatchMode::Expanded),
                    ],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "b.png");

        // Only-negative: NOT x -> b (all files minus those with x).
        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Not(
                        Tag::parse("x").unwrap(),
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "b.png");

        // Absent tag in a positive AND -> no results.
        let r = db
            .search(
                &Query {
                    predicates: vec![tag("x"), tag("absent")],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn search_empty_query_returns_all_files() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let _b = insert_named(&db, b"b", "b.png");
        tag_file(&db, a, svc, "x");
        // No predicates -> the universe seed, with no negations to subtract.
        let r = db
            .search(&Query::default(), ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn read_path_excludes_fully_missing_files() {
        let db = Db::open_in_memory().unwrap();
        let _a = insert_named(&db, b"keep", "keep.png");
        let _b = insert_named(&db, b"gone", "gone.png");

        // Hide gone.png's only location.
        assert_eq!(db.mark_missing_path(Path::new("gone.png")).unwrap(), 1);

        // list_files (the empty-gallery path) drops the all-missing file.
        let listed = db.list_files().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].hash, hash_bytes(b"keep"));

        // The empty-query search path (listings_for) drops it too.
        let r = db
            .search(&Query::default(), ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].hash, hash_bytes(b"keep"));
    }

    #[test]
    fn search_combines_relations_with_negation() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        // Both files tagged with the alias `samus`; b additionally `meta:wip`.
        tag_file(&db, a, svc, "samus");
        tag_file(&db, b, svc, "samus");
        tag_file(&db, b, svc, "meta:wip");

        // samus -> character:samus aran -> series:metroid
        let bad = db.tag_id(&Tag::parse("samus").unwrap()).unwrap().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();
        db.add_parent(ideal, parent, svc).unwrap();

        // series:metroid matches both (via sibling+parent expansion); NOT meta:wip
        // removes b -> only a remains.
        let r = db
            .search(
                &Query {
                    predicates: vec![
                        Predicate::Tag(Tag::parse("series:metroid").unwrap(), MatchMode::Expanded),
                        Predicate::Not(Tag::parse("meta:wip").unwrap(), MatchMode::Expanded),
                    ],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");
    }

    #[test]
    fn wildcard_namespace_is_relation_aware() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        tag_file(&db, a, svc, "samus"); // alias
        tag_file(&db, b, svc, "creator:nintendo");
        // samus -> character:samus aran
        let bad = db.tag_id(&Tag::parse("samus").unwrap()).unwrap().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();

        // character:* matches a (effective character tag via the alias), not b.
        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Wild(
                        TagPattern::NamespaceAny {
                            namespace: "character".into(),
                        },
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");
    }

    #[test]
    fn wildcard_namespace_follows_parents() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        tag_file(&db, a, svc, "costume:zero suit"); // raw child tag
        // costume:zero suit -> character:samus aran (parent/implication)
        let child = db
            .tag_id(&Tag::parse("costume:zero suit").unwrap())
            .unwrap()
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        db.add_parent(child, parent, svc).unwrap();

        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Wild(
                        TagPattern::NamespaceAny {
                            namespace: "character".into(),
                        },
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn wildcard_excludes_cross_namespace_alias() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let e = insert_named(&db, b"e", "e.png");
        tag_file(&db, e, svc, "character:link"); // raw, but a bad alias
        // character:link -> creator:nintendo : its ideal is NOT a character tag.
        let bad = db
            .tag_id(&Tag::parse("character:link").unwrap())
            .unwrap()
            .unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("creator:nintendo").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();

        // character:* must NOT match e (its effective tag is creator:nintendo).
        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Wild(
                        TagPattern::NamespaceAny {
                            namespace: "character".into(),
                        },
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn wildcard_prefix_escapes_underscore() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        tag_file(&db, a, svc, "blue_sky"); // matches prefix "blue_"
        tag_file(&db, b, svc, "bluex"); // must NOT match (underscore is literal)

        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Wild(
                        TagPattern::AnyNamespaceGlob {
                            glob: "blue_*".into(),
                        },
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");
    }

    #[test]
    fn wildcard_matches_leading_and_interior() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        let c = insert_named(&db, b"c", "c.png");
        tag_file(&db, a, svc, "samus aran"); // matches *aran and sam*ran
        tag_file(&db, b, svc, "dark samus"); // matches *samus
        tag_file(&db, c, svc, "metroid"); // matches neither

        let names = |pat: TagPattern| -> Vec<String> {
            let mut v = db
                .search(
                    &Query {
                        predicates: vec![Predicate::Wild(pat, MatchMode::Expanded)],
                    },
                    ReadScope::Merged,
                    Expansion::Expanded,
                )
                .unwrap()
                .into_iter()
                .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
                .collect::<Vec<_>>();
            v.sort();
            v
        };

        // Leading wildcard: every subtag ending in "samus".
        assert_eq!(
            names(TagPattern::AnyNamespaceGlob {
                glob: "*samus".into()
            }),
            vec!["b.png"]
        );
        // Interior wildcard.
        assert_eq!(
            names(TagPattern::AnyNamespaceGlob {
                glob: "sam*ran".into()
            }),
            vec!["a.png"]
        );
        // Surrounding wildcards: any subtag containing "samus".
        assert_eq!(
            names(TagPattern::AnyNamespaceGlob {
                glob: "*samus*".into()
            }),
            vec!["a.png", "b.png"]
        );
    }

    #[test]
    fn wildcard_combines_with_negation() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        tag_file(&db, a, svc, "character:samus");
        tag_file(&db, b, svc, "character:link");
        tag_file(&db, b, svc, "meta:wip");

        // character:* AND NOT meta:wip -> a only.
        let r = db
            .search(
                &Query {
                    predicates: vec![
                        Predicate::Wild(
                            TagPattern::NamespaceAny {
                                namespace: "character".into(),
                            },
                            MatchMode::Expanded,
                        ),
                        Predicate::Not(Tag::parse("meta:wip").unwrap(), MatchMode::Expanded),
                    ],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");

        // NotWild: NOT character:* -> both files have character tags -> empty.
        let none = db
            .search(
                &Query {
                    predicates: vec![Predicate::NotWild(
                        TagPattern::NamespaceAny {
                            namespace: "character".into(),
                        },
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn wildcard_matching_no_tags_is_empty() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        tag_file(&db, a, svc, "character:samus");
        let r = db
            .search(
                &Query {
                    predicates: vec![Predicate::Wild(
                        TagPattern::NamespaceAny {
                            namespace: "nonexistent".into(),
                        },
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert!(r.is_empty());
    }

    /// Set intrinsic metadata columns on a file row directly (tests only).
    fn set_meta(
        db: &Db,
        file_id: i64,
        mime: Option<&str>,
        width: Option<i64>,
        height: Option<i64>,
        duration_ms: Option<i64>,
    ) {
        db.conn
            .execute(
                "UPDATE files SET mime = ?1, width = ?2, height = ?3, duration_ms = ?4
                 WHERE id = ?5",
                params![mime, width, height, duration_ms, file_id],
            )
            .unwrap();
    }

    #[test]
    fn system_numeric_comparisons() {
        let db = Db::open_in_memory().unwrap();
        // rec(content,name).size == content.len(): a = 5 bytes, b = 11 bytes.
        let a = insert_named(&db, b"alpha", "a.png");
        let b = insert_named(&db, b"beta-bytes!", "b.png");
        set_meta(&db, a, Some("image/png"), Some(100), Some(50), Some(0));
        set_meta(&db, b, Some("image/gif"), Some(4000), Some(2000), Some(0));

        let names = |body: &str| {
            let q = Query {
                predicates: vec![Predicate::System(SystemPredicate::parse(body).unwrap())],
            };
            let mut v: Vec<String> = db
                .search(&q, ReadScope::Merged, Expansion::Expanded)
                .unwrap()
                .iter()
                .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };

        assert_eq!(names("size>5"), vec!["b.png"]); // 11 > 5, 5 !> 5
        assert_eq!(names("size>=5"), vec!["a.png", "b.png"]);
        assert_eq!(names("size<11"), vec!["a.png"]);
        assert_eq!(names("size=5"), vec!["a.png"]);
        assert_eq!(names("width>1000"), vec!["b.png"]);
        assert_eq!(names("height<=50"), vec!["a.png"]);
    }

    #[test]
    fn system_filetype_matches_exact_mime() {
        let db = Db::open_in_memory().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.gif");
        set_meta(&db, a, Some("image/png"), None, None, None);
        set_meta(&db, b, Some("image/gif"), None, None, None);

        let q = Query {
            predicates: vec![Predicate::System(
                SystemPredicate::parse("type=image/png").unwrap(),
            )],
        };
        let r = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");
    }

    #[test]
    fn system_null_column_excluded_by_compare_included_by_negation() {
        let db = Db::open_in_memory().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        let b = insert_named(&db, b"b", "b.png");
        // a has a width; b's width is NULL.
        set_meta(&db, a, Some("image/png"), Some(800), Some(600), None);
        set_meta(&db, b, Some("image/png"), None, None, None);

        // Comparison excludes the NULL-width file.
        let pos = Query {
            predicates: vec![Predicate::System(
                SystemPredicate::parse("width>0").unwrap(),
            )],
        };
        let r = db
            .search(&pos, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "a.png");

        // Negation re-includes it (all files minus those matching width>0) -> b.
        let neg = Query {
            predicates: vec![Predicate::NotSystem(
                SystemPredicate::parse("width>0").unwrap(),
            )],
        };
        let r = db
            .search(&neg, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "b.png");
    }

    #[test]
    fn system_intersects_with_tag_predicate() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let a = insert_named(&db, b"alpha", "a.png"); // 5 bytes
        let b = insert_named(&db, b"beta-bytes!", "b.png"); // 11 bytes
        set_meta(&db, a, Some("image/png"), None, None, None);
        set_meta(&db, b, Some("image/png"), None, None, None);
        tag_file(&db, a, svc, "character:samus");
        tag_file(&db, b, svc, "character:samus");

        // character:samus AND size > 5 -> only b.
        let q = Query {
            predicates: vec![
                Predicate::Tag(Tag::parse("character:samus").unwrap(), MatchMode::Expanded),
                Predicate::System(SystemPredicate::parse("size>5").unwrap()),
            ],
        };
        let r = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path.file_name().unwrap().to_str().unwrap(), "b.png");
    }

    #[test]
    fn system_matching_nothing_is_empty() {
        let db = Db::open_in_memory().unwrap();
        let a = insert_named(&db, b"a", "a.png");
        set_meta(&db, a, Some("image/png"), Some(10), Some(10), None);
        let q = Query {
            predicates: vec![Predicate::System(
                SystemPredicate::parse("width>99999").unwrap(),
            )],
        };
        assert!(
            db.search(&q, ReadScope::Merged, Expansion::Expanded)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn list_siblings_and_parents_return_tag_pairs() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let bad = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus aran").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, svc).unwrap();
        db.add_parent(ideal, parent, svc).unwrap();

        let sibs = db.list_siblings(svc).unwrap();
        assert_eq!(sibs.len(), 1);
        assert_eq!(sibs[0].0.to_string(), "samus");
        assert_eq!(sibs[0].1.to_string(), "character:samus aran");

        let pars = db.list_parents(svc).unwrap();
        assert_eq!(pars.len(), 1);
        assert_eq!(pars[0].0.to_string(), "character:samus aran");
        assert_eq!(pars[0].1.to_string(), "series:metroid");
    }

    #[test]
    fn included_services_orders_by_priority_then_id() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add_shared_service("a", "http://a/", None).unwrap(); // prio 0
        let b = db.add_shared_service("b", "http://b/", None).unwrap(); // prio 0
        db.set_service_priority(b, 500).unwrap();
        let local = db.local_service_id().unwrap(); // prio 1000

        // Merged: priority DESC, id ASC -> [local(1000), b(500), a(0)]
        assert_eq!(
            db.included_services(ReadScope::Merged).unwrap(),
            vec![local, b, a]
        );
        // LocalOnly: just the seeded local service (no second local service added yet).
        assert_eq!(
            db.included_services(ReadScope::LocalOnly).unwrap(),
            vec![local]
        );

        // Add a second local service (prio 0); LocalOnly must include both.
        let second = db.add_local_service("Hydrus: imported tags", None).unwrap();
        // seeded (prio 1000) first, second (prio 0) after, ordered prio DESC, id ASC.
        assert_eq!(
            db.included_services(ReadScope::LocalOnly).unwrap(),
            vec![local, second]
        );
    }

    /// Helper: parse a single-term query, panicking on parse error.
    fn q(tag: &str) -> naiad_core::Query {
        naiad_core::parse_query(&[tag.to_string()]).unwrap()
    }

    #[test]
    fn local_only_includes_all_local_services() {
        let db = Db::open_in_memory().unwrap();
        let first = db.local_service_id().unwrap(); // seeded, prio 1000
        let second = db.add_local_service("Hydrus: imported tags", None).unwrap(); // prio 0
        let shared = db.add_shared_service("s", "http://s/", None).unwrap();
        assert_eq!(
            db.included_services(ReadScope::LocalOnly).unwrap(),
            vec![first, second]
        );
        assert!(
            db.included_services(ReadScope::Merged)
                .unwrap()
                .contains(&shared)
        );
    }

    #[test]
    fn local_only_search_finds_tags_on_second_local_service() {
        let db = Db::open_in_memory().unwrap();
        let hydrus = db.add_local_service("Hydrus: imported tags", None).unwrap();
        let f = insert_named(&db, b"h1", "h1.png");
        tag_file(&db, f, hydrus, "series:kantai collection");
        let got = db
            .search(
                &q("series:kantai collection"),
                ReadScope::LocalOnly,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn local_service_is_included_in_merged_scope() {
        let db = Db::open_in_memory().unwrap();
        let hydrus = db.add_local_service("Hydrus: imported tags", None).unwrap();
        let f = insert_named(&db, b"h2", "h2.png");
        tag_file(&db, f, hydrus, "male");
        let got = db
            .search(&q("male"), ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn merged_sibling_edges_higher_priority_wins_lower_fills_gaps() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let pulled = db.add_shared_service("p", "http://p/", None).unwrap();
        db.set_service_priority(pulled, 10).unwrap(); // local (1000) still higher

        let a = db.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        let b = db.intern_tag(&Tag::parse("b").unwrap()).unwrap();
        let c = db.intern_tag(&Tag::parse("c").unwrap()).unwrap();
        let d = db.intern_tag(&Tag::parse("d").unwrap()).unwrap();
        let e = db.intern_tag(&Tag::parse("e").unwrap()).unwrap();

        // Conflict on `a`: local a->b, pulled a->c. Local wins.
        db.add_sibling(a, b, local).unwrap();
        db.add_sibling(a, c, pulled).unwrap();
        // Gap: only pulled defines d->e. Pulled fills it.
        db.add_sibling(d, e, pulled).unwrap();

        let services = db.included_services(ReadScope::Merged).unwrap();
        let merged = db.merged_sibling_edges(&services).unwrap();
        assert_eq!(merged.get(&a), Some(&b)); // local won
        assert_eq!(merged.get(&d), Some(&e)); // pulled filled the gap
    }

    #[test]
    fn merged_parent_edges_unions_across_services() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let pulled = db.add_shared_service("p", "http://p/", None).unwrap();

        let child = db.intern_tag(&Tag::parse("child").unwrap()).unwrap();
        let p1 = db.intern_tag(&Tag::parse("p1").unwrap()).unwrap();
        let p2 = db.intern_tag(&Tag::parse("p2").unwrap()).unwrap();

        db.add_parent(child, p1, local).unwrap();
        db.add_parent(child, p2, pulled).unwrap();

        let services = db.included_services(ReadScope::Merged).unwrap();
        let merged = db.merged_parent_edges(&services).unwrap();
        let mut got = merged.get(&child).cloned().unwrap_or_default();
        got.sort_unstable();
        assert_eq!(got, vec![p1, p2]); // union of both services' parents
    }

    #[test]
    fn file_ids_with_any_tag_spans_multiple_services() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let pulled = db.add_shared_service("p", "http://p/", None).unwrap();

        // One file, tagged only on the pulled service.
        let r = rec(b"aa", "aa.png");
        db.insert_file(&r, 1).unwrap();
        let file_id = db.file_id_by_hash(&r.hash).unwrap().unwrap();
        let t = db.intern_tag(&Tag::parse("only:pulled").unwrap()).unwrap();
        db.add_mapping(file_id, t, pulled).unwrap();

        let mut tags = BTreeSet::new();
        tags.insert(t);
        let blocks = db.block_matcher().unwrap();
        // Local-only slice: not found.
        assert!(
            db.file_ids_with_any_tag(&tags, &[local], &blocks, None)
                .unwrap()
                .is_empty()
        );
        // Merged slice: found.
        assert!(
            db.file_ids_with_any_tag(&tags, &[local, pulled], &blocks, None)
                .unwrap()
                .contains(&file_id)
        );
    }

    #[test]
    fn new_shared_service_defaults_to_zero_priority_and_is_settable() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .add_shared_service("repo-a", "http://example/", None)
            .unwrap();
        assert_eq!(db.service_priority(id).unwrap(), 0);
        db.set_service_priority(id, 50).unwrap();
        assert_eq!(db.service_priority(id).unwrap(), 50);
        // Local seeded high.
        let local = db.local_service_id().unwrap();
        assert_eq!(db.service_priority(local).unwrap(), 1000);
        // Unknown service id must return an error, not silently succeed.
        assert!(
            db.set_service_priority(999_999, 5).is_err(),
            "setting priority on a nonexistent service id must fail"
        );
    }

    #[test]
    fn block_rules_suppress_pulled_tags_but_exempt_local() {
        use naiad_core::Tag;

        let db = Db::open_in_memory().unwrap();
        let local_svc = db.local_service_id().unwrap();
        let file_id = insert_named(&db, b"a", "a.png");
        let hash = db.file_hash(file_id).unwrap().unwrap();

        // A pulled shared service asserts `meme:bad` (author AAA..) and `keep:ok`.
        let pulled = db.add_shared_service("ptr", "http://x", None).unwrap();
        db.merge_pulled_mappings(
            pulled,
            &[(
                hash,
                vec![
                    Tag::parse("meme:bad").unwrap(),
                    Tag::parse("keep:ok").unwrap(),
                ],
            )],
        )
        .unwrap();

        let q = |t: &str| Query {
            predicates: vec![Predicate::Tag(Tag::parse(t).unwrap(), MatchMode::Expanded)],
        };

        // Baseline: both pulled tags show and both match search.
        let shown = db.display_tags_of(file_id, ReadScope::Merged).unwrap();
        assert_eq!(shown.len(), 2);
        assert_eq!(
            db.search(&q("meme:bad"), ReadScope::Merged, Expansion::Expanded,)
                .unwrap()
                .len(),
            1
        );

        // Block the exact tag: it vanishes from display AND search.
        let rule = db.add_block_rule(BlockKind::Tag, "meme:bad", None).unwrap();
        let shown = db.display_tags_of(file_id, ReadScope::Merged).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].tag, Tag::parse("keep:ok").unwrap());
        assert!(
            db.search(&q("meme:bad"), ReadScope::Merged, Expansion::Expanded,)
                .unwrap()
                .is_empty()
        );

        // Removing the rule restores visibility.
        db.remove_block_rule(rule).unwrap();
        assert_eq!(
            db.display_tags_of(file_id, ReadScope::Merged)
                .unwrap()
                .len(),
            2
        );

        // Pattern block hides by namespace glob.
        db.add_block_rule(BlockKind::TagPattern, "meme:*", None)
            .unwrap();
        assert!(
            db.search(&q("meme:bad"), ReadScope::Merged, Expansion::Expanded,)
                .unwrap()
                .is_empty()
        );

        // Local-exempt: the same tag applied locally is NOT suppressed by the pattern block.
        tag_file(&db, file_id, local_svc, "meme:bad");
        let shown = db.display_tags_of(file_id, ReadScope::Merged).unwrap();
        assert!(
            shown
                .iter()
                .any(|t| t.tag == Tag::parse("meme:bad").unwrap())
        );
        assert_eq!(
            db.search(&q("meme:bad"), ReadScope::Merged, Expansion::Expanded,)
                .unwrap()
                .len(),
            1,
            "local mapping still matches despite the tag/pattern block"
        );
    }

    #[test]
    fn block_rules_add_list_remove_and_are_idempotent() {
        let db = Db::open_in_memory().unwrap();

        let tag_id = db.add_block_rule(BlockKind::Tag, "Meme:Bad", None).unwrap();
        // Adding the same rule (after normalization) is idempotent: same id.
        let again = db
            .add_block_rule(BlockKind::Tag, "meme:bad", Some("dupe"))
            .unwrap();
        assert_eq!(tag_id, again);

        // Non-canonical input for TagPattern: 'Spam:*' should canonicalize to 'spam:*'.
        let pat_id = db
            .add_block_rule(BlockKind::TagPattern, "Spam:*", None)
            .unwrap();
        // Adding the canonical form afterwards returns the SAME id (idempotent).
        let pat_id_again = db
            .add_block_rule(BlockKind::TagPattern, "spam:*", None)
            .unwrap();
        assert_eq!(pat_id, pat_id_again);

        let author_id = db
            .add_block_rule(BlockKind::Author, &"ab".repeat(32), Some("troll"))
            .unwrap();

        let rules = db.list_block_rules().unwrap();
        assert_eq!(rules.len(), 3);
        // Exact tag target is normalized to 'ns:subtag'.
        assert_eq!(rules[0].kind, BlockKind::Tag);
        assert_eq!(rules[0].target, "meme:bad");
        // TagPattern target is canonicalized (lowercased).
        assert_eq!(rules[1].kind, BlockKind::TagPattern);
        assert_eq!(rules[1].target, "spam:*");
        // Author target is lowercased hex.
        assert_eq!(rules[2].kind, BlockKind::Author);
        assert_eq!(rules[2].id, author_id);
        assert_eq!(rules[2].target, "ab".repeat(32));

        db.remove_block_rule(tag_id).unwrap();
        assert_eq!(db.list_block_rules().unwrap().len(), 2);

        // Removing a missing id errors.
        assert!(db.remove_block_rule(9999).is_err());

        // Invalid targets are rejected.
        assert!(
            db.add_block_rule(BlockKind::Author, "nothex", None)
                .is_err()
        );
        assert!(db.add_block_rule(BlockKind::Tag, "   ", None).is_err());
    }

    #[test]
    fn search_finds_pulled_tags_and_applies_pulled_siblings() {
        let db = Db::open_in_memory().unwrap();
        let _local = db.local_service_id().unwrap();
        let pulled = db.add_shared_service("p", "http://p/", None).unwrap();

        let r = rec(b"cc", "cc.png");
        db.insert_file(&r, 1).unwrap();
        let file_id = db.file_id_by_hash(&r.hash).unwrap().unwrap();

        // File is tagged char:samus only on the pulled service.
        let ideal = db.intern_tag(&Tag::parse("char:samus").unwrap()).unwrap();
        db.add_mapping(file_id, ideal, pulled).unwrap();
        // Pulled service aliases char:samus_aran -> char:samus.
        let bad = db
            .intern_tag(&Tag::parse("char:samus_aran").unwrap())
            .unwrap();
        db.add_sibling(bad, ideal, pulled).unwrap();

        // Merged: the pulled mapping + pulled sibling make it match.
        let merged = db
            .search(
                &Query {
                    predicates: vec![Predicate::Tag(
                        Tag::parse("char:samus_aran").unwrap(),
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap();
        assert_eq!(merged.len(), 1);

        // LocalOnly: pulled data invisible -> no match.
        let local_only = db
            .search(
                &Query {
                    predicates: vec![Predicate::Tag(
                        Tag::parse("char:samus_aran").unwrap(),
                        MatchMode::Expanded,
                    )],
                },
                ReadScope::LocalOnly,
                Expansion::Expanded,
            )
            .unwrap();
        assert!(local_only.is_empty());
    }

    // Helper: build a sibling DeltaEdgeInput.
    fn sib(from: &str, to: &str, author: &str, seq: u64, deleted: bool) -> DeltaEdgeInput {
        DeltaEdgeInput {
            kind: EdgeKind::Sibling,
            from: Tag::parse(from).unwrap(),
            to: Tag::parse(to).unwrap(),
            author: author.to_string(),
            deleted,
            seq,
        }
    }

    #[test]
    fn first_delta_pull_materializes_siblings_and_advances_cursor() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://x", None).unwrap();
        let edges = vec![sib(
            "character:samus_aran",
            "character:samus",
            "aa",
            1,
            false,
        )];
        let stats = db.merge_relation_delta(svc, true, 1, &edges).unwrap();
        assert_eq!(stats.siblings, 1);
        assert_eq!(db.relation_cursor(svc).unwrap(), Some(1));
        assert!(db.last_relation_pull_at(svc).unwrap().is_some());
        let sibs = db.list_siblings(svc).unwrap();
        assert_eq!(sibs.len(), 1);
        assert_eq!(sibs[0].1, Tag::parse("character:samus").unwrap());
    }

    #[test]
    fn a_tombstone_removes_the_sibling_winner() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://x", None).unwrap();
        db.merge_relation_delta(svc, true, 1, &[sib("a:x", "a:y", "aa", 1, false)])
            .unwrap();
        let stats = db
            .merge_relation_delta(svc, false, 2, &[sib("a:x", "a:y", "aa", 2, true)])
            .unwrap();
        assert_eq!(stats.siblings, 0, "winner removed");
        assert_eq!(db.relation_cursor(svc).unwrap(), Some(2));
    }

    #[test]
    fn retracting_the_current_winner_promotes_the_next_smallest() {
        use naiad_core::Tag;
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://x", None).unwrap();
        db.merge_relation_delta(
            svc,
            true,
            2,
            &[
                sib("a:bad", "a:aaa", "aa", 1, false),
                sib("a:bad", "a:zzz", "bb", 2, false),
            ],
        )
        .unwrap();
        // current winner is a:aaa (smallest to). Retract it -> a:zzz promoted.
        db.merge_relation_delta(svc, false, 3, &[sib("a:bad", "a:aaa", "aa", 3, true)])
            .unwrap();
        let sibs = db.list_siblings(svc).unwrap();
        assert_eq!(sibs.len(), 1);
        assert_eq!(sibs[0].1, Tag::parse("a:zzz").unwrap());
    }

    #[test]
    fn full_reset_clears_stale_staging() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://x", None).unwrap();
        db.merge_relation_delta(svc, true, 5, &[sib("a:x", "a:y", "aa", 5, false)])
            .unwrap();
        let stats = db
            .merge_relation_delta(svc, true, 1, &[sib("a:p", "a:q", "aa", 1, false)])
            .unwrap();
        assert_eq!(stats.siblings, 1, "only the new edge survives");
        assert_eq!(db.relation_cursor(svc).unwrap(), Some(1));
    }

    #[test]
    fn parent_edges_add_then_tombstone_and_author_updates_to_min() {
        use naiad_core::Tag;
        let par = |from: &str, to: &str, author: &str, seq: u64, deleted: bool| DeltaEdgeInput {
            kind: EdgeKind::Parent,
            from: Tag::parse(from).unwrap(),
            to: Tag::parse(to).unwrap(),
            author: author.to_string(),
            deleted,
            seq,
        };
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://x", None).unwrap();

        // Add a parent edge → tag_parents has it.
        let stats = db
            .merge_relation_delta(svc, true, 1, &[par("a:child", "a:parent", "bb", 1, false)])
            .unwrap();
        assert_eq!(stats.parents, 1);
        let parents = db.list_parents(svc).unwrap();
        assert_eq!(parents.len(), 1);
        assert_eq!(
            parents[0],
            (
                Tag::parse("a:child").unwrap(),
                Tag::parse("a:parent").unwrap()
            )
        );

        // A second author for the same edge with a smaller author → MIN(author)
        // wins ("aa" < "bb"); the collapsed row's author updates in place.
        db.merge_relation_delta(svc, false, 2, &[par("a:child", "a:parent", "aa", 2, false)])
            .unwrap();
        assert_eq!(db.list_parents(svc).unwrap().len(), 1, "still one edge");

        // Tombstone both authors → the parent row disappears.
        let stats = db
            .merge_relation_delta(
                svc,
                false,
                4,
                &[
                    par("a:child", "a:parent", "aa", 3, true),
                    par("a:child", "a:parent", "bb", 4, true),
                ],
            )
            .unwrap();
        assert_eq!(stats.parents, 0, "no current author left → row removed");
        assert!(db.list_parents(svc).unwrap().is_empty());
    }

    #[test]
    fn app_setting_round_trips_internal_scalar() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.app_setting("ui.gallery_sort").unwrap(), None);

        db.set_app_setting("ui.gallery_sort", "name:asc").unwrap();
        assert_eq!(
            db.app_setting("ui.gallery_sort").unwrap().as_deref(),
            Some("name:asc")
        );

        db.set_app_setting("ui.gallery_sort", "size:desc").unwrap();
        assert_eq!(
            db.app_setting("ui.gallery_sort").unwrap().as_deref(),
            Some("size:desc")
        );
    }

    // ── (trust-floor tests deleted) ──────────────────────────────────────────

    #[test]
    fn local_service_can_be_created_and_is_distinct() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .add_local_service("Hydrus: public tag repository", None)
            .unwrap();
        assert!(id > 1, "distinct from the seeded local service id 1");
        assert!(
            db.add_local_service("Hydrus: public tag repository", None)
                .is_err()
        );
    }

    #[test]
    fn insert_file_persists_and_backfills_sha256() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let marker = db.next_scan_marker().unwrap();
        let rec = FileRecord::new(hash_bytes(b"x"), "/a/x.png".into(), 1, None);
        db.insert_file(&rec, marker).unwrap(); // no sha256 yet
        let rec2 = rec.clone().with_sha256("ab".repeat(32));
        db.insert_file(&rec2, db.next_scan_marker().unwrap())
            .unwrap();
        let missing = db.files_missing_sha256().unwrap();
        assert!(missing.is_empty(), "sha256 was backfilled on rescan");
    }

    #[test]
    fn with_tx_commits_on_ok_and_rolls_back_on_err() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let marker = db.next_scan_marker().unwrap();

        // Ok closure: writes inside the transaction are committed.
        db.with_tx(|db| {
            let rec = FileRecord::new(hash_bytes(b"kept"), "/kept.png".into(), 4, None);
            db.insert_file(&rec, marker)
        })
        .unwrap();
        assert_eq!(db.file_count().unwrap(), 1);

        // Err closure: writes made before the failure are rolled back.
        let res: Result<()> = db.with_tx(|db| {
            let rec = FileRecord::new(hash_bytes(b"gone"), "/gone.png".into(), 4, None);
            db.insert_file(&rec, marker)?;
            Err(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))
        });
        assert!(res.is_err());
        assert_eq!(db.file_count().unwrap(), 1, "failed tx must roll back");
    }

    #[test]
    fn stage_then_resolve_creates_mappings_for_known_files() {
        use naiad_core::{FileRecord, Tag, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_local_service("Hydrus: PTR", None).unwrap();
        let sha = "cd".repeat(32);
        let tag_id = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        db.stage_mapping(&sha, tag_id, svc, "current").unwrap();
        assert_eq!(
            db.resolve_staged_mappings(svc, "sha256").unwrap(),
            0,
            "no file yet"
        );
        let rec =
            FileRecord::new(hash_bytes(b"f"), "/f.png".into(), 1, None).with_sha256(sha.clone());
        db.insert_file(&rec, db.next_scan_marker().unwrap())
            .unwrap();
        assert_eq!(db.resolve_staged_mappings(svc, "sha256").unwrap(), 1);
        let file_id = db.file_id_by_hash(&hash_bytes(b"f")).unwrap().unwrap();
        assert_eq!(db.tags_of(file_id).unwrap().len(), 1);
        assert_eq!(db.resolve_staged_mappings(svc, "sha256").unwrap(), 0);
    }

    #[test]
    fn deleted_staged_tombstone_removes_mapping() {
        use naiad_core::{FileRecord, Tag, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_local_service("Hydrus: PTR", None).unwrap();
        let sha = "ef".repeat(32);
        let tag_id = db.intern_tag(&Tag::parse("messy").unwrap()).unwrap();
        let rec =
            FileRecord::new(hash_bytes(b"g"), "/g.png".into(), 1, None).with_sha256(sha.clone());
        db.insert_file(&rec, db.next_scan_marker().unwrap())
            .unwrap();
        db.stage_mapping(&sha, tag_id, svc, "current").unwrap();
        db.resolve_staged_mappings(svc, "sha256").unwrap();
        let file_id = db.file_id_by_hash(&hash_bytes(b"g")).unwrap().unwrap();
        assert_eq!(db.tags_of(file_id).unwrap().len(), 1);
        db.stage_mapping(&sha, tag_id, svc, "deleted").unwrap();
        db.resolve_staged_mappings(svc, "sha256").unwrap();
        assert_eq!(db.tags_of(file_id).unwrap().len(), 0);
    }

    #[test]
    fn library_sha256s_lists_present_files_with_sha() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        // One file with a sha256, one without.
        let with = FileRecord::new(hash_bytes(b"a"), "/a.png".into(), 1, None)
            .with_sha256("11".repeat(32));
        let without = FileRecord::new(hash_bytes(b"b"), "/b.png".into(), 1, None);
        db.insert_file(&with, db.next_scan_marker().unwrap())
            .unwrap();
        db.insert_file(&without, db.next_scan_marker().unwrap())
            .unwrap();
        let shas = db.library_sha256s().unwrap();
        assert_eq!(shas, vec!["11".repeat(32)], "only the file with a sha256");
    }

    #[test]
    fn library_files_with_sha256_keeps_file_id() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let with = FileRecord::new(hash_bytes(b"a"), "/a.png".into(), 1, None)
            .with_sha256("22".repeat(32));
        let without = FileRecord::new(hash_bytes(b"b"), "/b.png".into(), 1, None);
        db.insert_file(&with, db.next_scan_marker().unwrap())
            .unwrap();
        db.insert_file(&without, db.next_scan_marker().unwrap())
            .unwrap();
        let id = db.file_id_by_hash(&hash_bytes(b"a")).unwrap().unwrap();
        let rows = db.library_files_with_sha256().unwrap();
        assert_eq!(
            rows,
            vec![(id, "22".repeat(32))],
            "only the file with a sha"
        );
    }

    #[test]
    fn apply_hydrus_mappings_interns_and_dedups() {
        use naiad_core::{FileRecord, Tag, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let rec = FileRecord::new(hash_bytes(b"f"), "/f.png".into(), 1, None);
        db.insert_file(&rec, db.next_scan_marker().unwrap())
            .unwrap();
        let file_id = db.file_id_by_hash(&hash_bytes(b"f")).unwrap().unwrap();
        let svc = db.add_local_service("Hydrus: imported tags", None).unwrap();

        let items = vec![
            (file_id, Tag::parse("maid").unwrap()),
            (file_id, Tag::parse("character:samus").unwrap()),
        ];
        let applied = db.apply_hydrus_mappings(svc, &items).unwrap();
        assert_eq!(applied, 2, "two new mappings");

        // Re-applying the same batch is idempotent (no duplicates).
        let again = db.apply_hydrus_mappings(svc, &items).unwrap();
        assert_eq!(again, 0, "no new rows on re-apply");

        let tags = db.tags_of(file_id).unwrap();
        assert_eq!(tags.len(), 2, "file carries both tags");
    }

    #[test]
    fn open_readonly_sees_writer_commits_and_rejects_writes() {
        use naiad_core::{FileRecord, hash_bytes};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.db");

        let writer = Db::open(&path).unwrap();
        let rec = FileRecord::new(hash_bytes(b"r"), "/r.png".into(), 1, None);
        writer
            .insert_file(&rec, writer.next_scan_marker().unwrap())
            .unwrap();

        // A separate read-only connection sees the committed row...
        let reader = Db::open_readonly(&path).unwrap();
        assert_eq!(reader.list_files().unwrap().len(), 1);
        // ...and cannot mutate the DB.
        assert!(
            reader
                .insert_file(&rec, reader.next_scan_marker().unwrap_or(1))
                .is_err(),
            "read-only connection must reject writes"
        );
    }

    /// The writer must carry the #232 WAL-hygiene pragmas; `synchronous`
    /// reports 1 for NORMAL.
    #[test]
    fn writer_gets_wal_hygiene_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("naiad.db")).unwrap();
        let jsl: i64 = db
            .conn
            .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jsl, WAL_SIZE_LIMIT);
        let sync: i64 = db
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 1, "synchronous must be NORMAL");
    }

    /// `checkpoint_wal` with no readers must fully checkpoint and truncate the
    /// WAL file to zero bytes (#232).
    #[test]
    fn checkpoint_wal_truncates_when_unpinned() {
        use naiad_core::{FileRecord, hash_bytes};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.db");
        let db = Db::open(&path).unwrap();
        let rec = FileRecord::new(hash_bytes(b"cp"), "/cp.png".into(), 1, None);
        db.insert_file(&rec, db.next_scan_marker().unwrap())
            .unwrap();

        let cp = db.checkpoint_wal().unwrap();
        assert!(!cp.busy, "no readers → checkpoint must complete");
        assert_eq!(
            cp.log_frames, cp.checkpointed_frames,
            "all frames must be copied back"
        );
        let wal_len = std::fs::metadata(dir.path().join("naiad.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(wal_len, 0, "WAL must be truncated (got {wal_len} bytes)");
    }

    #[test]
    fn complete_tags_ranks_by_count_desc() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f1 = insert_named(&db, b"1", "1.png");
        let f2 = insert_named(&db, b"2", "2.png");
        let f3 = insert_named(&db, b"3", "3.png");
        for f in [f1, f2, f3] {
            tag_file(&db, f, svc, "character:samus_aran");
        }
        tag_file(&db, f1, svc, "character:samurai");
        let out = db
            .complete_tags("character:samu", 10, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].subtag, "samus_aran");
        assert_eq!(out[0].count, 3);
        assert_eq!(out[1].subtag, "samurai");
    }

    #[test]
    fn completion_count_cache_tracks_mapping_writes() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f1 = insert_named(&db, b"completion-count-1", "count1.png");
        let f2 = insert_named(&db, b"completion-count-2", "count2.png");
        let tag = Tag::parse("character:samus").unwrap();
        let tag_id = db.intern_tag(&tag).unwrap();

        db.add_mapping(f1, tag_id, svc).unwrap();
        db.add_mapping(f2, tag_id, svc).unwrap();
        db.add_mapping(f2, tag_id, svc).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "idempotent inserts count current mappings once");

        db.remove_mapping(f1, tag_id, svc).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "removing a current mapping decrements the cache");

        db.remove_mapping(f2, tag_id, svc).unwrap();
        let remaining: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "zero-count tags are removed from the cache");
    }

    #[test]
    fn completion_count_cache_tracks_authoritative_repo_replaces() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo", None).unwrap();
        let owned = hash_bytes(b"completion-repo-owned");
        db.insert_file(
            &FileRecord::new(owned, "/repo-owned.png".into(), 1, None),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let tag = Tag::parse("character:samus").unwrap();

        db.merge_pulled_mappings(svc, &[(owned, vec![tag.clone()])])
            .unwrap();
        let tag_id = db.tag_id(&tag).unwrap().unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        db.merge_pulled_mappings(svc, &[] as &[(_, Vec<Tag>)])
            .unwrap();
        let remaining: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 0,
            "authoritative replacement removes stale completion counts"
        );
    }

    #[test]
    fn complete_tags_bare_token_matches_subtag_across_namespaces() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "character:samus_aran");
        tag_file(&db, f, svc, "samus_amiibo"); // unnamespaced
        let out = db
            .complete_tags("samu", 10, CompletionMode::Prefix)
            .unwrap();
        let subs: Vec<&str> = out.iter().map(|t| t.subtag.as_str()).collect();
        assert!(subs.contains(&"samus_aran"));
        assert!(subs.contains(&"samus_amiibo"));
    }

    /// Regression for #71: the unnamespaced completion branch must resolve via an
    /// index, never a full scan of the ~1M-row `tags` table. The 0012 BINARY index
    /// couldn't serve `subtag LIKE ?` (LIKE is case-insensitive → needs NOCASE);
    /// the resulting `SCAN` cost ~86s cold and froze the tag lane. Assert the query
    /// plan uses the NOCASE index and does not scan `tags`.
    #[test]
    fn complete_tags_unnamespaced_uses_index_not_scan() {
        let db = Db::open_in_memory().unwrap();
        let plan: Vec<String> = db
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT t.namespace, t.subtag, c.current_count
                 FROM tags t
                 JOIN tag_completion_counts c ON c.tag_id = t.id
                 WHERE t.subtag LIKE ?1 ESCAPE '\\'
                 ORDER BY c.current_count DESC, t.namespace ASC, t.subtag ASC
                 LIMIT ?2",
            )
            .unwrap()
            .query_map(params!["samu%", 10i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();
        let joined = plan.join(" | ");
        assert!(
            joined.contains("idx_tags_subtag_nocase"),
            "expected NOCASE index range scan, got plan: {joined}"
        );
        assert!(
            !plan.iter().any(|s| s.contains("SCAN t")),
            "unnamespaced completion still full-scans tags: {joined}"
        );
    }

    /// Regression for #76: startup warmup must sequentially walk the same
    /// NOCASE index used by bare-prefix completion plus the two tables it probes.
    /// Otherwise a new prefix still faults untouched pages on first use.
    #[test]
    fn completion_warmup_walks_index_and_count_rows() {
        let db = Db::open_in_memory().unwrap();
        let explain = |sql: &str| -> String {
            db.conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<String>>>()
                .unwrap()
                .join(" | ")
        };
        let index_plan = explain(
            "SELECT COALESCE(SUM(length(subtag)), 0)
             FROM tags INDEXED BY idx_tags_subtag_nocase",
        );
        assert!(
            index_plan.contains("SCAN tags USING COVERING INDEX idx_tags_subtag_nocase"),
            "warmup must walk the completion index: {index_plan}"
        );
        let tags_plan = explain(
            "SELECT COALESCE(SUM(length(namespace)), 0)
             FROM tags NOT INDEXED",
        );
        assert!(
            tags_plan.contains("SCAN tags"),
            "warmup must walk tags table pages: {tags_plan}"
        );
        let counts_plan = explain(
            "SELECT COALESCE(SUM(current_count), 0)
             FROM tag_completion_counts NOT INDEXED",
        );
        assert!(
            counts_plan.contains("SCAN tag_completion_counts"),
            "warmup must walk completion-count pages: {counts_plan}"
        );
    }

    #[test]
    fn interrupt_handle_stops_a_running_statement() {
        let db = Db::open_in_memory().unwrap();
        let interrupt = db.interrupt_handle();
        let worker = std::thread::spawn(move || {
            db.conn.query_row(
                "WITH RECURSIVE count(x) AS (
                     VALUES(0)
                     UNION ALL
                     SELECT x + 1 FROM count WHERE x < 1000000000
                 )
                 SELECT sum(x) FROM count",
                [],
                |row| row.get::<_, i64>(0),
            )
        });

        // Interrupt repeatedly so the test is race-free even if the worker has
        // not entered sqlite3_step by the first call.
        while !worker.is_finished() {
            interrupt.interrupt();
            std::thread::sleep(Duration::from_millis(1));
        }
        let err = worker
            .join()
            .expect("query worker panicked")
            .expect_err("long-running statement was not interrupted");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
    }

    #[test]
    fn query_cancellation_is_effective_before_statement_start() {
        let db = Db::open_in_memory().unwrap();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let progress_cancelled = Arc::clone(&cancelled);
        let result = db.with_query_cancellation(
            move || progress_cancelled.load(std::sync::atomic::Ordering::Acquire),
            |db| {
                db.conn.query_row(
                    "WITH RECURSIVE count(x) AS (
                         VALUES(0)
                         UNION ALL
                         SELECT x + 1 FROM count WHERE x < 1000000000
                     )
                     SELECT sum(x) FROM count",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            },
        );
        let err = result.expect_err("pre-statement cancellation was ignored");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
        assert_eq!(
            db.conn
                .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn query_cancellation_remains_effective_between_statements() {
        let db = Db::open_in_memory().unwrap();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_cancelled = Arc::clone(&cancelled);
        let result = db.with_query_cancellation(
            move || progress_cancelled.load(std::sync::atomic::Ordering::Acquire),
            |db| {
                db.conn
                    .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                db.conn.query_row(
                    "WITH RECURSIVE count(x) AS (
                         VALUES(0)
                         UNION ALL
                         SELECT x + 1 FROM count WHERE x < 1000000000
                     )
                     SELECT sum(x) FROM count",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            },
        );
        let err = result.expect_err("between-statement cancellation was ignored");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
    }

    #[test]
    fn complete_tags_substring_matches_interior_prefix_does_not() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "samus");
        assert!(
            db.complete_tags("amu", 10, CompletionMode::Prefix)
                .unwrap()
                .is_empty()
        );
        let sub = db
            .complete_tags("amu", 10, CompletionMode::Substring)
            .unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].subtag, "samus");
    }

    #[test]
    fn complete_tags_escapes_like_metachars_in_token() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "samus_aran");
        tag_file(&db, f, svc, "samusxaran"); // would match if `_` were a wildcard
        let out = db
            .complete_tags("samus_a", 10, CompletionMode::Prefix)
            .unwrap();
        let subs: Vec<&str> = out.iter().map(|t| t.subtag.as_str()).collect();
        assert!(subs.contains(&"samus_aran"));
        assert!(!subs.contains(&"samusxaran"));
    }

    #[test]
    fn complete_tags_empty_token_is_empty() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            db.complete_tags("", 10, CompletionMode::Prefix)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn complete_tags_quoted_fragment_matches_spaced_subtag() {
        // #49: typing an opening quote must not kill autocomplete. `"some`
        // should surface the spaced tag `some tag` (quote is a delimiter, not a
        // literal), mirroring how search's tokenize() strips quotes.
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "some tag");
        tag_file(&db, f, svc, "some other tag");
        let out = db
            .complete_tags("\"some", 10, CompletionMode::Prefix)
            .unwrap();
        let subs: Vec<&str> = out.iter().map(|t| t.subtag.as_str()).collect();
        assert!(subs.contains(&"some tag"), "got {subs:?}");
        assert!(subs.contains(&"some other tag"), "got {subs:?}");
    }

    #[test]
    fn complete_tags_quoted_namespaced_fragment_resolves_namespace() {
        // #49: `creator:"some` keeps the namespace and matches the spaced subtag.
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "creator:some body");
        tag_file(&db, f, svc, "character:some body"); // different namespace — excluded
        let out = db
            .complete_tags("creator:\"some", 10, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(out.len(), 1, "got {out:?}");
        assert_eq!(out[0].namespace, "creator");
        assert_eq!(out[0].subtag, "some body");
    }

    #[test]
    fn complete_tags_bare_quote_is_empty() {
        // A lone quote has no inner text → no suggestions (not an error).
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "some tag");
        assert!(
            db.complete_tags("\"", 10, CompletionMode::Prefix)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn complete_namespaces_quoted_prefix_resolves() {
        // #49: `"creat` should still rank the `creator` namespace.
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "creator:foo");
        let out = db.complete_namespaces("\"creat", 10).unwrap();
        let names: Vec<&str> = out.iter().map(|n| n.namespace.as_str()).collect();
        assert!(names.contains(&"creator"), "got {names:?}");
    }

    #[test]
    fn complete_namespaces_ranks_and_excludes_unnamespaced() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "artist:foo");
        tag_file(&db, f, svc, "artist:bar");
        tag_file(&db, f, svc, "art:thing");
        tag_file(&db, f, svc, "general_tag"); // unnamespaced — must not appear
        let out = db.complete_namespaces("art", 10).unwrap();
        let names: Vec<&str> = out.iter().map(|n| n.namespace.as_str()).collect();
        assert_eq!(names, vec!["artist", "art"]); // artist: 2 tags, art: 1
        assert!(!names.contains(&""));
    }

    #[test]
    fn complete_namespaces_ignores_relation_only_tags() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"1", "1.png");
        tag_file(&db, f, svc, "artist:foo");
        // Interned but never mapped — a relation-import dictionary entry.
        db.intern_tag(&Tag::parse("artist:ghost").unwrap()).unwrap();
        db.intern_tag(&Tag::parse("photoset:only-in-dictionary").unwrap())
            .unwrap();

        let out = db.complete_namespaces("", 10).unwrap();
        let artist = out.iter().find(|n| n.namespace == "artist").unwrap();
        assert_eq!(artist.tag_count, 1, "unmapped artist:ghost must not count");
        assert!(
            !out.iter().any(|n| n.namespace == "photoset"),
            "namespace with zero mapped tags must not appear"
        );
    }

    // ── TagCache + batch methods ─────────────────────────────────────────────

    #[test]
    fn intern_tag_cached_returns_same_id_and_inserts_once() {
        let db = Db::open_in_memory().unwrap();
        let tag = Tag::parse("character:samus").unwrap();
        let cache = TagCache::new();
        let mut pending = TagCache::new();

        let id1 = db.intern_tag_cached(&tag, &cache, &mut pending).unwrap();

        // COUNT before second call — should stay at 1.
        let count_before: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();

        let id2 = db.intern_tag_cached(&tag, &cache, &mut pending).unwrap();

        let count_after: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();

        assert_eq!(id1, id2, "cache hit must return the same id");
        assert_eq!(count_before, 1, "one tags row after first intern");
        assert_eq!(count_after, count_before, "no second INSERT on cache hit");
    }

    #[test]
    fn failed_batch_rolls_back_without_polluting_tag_cache() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_local_service("rollback-svc", None).unwrap();
        let mut cache = TagCache::new();

        // Make the staged_mappings insert fail after the tag has been interned.
        db.conn
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON staged_mappings
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            )
            .unwrap();

        let items: Vec<(String, Tag, &str)> = vec![(
            "aa".repeat(32),
            Tag::parse("character:samus").unwrap(),
            "current",
        )];
        db.stage_mappings_batch(svc, &items, &mut cache)
            .unwrap_err();

        let tags_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags_count, 0, "interned tag row rolled back");
        assert!(
            cache.0.is_empty(),
            "cache must not hold ids from a rolled-back transaction"
        );

        // The same cache stays usable once the failure is cleared.
        db.conn.execute_batch("DROP TRIGGER boom").unwrap();
        let n = db.stage_mappings_batch(svc, &items, &mut cache).unwrap();
        assert_eq!(n, 1);
        assert_eq!(cache.0.len(), 1, "committed intern lands in the cache");
    }

    #[test]
    fn stage_mappings_batch_upserts_and_last_status_wins() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_local_service("batch-svc", None).unwrap();
        let sha = "aa".repeat(32);
        let tag = Tag::parse("character:samus").unwrap();
        let mut cache = TagCache::new();

        // Two entries for the same key; the second (deleted) must win.
        let items: Vec<(String, Tag, &str)> = vec![
            (sha.clone(), tag.clone(), "current"),
            (sha.clone(), tag.clone(), "deleted"),
        ];
        let n = db.stage_mappings_batch(svc, &items, &mut cache).unwrap();
        assert_eq!(n, 2, "returns items.len(), not rows changed");

        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM staged_mappings WHERE sha256 = ?1",
                params![sha],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "deleted", "later status wins within the batch");
    }

    #[test]
    fn stage_mappings_batch_count_matches_items() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_local_service("batch-svc2", None).unwrap();
        let mut cache = TagCache::new();

        let items: Vec<(String, Tag, &str)> = (0u8..5)
            .map(|i| {
                (
                    format!("{:0>64}", i),
                    Tag::parse(&format!("tag:item{i}")).unwrap(),
                    "current",
                )
            })
            .collect();
        let n = db.stage_mappings_batch(svc, &items, &mut cache).unwrap();
        assert_eq!(n, 5, "count equals number of items submitted");
    }

    #[test]
    fn add_siblings_batch_replaces_prior_ideal() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let bad = Tag::parse("samus").unwrap();
        let ideal1 = Tag::parse("character:samus").unwrap();
        let ideal2 = Tag::parse("character:samus aran").unwrap();
        let mut cache = TagCache::new();

        let n1 = db
            .add_siblings_batch(svc, &[(bad.clone(), ideal1)], &mut cache)
            .unwrap();
        assert_eq!(n1, 1, "first alias applied");

        let n2 = db
            .add_siblings_batch(svc, &[(bad.clone(), ideal2.clone())], &mut cache)
            .unwrap();
        assert_eq!(n2, 1, "re-alias applied");

        let bad_id = db.intern_tag(&bad).unwrap();
        let ideal2_id = db.intern_tag(&ideal2).unwrap();
        let edges = db.load_sibling_edges(svc).unwrap();
        assert_eq!(edges.len(), 1, "still only one edge");
        assert_eq!(
            edges.get(&bad_id),
            Some(&ideal2_id),
            "re-alias replaced the prior ideal"
        );
    }

    #[test]
    fn add_parents_batch_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let child = Tag::parse("character:samus aran").unwrap();
        let parent = Tag::parse("series:metroid").unwrap();
        let mut cache = TagCache::new();

        let n1 = db
            .add_parents_batch(svc, &[(child.clone(), parent.clone())], &mut cache)
            .unwrap();
        assert_eq!(n1, 1, "first application counted");

        let n2 = db
            .add_parents_batch(svc, &[(child.clone(), parent.clone())], &mut cache)
            .unwrap();
        assert_eq!(n2, 1, "idempotent re-application still counted");

        let child_id = db.intern_tag(&child).unwrap();
        let edges = db.load_parent_edges(svc).unwrap();
        assert_eq!(
            edges.get(&child_id).unwrap().len(),
            1,
            "no duplicate edge stored"
        );
    }

    #[test]
    fn batch_self_relations_skipped_and_not_counted() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let t = Tag::parse("self:same").unwrap();
        let other = Tag::parse("other:tag").unwrap();
        let mut cache = TagCache::new();

        // First item is a self-sibling (skipped); second is valid (counted).
        let sibs = db
            .add_siblings_batch(
                svc,
                &[(t.clone(), t.clone()), (t.clone(), other.clone())],
                &mut cache,
            )
            .unwrap();
        assert_eq!(sibs, 1, "self-sibling skipped, valid sibling counted");

        // First item is a self-parent (skipped); second is valid (counted).
        let pars = db
            .add_parents_batch(
                svc,
                &[(t.clone(), t.clone()), (other.clone(), t.clone())],
                &mut cache,
            )
            .unwrap();
        assert_eq!(pars, 1, "self-parent skipped, valid parent counted");
    }

    #[test]
    fn set_sha256_batch_updates_all_rows_and_returns_count() {
        use naiad_core::FileRecord;
        let db = Db::open_in_memory().unwrap();
        let recs = [
            FileRecord::new(hash_bytes(b"a"), "/a.png".into(), 1, None),
            FileRecord::new(hash_bytes(b"b"), "/b.png".into(), 1, None),
        ];
        for r in &recs {
            db.insert_file(r, db.next_scan_marker().unwrap()).unwrap();
        }
        let id_a = db.file_id_by_hash(&hash_bytes(b"a")).unwrap().unwrap();
        let id_b = db.file_id_by_hash(&hash_bytes(b"b")).unwrap().unwrap();

        let sha_a = "aa".repeat(32);
        let sha_b = "bb".repeat(32);
        let n = db
            .set_sha256_batch(&[(id_a, sha_a.clone()), (id_b, sha_b.clone())])
            .unwrap();
        assert_eq!(n, 2, "returns items.len()");

        let got_a: String = db
            .conn
            .query_row(
                "SELECT sha256 FROM files WHERE id = ?1",
                params![id_a],
                |r| r.get(0),
            )
            .unwrap();
        let got_b: String = db
            .conn
            .query_row(
                "SELECT sha256 FROM files WHERE id = ?1",
                params![id_b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(got_a, sha_a, "sha256 written for file a");
        assert_eq!(got_b, sha_b, "sha256 written for file b");
    }

    #[test]
    fn relation_graph_version_bumps_on_relation_writes_only() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let a = db.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        let b = db.intern_tag(&Tag::parse("b").unwrap()).unwrap();

        let v0 = db.relation_graph_version().unwrap();
        db.add_sibling(a, b, local).unwrap();
        let v1 = db.relation_graph_version().unwrap();
        assert!(v1 > v0, "sibling write must bump");

        db.add_parent(a, b, local).unwrap();
        let v2 = db.relation_graph_version().unwrap();
        assert!(v2 > v1, "parent write must bump");

        let shared = db.add_shared_service("s", "http://s/", None).unwrap();
        let v3 = db.relation_graph_version().unwrap();
        assert!(v3 > v2, "service write must bump");

        db.set_service_priority(shared, 42).unwrap();
        let v4 = db.relation_graph_version().unwrap();
        assert!(v4 > v3, "priority change must bump");

        // Local mapping must bump (any write bumps now that author column is gone).
        let f = insert_named(&db, b"x", "x.png");
        tag_file(&db, f, local, "c");
        let v5 = db.relation_graph_version().unwrap();
        assert!(
            v5 > v4,
            "local mapping write must bump relation_graph_version"
        );

        // block_rules write must bump.
        db.add_block_rule(
            BlockKind::Author,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
        )
        .unwrap();
        let v6 = db.relation_graph_version().unwrap();
        assert!(v6 > v5, "block_rule write must bump");
    }

    /// A syntactically valid 64-hex-char author, for tests that exercise
    /// `BlockKind::Author` rules (which validate the hex format) alongside
    /// matching authored mappings.
    #[allow(dead_code)]
    const AUTH1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn relation_graph_is_reused_until_invalidated() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        // Add a shared service so that `included_services` returns at least 2
        // entries — otherwise `services[..1]` would equal `services` and the
        // service-set-change assertion below would be vacuous.
        db.add_shared_service("peer", "http://peer/", None).unwrap();
        let a = db.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        let b = db.intern_tag(&Tag::parse("b").unwrap()).unwrap();
        db.add_sibling(a, b, local).unwrap();

        let services = db.included_services(ReadScope::Merged).unwrap();
        assert!(services.len() >= 2, "need >=2 services for the subset test");
        let g1 = db.relation_graph(&services).unwrap();
        let g2 = db.relation_graph(&services).unwrap();
        assert!(Arc::ptr_eq(&g1, &g2), "no writes -> cached Arc reused");

        let c = db.intern_tag(&Tag::parse("c").unwrap()).unwrap();
        db.add_sibling(c, b, local).unwrap();
        let g3 = db.relation_graph(&services).unwrap();
        assert!(!Arc::ptr_eq(&g2, &g3), "sibling write -> rebuilt");
        assert_eq!(g3.siblings().get(&c), Some(&b));

        let g4 = db.relation_graph(&services[..1]).unwrap();
        assert!(!Arc::ptr_eq(&g3, &g4), "different service set -> rebuilt");
    }

    #[test]
    fn shared_cache_builds_relation_graph_once_across_connections() {
        // Two read-only connections sharing one cache must return the SAME
        // `Arc<RelationGraph>` — the ~600MB cold build happens once, not per
        // connection (#70).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.db");
        let writer = Db::open(&path).unwrap();
        let a = writer.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        let b = writer.intern_tag(&Tag::parse("b").unwrap()).unwrap();
        writer
            .add_sibling(a, b, writer.local_service_id().unwrap())
            .unwrap();
        drop(writer);

        let cache = Db::new_relation_cache();
        let conn1 = Db::open_readonly_with_cache(&path, cache.clone()).unwrap();
        let conn2 = Db::open_readonly_with_cache(&path, cache).unwrap();
        let services = conn1.included_services(ReadScope::Merged).unwrap();

        let g1 = conn1.relation_graph(&services).unwrap();
        let g2 = conn2.relation_graph(&services).unwrap();
        assert!(
            Arc::ptr_eq(&g1, &g2),
            "shared cache -> graph built once, both connections reuse it"
        );
        assert_eq!(g1.siblings().get(&a), Some(&b));

        // A connection with its OWN cache rebuilds independently.
        let lonely = Db::open_readonly(&path).unwrap();
        let g3 = lonely.relation_graph(&services).unwrap();
        assert!(
            !Arc::ptr_eq(&g1, &g3),
            "separate cache -> independent build"
        );
    }

    #[test]
    fn shared_cache_keeps_distinct_service_scopes_resident() {
        // A `LocalOnly` detail read must not evict the warmed `Merged` graph
        // from the shared cache: toggling scopes should reuse both built graphs,
        // not re-trigger the ~600MB cold rebuild the change eliminates (#70).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.db");
        let writer = Db::open(&path).unwrap();
        let local = writer.local_service_id().unwrap();
        writer
            .add_shared_service("peer", "http://peer/", None)
            .unwrap();
        let a = writer.intern_tag(&Tag::parse("a").unwrap()).unwrap();
        let b = writer.intern_tag(&Tag::parse("b").unwrap()).unwrap();
        writer.add_sibling(a, b, local).unwrap();
        drop(writer);

        let cache = Db::new_relation_cache();
        let pool = Db::open_readonly_with_cache(&path, cache.clone()).unwrap();
        let lane = Db::open_readonly_with_cache(&path, cache).unwrap();

        let merged = pool.included_services(ReadScope::Merged).unwrap();
        let local_only = pool.included_services(ReadScope::LocalOnly).unwrap();
        assert_ne!(
            merged, local_only,
            "need distinct service scopes for the coexistence test"
        );

        let m1 = pool.relation_graph(&merged).unwrap();
        // A local-only read on the tag lane builds a second graph...
        let _l = lane.relation_graph(&local_only).unwrap();
        // ...which must NOT have evicted the merged graph.
        let m2 = pool.relation_graph(&merged).unwrap();
        assert!(
            Arc::ptr_eq(&m1, &m2),
            "merged graph survives a local-only build -> no cross-scope thrash"
        );
    }

    #[test]
    fn tag_namespace_counts_track_completion_membership() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let f = insert_named(&db, b"n", "n.png");
        let reimu = db
            .intern_tag(&Tag::parse("character:reimu").unwrap())
            .unwrap();
        let marisa = db
            .intern_tag(&Tag::parse("character:marisa").unwrap())
            .unwrap();

        // No mappings yet -> namespace absent.
        assert!(db.complete_namespaces("char", 10).unwrap().is_empty());

        // Two current mappings in the same namespace -> count 2.
        db.add_mapping(f, reimu, local).unwrap();
        db.add_mapping(f, marisa, local).unwrap();
        let out = db.complete_namespaces("char", 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].namespace, "character");
        assert_eq!(out[0].tag_count, 2);

        // Remove one tag -> its completion row leaves -> count drops to 1.
        db.remove_mapping(f, marisa, local).unwrap();
        assert_eq!(db.complete_namespaces("char", 10).unwrap()[0].tag_count, 1);

        // Remove the last -> namespace row is deleted entirely.
        db.remove_mapping(f, reimu, local).unwrap();
        assert!(db.complete_namespaces("char", 10).unwrap().is_empty());
    }

    // ── mapping_tool ────────────────────────────────────────────────────────

    // ── rejection store helpers ───────────────────────────────────────────────

    /// Open a fresh in-memory database. Convenience alias used by rejection tests.
    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// Seed one shared service + one file + one pulled mapping.
    /// Returns `(service_id, file_id, tag_id)`.
    fn seed_shared_mapping(db: &Db, repo_name: &str, tag_str: &str) -> (i64, i64, i64) {
        let svc = db
            .add_shared_service(repo_name, &format!("http://{repo_name}"), None)
            .unwrap();
        let hash = naiad_core::hash_bytes(repo_name.as_bytes());
        db.insert_file(
            &naiad_core::FileRecord::new(hash, format!("/lib/{repo_name}.png").into(), 1, None),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let file = db.file_id_by_hash(&hash).unwrap().unwrap();
        let tag = Tag::parse(tag_str).unwrap();
        let tag_id = db.intern_tag(&tag).unwrap();
        db.merge_pulled_mappings(svc, &[(hash, vec![tag])]).unwrap();
        (svc, file, tag_id)
    }

    // ── rejection store tests ─────────────────────────────────────────────────

    #[test]
    fn rejection_round_trips_idempotently_and_undoes() {
        let db = test_db();
        let (svc, file, tag) = seed_shared_mapping(&db, "repo", "series:metroid");

        assert!(db.list_rejections(None).unwrap().is_empty());
        db.add_rejection(svc, file, tag, None).unwrap();
        db.add_rejection(svc, file, tag, None).unwrap(); // re-reject is a no-op
        let rs = db.list_rejections(None).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(
            (rs[0].service_id, rs[0].file_id, rs[0].tag_id),
            (svc, file, tag)
        );

        let m = db.reject_matcher().unwrap();
        assert!(m.is_rejected(svc, file, tag));
        assert!(!m.is_rejected(svc, file, tag + 1));

        db.remove_rejection(svc, file, tag).unwrap();
        db.remove_rejection(svc, file, tag).unwrap(); // undo is idempotent
        assert!(db.list_rejections(None).unwrap().is_empty());
        assert!(!db.reject_matcher().unwrap().is_rejected(svc, file, tag));
    }

    // ── rejection filter tests (Task 2) ──────────────────────────────────────

    /// Get the blake3 hash of an already-inserted file.
    fn file_hash_of(db: &Db, file_id: i64) -> Hash {
        db.file_hash(file_id).unwrap().unwrap()
    }

    /// Resolve `tag_id` to its `"namespace:subtag"` string.
    #[allow(dead_code)]
    fn tag_of(db: &Db, tag_id: i64) -> String {
        db.conn
            .query_row(
                "SELECT namespace || ':' || subtag FROM tags WHERE id = ?1",
                params![tag_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    }

    /// True iff `display_tags_detailed` (the primary detailed-tags fn) includes `tag_str`.
    fn detailed_has(db: &Db, file_id: i64, tag_str: &str) -> bool {
        db.display_tags_detailed(file_id, ReadScope::Merged)
            .unwrap()
            .iter()
            .any(|td| td.tag.to_string() == tag_str)
    }

    /// True iff `display_tags_of` (the second detail fn) includes `tag_str`.
    fn detailed2_has(db: &Db, file_id: i64, tag_str: &str) -> bool {
        db.display_tags_of(file_id, ReadScope::Merged)
            .unwrap()
            .iter()
            .any(|t| t.tag.to_string() == tag_str)
    }

    /// True iff effective (expanded) search finds the file with `hash` via `tag_str`.
    fn search_matches(db: &Db, tag_str: &str, hash: &Hash) -> bool {
        let q = Query {
            predicates: vec![Predicate::Tag(
                Tag::parse(tag_str).unwrap(),
                MatchMode::Expanded,
            )],
        };
        db.search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap()
            .iter()
            .any(|fl| &fl.hash == hash)
    }

    /// True iff `tags_of` (raw listing, no matchers) includes `tag_str`.
    fn raw_has(db: &Db, file_id: i64, tag_str: &str) -> bool {
        db.tags_of(file_id)
            .unwrap()
            .iter()
            .any(|t| t.to_string() == tag_str)
    }

    /// True iff raw (non-expanded) search finds the file with `hash` via `tag_str`.
    fn raw_search_matches(db: &Db, tag_str: &str, hash: &Hash) -> bool {
        let q = Query {
            predicates: vec![Predicate::Tag(
                Tag::parse(tag_str).unwrap(),
                MatchMode::Expanded,
            )],
        };
        db.search(&q, ReadScope::Merged, Expansion::Raw)
            .unwrap()
            .iter()
            .any(|fl| &fl.hash == hash)
    }

    /// `display_tags_detailed` surfaces the shared service name(s) for pulled tags
    /// and an empty services list for purely local tags. Regression test for the
    /// post-pivot ghost-reject flow that depends on this field.
    #[test]
    fn display_tags_detailed_services_field_regression() {
        let db = test_db();

        // Pulled tag: service "ptr" carries "char:samus" for the file.
        let (_svc, file_id, _tag_id) = seed_shared_mapping(&db, "ptr", "char:samus");
        let detailed = db
            .display_tags_detailed(file_id, ReadScope::Merged)
            .unwrap();
        let entry = detailed
            .iter()
            .find(|td| td.tag.to_string() == "char:samus")
            .expect("char:samus must be in detailed list");
        assert_eq!(entry.presence, TagPresence::Pulled);
        assert_eq!(
            entry.services,
            vec!["ptr".to_string()],
            "pulled tag must carry its shared service name"
        );

        // Local tag: add via local service — services must be empty.
        let local_svc = db.local_service_ids().unwrap().into_iter().next().unwrap();
        let local_tag = Tag::parse("char:ridley").unwrap();
        let local_tag_id = db.intern_tag(&local_tag).unwrap();
        db.add_mapping(file_id, local_tag_id, local_svc).unwrap();
        let detailed2 = db
            .display_tags_detailed(file_id, ReadScope::Merged)
            .unwrap();
        let local_entry = detailed2
            .iter()
            .find(|td| td.tag.to_string() == "char:ridley")
            .expect("char:ridley must be in detailed list");
        assert_eq!(local_entry.presence, TagPresence::Local);
        assert!(
            local_entry.services.is_empty(),
            "local tag must have empty services list"
        );
    }

    /// A rejected mapping vanishes from all effective surfaces (display + search)
    /// but remains accessible via raw listing and raw search.
    /// `remove_rejection` restores it to effective surfaces.
    #[test]
    fn rejected_mapping_leaves_display_and_search_but_not_raw() {
        let db = test_db();
        let (svc, file, tag_id) = seed_shared_mapping(&db, "rej_repo", "series:metroid");
        let hash = file_hash_of(&db, file);

        // Visible everywhere before rejection.
        assert!(
            detailed_has(&db, file, "series:metroid"),
            "display_tags_detailed must show the mapping before rejection"
        );
        assert!(
            detailed2_has(&db, file, "series:metroid"),
            "display_tags_of must show the mapping before rejection"
        );
        assert!(
            search_matches(&db, "series:metroid", &hash),
            "effective search must find the file before rejection"
        );
        assert!(
            raw_has(&db, file, "series:metroid"),
            "raw listing must show the mapping before rejection"
        );

        db.add_rejection(svc, file, tag_id, None).unwrap();

        // Gone from every effective surface...
        assert!(
            !detailed_has(&db, file, "series:metroid"),
            "display_tags_detailed must hide a rejected mapping"
        );
        assert!(
            !detailed2_has(&db, file, "series:metroid"),
            "display_tags_of must hide a rejected mapping"
        );
        assert!(
            !search_matches(&db, "series:metroid", &hash),
            "display and search must agree: rejected mapping hidden from search"
        );
        // ...but RAW IS UNTOUCHED (maxim #7 — load-bearing).
        assert!(
            raw_has(&db, file, "series:metroid"),
            "tags_of (raw listing) must NOT be affected by rejection — raw stays raw"
        );
        assert!(
            raw_search_matches(&db, "series:metroid", &hash),
            "raw search must NOT be affected by rejection — raw stays raw"
        );

        // Undo restores to effective surfaces.
        db.remove_rejection(svc, file, tag_id).unwrap();
        assert!(
            detailed_has(&db, file, "series:metroid"),
            "display_tags_detailed must show the mapping after remove_rejection"
        );
        assert!(
            search_matches(&db, "series:metroid", &hash),
            "search must find the file again after remove_rejection"
        );
    }

    // ── contributor identity tests (migration 0029) ───────────────────────────

    // ── filed petitions tests (migration 0029) ────────────────────────────────

    // ── migration 0030 scenario test ──────────────────────────────────────────

    /// Pre-pivot scenario test for migration 0030.
    ///
    /// Builds an in-memory DB through migrations 0001-0029, seeds a
    /// representative slice of pre-pivot state (pulled supporters, petitions,
    /// author trust, mixed block rules, local + shared mappings, a rejection,
    /// a non-NULL mapping_cursor), then applies migration 0030 and asserts the
    /// full pivot invariant set.
    #[test]
    fn migration_0030_cleans_trust_tables_and_wipes_shared_mappings() {
        let mut conn = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut conn, 29).unwrap();

        // Service id=1 ('my tags', scope='local') is seeded by migration 0002.
        // Add a shared service with a non-NULL mapping_cursor.
        conn.execute(
            "INSERT INTO services (name, scope, url, mapping_cursor)
             VALUES ('repo', 'shared', 'http://x', 42)",
            [],
        )
        .unwrap();
        let shared_svc: i64 = conn
            .query_row("SELECT id FROM services WHERE scope='shared'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // One file.
        conn.execute(
            "INSERT INTO files (blake3, size, state, imported_at)
             VALUES ('aabbccdd', 1, 'active', 0)",
            [],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE blake3='aabbccdd'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // Two tags: one gets a local mapping, the other a shared mapping.
        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('series', 'zelda')",
            [],
        )
        .unwrap();
        let tag_local: i64 = conn
            .query_row("SELECT id FROM tags WHERE subtag='zelda'", [], |r| r.get(0))
            .unwrap();

        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('series', 'metroid')",
            [],
        )
        .unwrap();
        let tag_shared: i64 = conn
            .query_row("SELECT id FROM tags WHERE subtag='metroid'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // Local mapping (service_id=1) — must survive migration 0030.
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![file_id, tag_local],
        )
        .unwrap();

        // Shared mapping (service_id=shared_svc) — must be wiped by migration 0030.
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, ?3, 'current', 0)",
            params![file_id, tag_shared, shared_svc],
        )
        .unwrap();

        // Rejection for the shared mapping — must survive migration 0030.
        conn.execute(
            "INSERT INTO mapping_rejections (service_id, file_id, tag_id, kind, created_at)
             VALUES (?1, ?2, ?3, 'mapping', 0)",
            params![shared_svc, file_id, tag_shared],
        )
        .unwrap();

        // Block rules: one 'tag' kind (must survive), one 'author' kind (must be deleted).
        conn.execute(
            "INSERT INTO block_rules (kind, target, created_at)
             VALUES ('tag', 'series:metroid', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO block_rules (kind, target, created_at)
             VALUES ('author', 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef', 0)",
            [],
        )
        .unwrap();

        // Trust floor app_setting — must be deleted by migration 0030.
        conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('trust_floor', '5')",
            [],
        )
        .unwrap();

        // Filed petition (0029 table) — table must be dropped.
        conn.execute(
            "INSERT INTO filed_petitions (service_id, hash, tag, petitioner, status, filed_at)
             VALUES (?1, 'aabbccdd', 'series:metroid', 'abc', 'open', 0)",
            params![shared_svc],
        )
        .unwrap();

        // Mapping supporter (0026 table) — table must be dropped.
        conn.execute(
            "INSERT INTO mapping_supporters (file_id, tag_id, service_id, author, created_at)
             VALUES (?1, ?2, ?3, 'abc123', 0)",
            params![file_id, tag_shared, shared_svc],
        )
        .unwrap();

        // ── Apply migration 0030 ──────────────────────────────────────────────
        MIGRATIONS.to_version(&mut conn, 30).unwrap();

        // (a) Rejection survives.
        let rej_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mapping_rejections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rej_count, 1,
            "mapping_rejections row must survive migration 0030"
        );

        // (b) Tag block survives; author block is gone.
        let tag_block: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM block_rules WHERE kind='tag'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_block, 1, "tag block_rule must survive migration 0030");
        let author_block: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM block_rules WHERE kind='author'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            author_block, 0,
            "author block_rule must be deleted by migration 0030"
        );

        // (c) Local-service mapping survives.
        let local_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mappings WHERE service_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            local_count, 1,
            "local-service mapping must survive migration 0030"
        );

        // (d) Shared-service mappings wiped; mapping_cursor reset to NULL.
        let shared_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mappings WHERE service_id=?1",
                params![shared_svc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            shared_count, 0,
            "shared-service mappings must be wiped by migration 0030"
        );
        let cursor: Option<i64> = conn
            .query_row(
                "SELECT mapping_cursor FROM services WHERE id=?1",
                params![shared_svc],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            cursor.is_none(),
            "mapping_cursor must be reset to NULL for shared services"
        );

        // (e) Dropped tables no longer exist.
        for table in &[
            "mapping_supporters",
            "rejection_tools",
            "filed_petitions",
            "author_trust",
            "trust_score_version",
            "tools",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                exists, 0,
                "table `{table}` must not exist after migration 0030"
            );
        }

        // contributor_mode column gone from services.
        let mode_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('services') WHERE name='contributor_mode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            mode_col, 0,
            "services.contributor_mode must be dropped by migration 0030"
        );

        // tool_id column gone from staged_mappings.
        let tool_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('staged_mappings') WHERE name='tool_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            tool_col, 0,
            "staged_mappings.tool_id must be dropped by migration 0030"
        );

        // author column gone from mappings.
        let author_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('mappings') WHERE name='author'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            author_col, 0,
            "mappings.author must be dropped by migration 0030"
        );
    }

    // ── migration 0031 scenario tests (issue #77) ─────────────────────────────

    /// Canonical rows must survive migration 0031 completely unchanged — the
    /// migration is a no-op on any database written by a correct build.
    #[test]
    fn migration_0031_is_noop_on_canonical_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut conn, 30).unwrap();

        // One file + one canonical tag + one mapping.
        conn.execute(
            "INSERT INTO files (blake3, size, state, imported_at)
             VALUES ('aabb1', 1, 'active', 0)",
            [],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE blake3='aabb1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('character', 'samus')",
            [],
        )
        .unwrap();
        let tag_id: i64 = conn
            .query_row(
                "SELECT id FROM tags WHERE namespace='character' AND subtag='samus'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![file_id, tag_id],
        )
        .unwrap();

        let tags_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();

        // Apply migration 0031.
        MIGRATIONS.to_version(&mut conn, 31).unwrap();

        // Tag count unchanged.
        let tags_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            tags_before, tags_after,
            "canonical row count must not change"
        );

        // Row still has the original (namespace, subtag).
        let (ns, sub): (String, String) = conn
            .query_row(
                "SELECT namespace, subtag FROM tags WHERE id = ?1",
                params![tag_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ns, "character");
        assert_eq!(sub, "samus");

        // Mapping survives.
        let map_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mappings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(map_count, 1, "mapping must survive unchanged");

        // Completion count is correct.
        let compl: i64 = conn
            .query_row(
                "SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(compl, 1, "completion count must be 1");
    }

    /// A non-canonical row that canonicalizes onto an existing canonical row
    /// must be merged: its mappings are de-duped onto the canonical id, the
    /// non-canonical row is deleted, and completion counts are rebuilt correctly.
    #[test]
    fn migration_0031_merges_non_canonical_collision() {
        let mut conn = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut conn, 30).unwrap();

        // Two files.
        conn.execute(
            "INSERT INTO files (blake3, size, state, imported_at) VALUES ('file1', 1, 'active', 0)",
            [],
        )
        .unwrap();
        let file1: i64 = conn
            .query_row("SELECT id FROM files WHERE blake3='file1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO files (blake3, size, state, imported_at) VALUES ('file2', 1, 'active', 0)",
            [],
        )
        .unwrap();
        let file2: i64 = conn
            .query_row("SELECT id FROM files WHERE blake3='file2'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // Canonical tag ('', 'hello') mapped to file1.
        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('', 'hello')",
            [],
        )
        .unwrap();
        let tag_hello: i64 = conn
            .query_row(
                "SELECT id FROM tags WHERE namespace='' AND subtag='hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![file1, tag_hello],
        )
        .unwrap();

        // Non-canonical tag ('', 'HELLO') — uppercase, not producible by Tag::parse.
        // It canonicalizes to ('', 'hello') = tag_hello.
        // Mapped to both file1 (will conflict) and file2 (will be moved).
        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('', 'HELLO')",
            [],
        )
        .unwrap();
        let tag_hello_upper: i64 = conn
            .query_row(
                "SELECT id FROM tags WHERE namespace='' AND subtag='HELLO'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![file1, tag_hello_upper],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![file2, tag_hello_upper],
        )
        .unwrap();

        // Apply migration 0031.
        MIGRATIONS.to_version(&mut conn, 31).unwrap();

        // Non-canonical tag must be gone.
        let noncanon: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags WHERE subtag='HELLO'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(noncanon, 0, "non-canonical 'HELLO' tag must be deleted");

        // Canonical tag still exists.
        let canon: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tags WHERE namespace='' AND subtag='hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(canon, 1, "canonical 'hello' tag must survive");

        // Both file1 and file2 are mapped to tag_hello (file2's mapping moved,
        // file1's conflicting mapping was dropped as a dedup).
        let map_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mappings WHERE tag_id = ?1",
                params![tag_hello],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            map_count, 2,
            "both file mappings must survive under canonical tag_id; got {map_count}"
        );

        // Total mapping count: no phantom rows.
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM mappings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            total, 2,
            "exactly 2 distinct mappings survive (no duplicates); got {total}"
        );

        // Completion count rebuilt to 2.
        let compl: i64 = conn
            .query_row(
                "SELECT current_count FROM tag_completion_counts WHERE tag_id = ?1",
                params![tag_hello],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            compl, 2,
            "completion count must be 2 after merge; got {compl}"
        );
    }

    /// A non-canonical tag referenced by sibling and parent edges must have
    /// those edges re-pointed to the canonical id after migration 0031.
    #[test]
    fn migration_0031_repoints_sibling_and_parent_edges() {
        let mut conn = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut conn, 30).unwrap();

        // Canonical: ('', 'bad') and ('', 'ideal').
        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('', 'bad')",
            [],
        )
        .unwrap();
        let tag_bad: i64 = conn
            .query_row("SELECT id FROM tags WHERE subtag='bad'", [], |r| r.get(0))
            .unwrap();

        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('', 'ideal')",
            [],
        )
        .unwrap();
        let tag_ideal: i64 = conn
            .query_row("SELECT id FROM tags WHERE subtag='ideal'", [], |r| r.get(0))
            .unwrap();

        // Non-canonical: ('', 'BAD') — canonicalizes to ('', 'bad').
        conn.execute(
            "INSERT INTO tags (namespace, subtag) VALUES ('', 'BAD')",
            [],
        )
        .unwrap();
        let tag_bad_upper: i64 = conn
            .query_row("SELECT id FROM tags WHERE subtag='BAD'", [], |r| r.get(0))
            .unwrap();

        // Sibling: non-canonical BAD → ideal.
        conn.execute(
            "INSERT INTO tag_siblings (bad_tag_id, ideal_tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![tag_bad_upper, tag_ideal],
        )
        .unwrap();

        // Parent: non-canonical BAD → ideal.
        conn.execute(
            "INSERT INTO tag_parents (child_tag_id, parent_tag_id, service_id, status, created_at)
             VALUES (?1, ?2, 1, 'current', 0)",
            params![tag_bad_upper, tag_ideal],
        )
        .unwrap();

        MIGRATIONS.to_version(&mut conn, 31).unwrap();

        // Non-canonical tag gone.
        let gone: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags WHERE subtag='BAD'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(gone, 0, "'BAD' tag must be removed");

        // Sibling row repointed to canonical 'bad'.
        let sib_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tag_siblings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sib_count, 1, "sibling edge must survive");
        let (sib_bad, sib_ideal): (i64, i64) = conn
            .query_row(
                "SELECT bad_tag_id, ideal_tag_id FROM tag_siblings LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            sib_bad, tag_bad,
            "sibling bad_tag_id must point to canonical 'bad' (id={}); got {}",
            tag_bad, sib_bad
        );
        assert_eq!(sib_ideal, tag_ideal);

        // Parent row repointed to canonical 'bad'.
        let par_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tag_parents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(par_count, 1, "parent edge must survive");
        let (par_child, par_parent): (i64, i64) = conn
            .query_row(
                "SELECT child_tag_id, parent_tag_id FROM tag_parents LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            par_child, tag_bad,
            "parent child_tag_id must point to canonical 'bad' (id={}); got {}",
            tag_bad, par_child
        );
        assert_eq!(par_parent, tag_ideal);

        // No self-edges.
        let self_sibs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tag_siblings WHERE bad_tag_id = ideal_tag_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(self_sibs, 0, "no self-sibling edges must survive");
    }

    // ── complete_tags leading-colon fix tests (issue #77, spec §6c) ──────────

    /// Typing `:)` must surface the `":)"` subtag (parse-consistent), not the
    /// bare `)` subtag. Before the fix, `split_once(':')` yielded ("", ")") and
    /// matched the bare `)` row while missing the leading-colon one.
    #[test]
    fn complete_tags_leading_colon_emoticon_is_parse_consistent() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f1 = insert_named(&db, b"f1-paren", "f1.png");
        let f2 = insert_named(&db, b"f2-colon-paren", "f2.png");
        tag_file(&db, f1, svc, ")"); // stores namespace="" subtag=")"
        tag_file(&db, f2, svc, ":)"); // stores namespace="" subtag=":)"

        // ":)" typed → (None, ":)") → subtag LIKE ':)%' → finds ":)", NOT ")"
        let out = db.complete_tags(":)", 10, CompletionMode::Prefix).unwrap();
        let subs: Vec<&str> = out.iter().map(|t| t.subtag.as_str()).collect();
        assert!(
            subs.contains(&":)"),
            "':)' token must surface ':)' subtag; got {subs:?}"
        );
        assert!(
            !subs.contains(&")"),
            "':)' token must NOT surface bare ')' subtag; got {subs:?}"
        );
    }

    /// `::)` typed must complete to the same set as `:)` typed — both spell the
    /// same leading-colon emoticon in canonical vs double-colon form.
    #[test]
    fn complete_tags_double_colon_form_completes_same_as_single() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"f-colon-paren", "f.png");
        tag_file(&db, f, svc, ":)"); // stores ("", ":)")

        // "::)" → split_completion_token → rest=":)", rest has ':' → (None, ":)")
        let out = db.complete_tags("::)", 10, CompletionMode::Prefix).unwrap();
        let subs: Vec<&str> = out.iter().map(|t| t.subtag.as_str()).collect();
        assert!(
            subs.contains(&":)"),
            "'::)' token must surface ':)' subtag; got {subs:?}"
        );
    }

    /// Bare "sam" token still matches subtags across all namespaces (regression
    /// guard: the split_completion_token change must not break non-colon tokens).
    #[test]
    fn complete_tags_bare_token_still_matches_across_namespaces_after_fix() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"sam-fix", "sam.png");
        tag_file(&db, f, svc, "character:samus");
        tag_file(&db, f, svc, "samus_amiibo");
        let out = db.complete_tags("sam", 10, CompletionMode::Prefix).unwrap();
        let subs: Vec<&str> = out.iter().map(|t| t.subtag.as_str()).collect();
        assert!(
            subs.contains(&"samus"),
            "bare 'sam' should still match across namespaces; got {subs:?}"
        );
        assert!(subs.contains(&"samus_amiibo"));
    }

    /// "a:" token still namespace-filters (regression guard).
    #[test]
    fn complete_tags_namespace_colon_still_filters_namespace_after_fix() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let f = insert_named(&db, b"ns-fix", "ns.png");
        tag_file(&db, f, svc, "a:thing");
        tag_file(&db, f, svc, "b:other");
        let out = db.complete_tags("a:", 10, CompletionMode::Prefix).unwrap();
        let namespaces: Vec<&str> = out.iter().map(|t| t.namespace.as_str()).collect();
        assert!(
            namespaces.iter().all(|&ns| ns == "a"),
            "all results must be in namespace 'a'; got {:?}",
            out
        );
    }

    #[test]
    fn detach_keeps_tags_but_hides_the_service_from_subscribed_lists() {
        use naiad_core::{Hash, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let service_id = db.add_shared_service("ptr", "http://x", None).unwrap();
        let owned: Hash = hash_bytes(b"owned-bytes");
        db.insert_file(
            &naiad_core::FileRecord::new(owned, "/lib/a.txt".into(), 11, Some(1)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.merge_pulled_mappings(service_id, &[(owned, vec![Tag::parse("x:y").unwrap()])])
            .unwrap();
        let file_id = db.file_id_by_hash(&owned).unwrap().unwrap();

        db.detach_service(service_id).unwrap();

        assert!(
            db.shared_service_by_name("ptr").unwrap().is_none(),
            "detached service is not subscribed"
        );
        assert!(
            db.list_shared_services().unwrap().is_empty(),
            "detached service is absent from the subscribed list"
        );
        assert_eq!(
            db.tags_of(file_id).unwrap().len(),
            1,
            "its tags survive the detach"
        );
    }

    #[test]
    fn subscribe_reattaches_a_detached_service_under_the_same_id() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_shared_service("ptr", "http://old", None).unwrap();
        db.detach_service(id).unwrap();

        let re = db
            .subscribe_shared_service("ptr", "http://new", None)
            .unwrap();
        assert_eq!(re, id, "re-attach reuses the detached row");
        let svc = db.shared_service_by_name("ptr").unwrap().unwrap();
        assert_eq!(svc.url, "http://new");

        // A second subscribe on a live name is an error, not a silent overwrite.
        assert!(
            db.subscribe_shared_service("ptr", "http://other", None)
                .is_err()
        );
    }

    #[test]
    fn shared_service_name_taken_sees_detached_rows() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            !db.shared_service_name_taken("ptr").unwrap(),
            "absent name = not taken"
        );
        let id = db.add_shared_service("ptr", "http://x", None).unwrap();
        assert!(
            db.shared_service_name_taken("ptr").unwrap(),
            "attached row = taken"
        );
        db.detach_service(id).unwrap();
        assert!(
            db.shared_service_name_taken("ptr").unwrap(),
            "detached row must still be seen as taken"
        );
    }

    #[test]
    fn scoped_merge_touches_only_the_requested_files() {
        use naiad_core::{Hash, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://x", None).unwrap();
        let a: Hash = hash_bytes(b"file-a");
        let b: Hash = hash_bytes(b"file-b");
        let marker = db.next_scan_marker().unwrap();
        db.insert_file(
            &naiad_core::FileRecord::new(a, "/lib/a".into(), 1, Some(1)),
            marker,
        )
        .unwrap();
        db.insert_file(
            &naiad_core::FileRecord::new(b, "/lib/b".into(), 1, Some(1)),
            marker,
        )
        .unwrap();
        // Both files tagged by a full pull.
        db.merge_pulled_mappings(
            svc,
            &[
                (a, vec![Tag::parse("t:a").unwrap()]),
                (b, vec![Tag::parse("t:b").unwrap()]),
            ],
        )
        .unwrap();

        // Scoped merge for `a` only: upstream now has a new tag for a; b unmentioned.
        let stats = db
            .merge_pulled_mappings_for_files(
                svc,
                &[a],
                &[(a, vec![(Tag::parse("t:a2").unwrap(), None)])],
            )
            .unwrap();
        assert_eq!(stats.matched_files, 1);

        let fa = db.file_id_by_hash(&a).unwrap().unwrap();
        let fb = db.file_id_by_hash(&b).unwrap().unwrap();
        let a_tags: Vec<String> = db
            .tags_of(fa)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(
            a_tags,
            vec!["t:a2".to_string()],
            "a is authoritative-replaced"
        );
        assert_eq!(
            db.tags_of(fb).unwrap().len(),
            1,
            "b's tags on this service are untouched by a scoped pull of a"
        );

        // Idempotence: repeating the same scoped merge adds zero rows.
        let again = db
            .merge_pulled_mappings_for_files(
                svc,
                &[a],
                &[(a, vec![(Tag::parse("t:a2").unwrap(), None)])],
            )
            .unwrap();
        assert_eq!(again.mappings, 0, "repeat scoped merge is a no-op");
    }

    // ── dual-domain provenance (#151, migration 0034) ────────────────────────

    /// A repo service with two files, ready for domain-scoped merges.
    fn dual_domain_fixture() -> (Db, i64, Hash, Hash) {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://x", None).unwrap();
        let a: Hash = hash_bytes(b"dd-file-a");
        let b: Hash = hash_bytes(b"dd-file-b");
        let marker = db.next_scan_marker().unwrap();
        db.insert_file(&FileRecord::new(a, "/lib/a".into(), 1, Some(1)), marker)
            .unwrap();
        db.insert_file(&FileRecord::new(b, "/lib/b".into(), 1, Some(1)), marker)
            .unwrap();
        (db, svc, a, b)
    }

    fn tag_names(db: &Db, hash: &Hash) -> Vec<String> {
        let fid = db.file_id_by_hash(hash).unwrap().unwrap();
        let mut v: Vec<String> = db
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        v.sort();
        v
    }

    fn domains_mask(db: &Db, hash: &Hash, tag: &str) -> Option<i64> {
        let fid = db.file_id_by_hash(hash).unwrap().unwrap();
        db.conn
            .query_row(
                "SELECT m.domains FROM mappings m
                   JOIN tags t ON t.id = m.tag_id
                  WHERE m.file_id = ?1 AND t.namespace || ':' || t.subtag = ?2",
                params![fid, tag],
                |r| r.get::<_, i64>(0),
            )
            .ok()
    }

    /// Matrix row 1 + 2: both domains merge in one pull, neither deletes the
    /// other's rows, and a tag both supply carries both provenance bits.
    #[test]
    fn dual_domain_merges_are_independent_and_union_their_tags() {
        let (db, svc, a, _b) = dual_domain_fixture();

        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                a,
                vec![
                    (Tag::parse("t:native").unwrap(), None),
                    (Tag::parse("t:both").unwrap(), None),
                ],
            )],
        )
        .unwrap();
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(
                a,
                vec![
                    (Tag::parse("t:interop").unwrap(), None),
                    (Tag::parse("t:both").unwrap(), None),
                ],
            )],
        )
        .unwrap();

        assert_eq!(
            tag_names(&db, &a),
            vec!["t:both", "t:interop", "t:native"],
            "the sha256 leg must not delete the blake3 leg's rows"
        );
        assert_eq!(
            domains_mask(&db, &a, "t:native"),
            Some(DOMAIN_BIT_BLAKE3),
            "a blake3-only tag carries only the blake3 bit"
        );
        assert_eq!(
            domains_mask(&db, &a, "t:interop"),
            Some(DOMAIN_BIT_SHA256),
            "a sha256-only tag carries only the sha256 bit"
        );
        assert_eq!(
            domains_mask(&db, &a, "t:both"),
            Some(DOMAIN_BIT_BLAKE3 | DOMAIN_BIT_SHA256),
            "a tag both domains supply carries both bits in ONE row"
        );

        // No flapping: repeating the same pull is stable.
        for _ in 0..3 {
            db.merge_pulled_mappings_in_domain(
                svc,
                "blake3",
                &[(
                    a,
                    vec![
                        (Tag::parse("t:native").unwrap(), None),
                        (Tag::parse("t:both").unwrap(), None),
                    ],
                )],
            )
            .unwrap();
            db.merge_pulled_mappings_in_domain(
                svc,
                "sha256",
                &[(
                    a,
                    vec![
                        (Tag::parse("t:interop").unwrap(), None),
                        (Tag::parse("t:both").unwrap(), None),
                    ],
                )],
            )
            .unwrap();
        }
        assert_eq!(
            tag_names(&db, &a),
            vec!["t:both", "t:interop", "t:native"],
            "repeated dual-domain pulls must not flap"
        );
    }

    /// Matrix rows 3 + 4: a retraction in one domain leaves the other's rows
    /// alone, and a row both domains supplied survives one of them dropping it.
    #[test]
    fn retracting_in_one_domain_spares_the_other() {
        let (db, svc, a, _b) = dual_domain_fixture();
        let both = vec![(Tag::parse("t:both").unwrap(), None::<String>)];

        db.merge_pulled_mappings_in_domain(svc, "blake3", &[(a, both.clone())])
            .unwrap();
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(
                a,
                vec![
                    (Tag::parse("t:both").unwrap(), None),
                    (Tag::parse("t:interop").unwrap(), None),
                ],
            )],
        )
        .unwrap();

        // blake3 retracts t:both — sha256 still supplies it, so it survives.
        db.merge_pulled_mappings_in_domain(svc, "blake3", &[(a, vec![])])
            .unwrap();
        assert_eq!(
            tag_names(&db, &a),
            vec!["t:both", "t:interop"],
            "sha256 still supplies t:both, so it must survive blake3's retraction"
        );
        assert_eq!(
            domains_mask(&db, &a, "t:both"),
            Some(DOMAIN_BIT_SHA256),
            "only the sha256 bit remains"
        );

        // Now sha256 retracts it too — the last claim is gone, so is the row.
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(a, vec![(Tag::parse("t:interop").unwrap(), None)])],
        )
        .unwrap();
        assert_eq!(
            tag_names(&db, &a),
            vec!["t:interop"],
            "once no domain supplies it, the row is reaped"
        );
    }

    /// Matrix row 6: a repo that stops advertising sha256 has its sha256-only
    /// rows reaped by the next pull rather than left orphaned.
    #[test]
    fn dropping_a_domain_reaps_only_its_own_rows() {
        let (db, svc, a, _b) = dual_domain_fixture();
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(a, vec![(Tag::parse("t:native").unwrap(), None)])],
        )
        .unwrap();
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(a, vec![(Tag::parse("t:interop").unwrap(), None)])],
        )
        .unwrap();

        // Repo stops serving sha256: an empty authoritative sha256 merge.
        db.merge_pulled_mappings_in_domain(svc, "sha256", &[])
            .unwrap();

        assert_eq!(
            tag_names(&db, &a),
            vec!["t:native"],
            "sha256's rows are reaped; the blake3 domain is untouched"
        );
    }

    /// The BLAKE3 delta path must be domain-scoped too: a full-bucket clear
    /// covers the whole hash range, and would otherwise drop sha256 rows in it.
    #[test]
    fn blake3_delta_full_bucket_clear_spares_sha256_rows() {
        let (db, svc, a, _b) = dual_domain_fixture();
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(a, vec![(Tag::parse("t:interop").unwrap(), None)])],
        )
        .unwrap();
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(a, vec![(Tag::parse("t:native").unwrap(), None)])],
        )
        .unwrap();

        // A delta whose full bucket spans the entire hash space.
        let marker = db.max_file_id().unwrap();
        db.merge_mapping_delta(
            svc,
            "blake3",
            &[MappingDeltaInput {
                hash: a,
                tag: Tag::parse("t:fresh").unwrap(),
                status: MappingDeltaStatus::Current,
                seq: 1,
                origin: None,
            }],
            &[("0".repeat(64), "f".repeat(64))],
            1,
            marker,
        )
        .unwrap();

        assert_eq!(
            tag_names(&db, &a),
            vec!["t:fresh", "t:interop"],
            "the bucket clear drops the stale blake3 tag but must spare the sha256 one"
        );
        assert_eq!(
            domains_mask(&db, &a, "t:interop"),
            Some(DOMAIN_BIT_SHA256),
            "the sha256 row keeps its bit through a blake3 bucket clear"
        );
    }

    /// A delta tombstone retracts only the sending domain's claim.
    #[test]
    fn blake3_delta_tombstone_spares_a_sha256_supplied_row() {
        let (db, svc, a, _b) = dual_domain_fixture();
        let both = vec![(Tag::parse("t:both").unwrap(), None::<String>)];
        db.merge_pulled_mappings_in_domain(svc, "blake3", &[(a, both.clone())])
            .unwrap();
        db.merge_pulled_mappings_in_domain(svc, "sha256", &[(a, both.clone())])
            .unwrap();

        let marker = db.max_file_id().unwrap();
        db.merge_mapping_delta(
            svc,
            "blake3",
            &[MappingDeltaInput {
                hash: a,
                tag: Tag::parse("t:both").unwrap(),
                status: MappingDeltaStatus::Deleted,
                seq: 2,
                origin: None,
            }],
            &[],
            2,
            marker,
        )
        .unwrap();

        assert_eq!(
            tag_names(&db, &a),
            vec!["t:both"],
            "sha256 still supplies the tag, so a blake3 tombstone must not remove it"
        );
        assert_eq!(domains_mask(&db, &a, "t:both"), Some(DOMAIN_BIT_SHA256));
    }

    /// Matrix row 9: the file-scoped merges stay domain-independent, and the
    /// all-domain form keeps its pre-#151 whole-service semantics.
    #[test]
    fn file_scoped_merges_are_domain_independent() {
        let (db, svc, a, b) = dual_domain_fixture();

        db.merge_pulled_mappings_for_files_in_domain(
            svc,
            &[a],
            "blake3",
            &[(a, vec![(Tag::parse("t:native").unwrap(), None)])],
        )
        .unwrap();
        db.merge_pulled_mappings_for_files_in_domain(
            svc,
            &[a],
            "sha256",
            &[(a, vec![(Tag::parse("t:interop").unwrap(), None)])],
        )
        .unwrap();
        assert_eq!(
            tag_names(&db, &a),
            vec!["t:interop", "t:native"],
            "the second domain's scoped merge must not wipe the first's"
        );

        // The all-domain form is still authoritative across every domain.
        db.merge_pulled_mappings_for_files(
            svc,
            &[a],
            &[(a, vec![(Tag::parse("t:only").unwrap(), None)])],
        )
        .unwrap();
        assert_eq!(
            tag_names(&db, &a),
            vec!["t:only"],
            "the un-scoped file merge keeps whole-service semantics"
        );
        assert!(
            tag_names(&db, &b).is_empty(),
            "file b was never requested and stays untouched"
        );
    }

    /// Matrix row 9 (continued): `merge_pulled_mappings` — still used by
    /// non-sync callers — must keep wiping the whole service regardless of
    /// provenance.
    #[test]
    fn whole_service_merge_still_ignores_domain_provenance() {
        let (db, svc, a, _b) = dual_domain_fixture();
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(a, vec![(Tag::parse("t:interop").unwrap(), None)])],
        )
        .unwrap();

        db.merge_pulled_mappings(svc, &[(a, vec![Tag::parse("t:fresh").unwrap()])])
            .unwrap();
        assert_eq!(
            tag_names(&db, &a),
            vec!["t:fresh"],
            "the all-domain merge wipes sha256 rows too"
        );
    }

    /// Matrix row 8: migration 0034 discards PULLED rows (whose provenance is
    /// not inferable) and rewinds pull state so the next pull re-derives it,
    /// while leaving LOCAL rows — user data — alone with a valid mask.
    #[test]
    fn migration_0034_discards_pulled_rows_keeps_local_and_forces_a_full_repull() {
        let mut conn = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut conn, 33).unwrap();

        conn.execute(
            "INSERT INTO services (id, name, scope, url, priority)
             VALUES (9, 'ptr', 'shared', 'http://x', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (id, blake3, size, state, imported_at)
             VALUES (7, ?1, 1, 'active', 0)",
            params![format!("{:0<64}", "ab")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, namespace, subtag) VALUES (5, 't', 'x')",
            [],
        )
        .unwrap();
        // One pulled row (service 9, subscribed) and one local row (service 1).
        conn.execute(
            "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at)
             VALUES (7, 5, 9, 'current', 0), (7, 5, 1, 'current', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO service_domain_pull_state
                 (service_id, domain, mapping_cursor, last_pull_file_marker)
             VALUES (9, 'blake3', 42, 100)",
            [],
        )
        .unwrap();

        MIGRATIONS.to_version(&mut conn, 34).unwrap();

        let pulled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mappings WHERE service_id = 9",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            pulled, 0,
            "pulled rows carry no inferable provenance, so they are re-derived, not guessed"
        );

        let local_mask: i64 = conn
            .query_row(
                "SELECT domains FROM mappings WHERE service_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            local_mask, DOMAIN_BIT_BLAKE3,
            "local rows survive with a valid non-zero mask"
        );

        let (cursor, marker): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT mapping_cursor, last_pull_file_marker
                   FROM service_domain_pull_state WHERE service_id = 9",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cursor, Some(0), "cursor is rewound");
        assert_eq!(
            marker, None,
            "a NULL marker is what makes the next pull request every bucket in full"
        );
    }

    // ── export-mappings ──────────────────────────────────────────────────────

    /// Two files (one active, one trashed), each with a current local mapping.
    /// The active file also gets a deleted-status local mapping and a current
    /// mapping on a shared-scope service. Assert that only the active file's
    /// current local tag(s) are yielded, and check tag round-trip for both a
    /// namespaced and an unnamespaced form.
    #[test]
    fn for_each_active_local_mapping_filters_correctly() {
        use naiad_core::{FileRecord, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let marker = db.next_scan_marker().unwrap();

        // Active file: will have current local mappings (should appear).
        let active_hash = hash_bytes(b"active-file-export");
        db.insert_file(
            &FileRecord::new(active_hash, "/active.png".into(), 11, None),
            marker,
        )
        .unwrap();
        let active_id = db.file_id_by_hash(&active_hash).unwrap().unwrap();

        // Trashed file: inserted directly because insert_file always sets 'active'.
        let trashed_hex = format!("{:0<64}", "deadbeef");
        db.conn
            .execute(
                "INSERT INTO files (blake3, size, state, imported_at) \
                 VALUES (?1, 1, 'trashed', 0)",
                params![trashed_hex],
            )
            .unwrap();
        let trashed_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM files WHERE blake3 = ?1",
                params![trashed_hex],
                |r| r.get(0),
            )
            .unwrap();

        // Active file — namespaced current local tag (should appear).
        let t_ns = Tag::parse("character:samus").unwrap();
        let t_ns_id = db.intern_tag(&t_ns).unwrap();
        db.add_mapping(active_id, t_ns_id, svc).unwrap();

        // Active file — unnamespaced current local tag (should appear).
        let t_unns = Tag::parse("shield").unwrap();
        let t_unns_id = db.intern_tag(&t_unns).unwrap();
        db.add_mapping(active_id, t_unns_id, svc).unwrap();

        // Active file — deleted-status local mapping (should NOT appear).
        let t_del = Tag::parse("rating:explicit").unwrap();
        let t_del_id = db.intern_tag(&t_del).unwrap();
        db.conn
            .execute(
                "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at) \
                 VALUES (?1, ?2, ?3, 'deleted', 0)",
                params![active_id, t_del_id, svc],
            )
            .unwrap();

        // Active file — current mapping on a shared-scope service (should NOT appear).
        db.conn
            .execute(
                "INSERT INTO services (name, scope) VALUES ('testrepo', 'shared')",
                [],
            )
            .unwrap();
        let shared_svc: i64 = db
            .conn
            .query_row(
                "SELECT id FROM services WHERE scope = 'shared' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let t_shared = Tag::parse("series:metroid").unwrap();
        let t_shared_id = db.intern_tag(&t_shared).unwrap();
        db.add_mapping(active_id, t_shared_id, shared_svc).unwrap();

        // Trashed file — current local mapping (should NOT appear).
        let t_trashed = Tag::parse("character:link").unwrap();
        let t_trashed_id = db.intern_tag(&t_trashed).unwrap();
        db.conn
            .execute(
                "INSERT INTO mappings (file_id, tag_id, service_id, status, created_at) \
                 VALUES (?1, ?2, ?3, 'current', 0)",
                params![trashed_id, t_trashed_id, svc],
            )
            .unwrap();

        let mut results: Vec<(String, String)> = Vec::new();
        db.for_each_active_local_mapping(|hash, tag| {
            results.push((hash.to_owned(), tag.to_owned()));
            Ok(())
        })
        .unwrap();

        // Only the active file's current local tags (2) should appear.
        assert_eq!(
            results.len(),
            2,
            "only active-file current-local tags; got {results:?}"
        );
        let expected_hash = active_hash.to_hex();
        for (h, _) in &results {
            assert_eq!(h, &expected_hash, "all rows must be for the active file");
        }
        let tags: Vec<&str> = results.iter().map(|(_, t)| t.as_str()).collect();
        // Namespaced tag round-trip.
        assert!(
            tags.contains(&"character:samus"),
            "namespaced tag must round-trip; got {tags:?}"
        );
        // Unnamespaced tag round-trip.
        assert!(
            tags.contains(&"shield"),
            "unnamespaced tag must round-trip; got {tags:?}"
        );
    }

    /// A fake repo database (has `repo_mappings`, lacks `files`) must be
    /// rejected by `assert_client_library` with a message identifying it as a
    /// repo database — matching the style of the server-side guard.
    #[test]
    fn assert_client_library_rejects_repo_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.db");
        // Create a fake repo database: only has repo_mappings, no files table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE repo_mappings \
                 (hash TEXT, tag TEXT, status TEXT, seq INTEGER);",
            )
            .unwrap();
        }
        let db = Db::open_readonly(&path).unwrap();
        let err = format!("{:#}", db.assert_client_library(&path).unwrap_err());
        assert!(
            err.contains("repo database"),
            "error must identify the file as a repo database; got: {err}"
        );
    }

    #[test]
    fn scoped_merge_authoritative_removal_clears_tags_when_no_entry_sent() {
        use naiad_core::{Hash, Tag, hash_bytes};

        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://x", None).unwrap();
        let a: Hash = hash_bytes(b"file-a-removal");
        let b: Hash = hash_bytes(b"file-b-removal");
        let marker = db.next_scan_marker().unwrap();
        db.insert_file(
            &naiad_core::FileRecord::new(a, "/lib/ar".into(), 1, Some(1)),
            marker,
        )
        .unwrap();
        db.insert_file(
            &naiad_core::FileRecord::new(b, "/lib/br".into(), 1, Some(1)),
            marker,
        )
        .unwrap();
        // Full pull: both files have a tag.
        db.merge_pulled_mappings(
            svc,
            &[
                (a, vec![Tag::parse("t:a").unwrap()]),
                (b, vec![Tag::parse("t:b").unwrap()]),
            ],
        )
        .unwrap();

        // Scoped merge for `b` only with an empty entries list: upstream now
        // has no tags for b. This is an authoritative removal — b's mapping
        // must be cleared. a is not in `hashes`, so it is untouched.
        let stats = db.merge_pulled_mappings_for_files(svc, &[b], &[]).unwrap();

        let fa = db.file_id_by_hash(&a).unwrap().unwrap();
        let fb = db.file_id_by_hash(&b).unwrap().unwrap();
        assert_eq!(
            db.tags_of(fb).unwrap().len(),
            0,
            "b's tags are cleared by authoritative removal"
        );
        assert_eq!(db.tags_of(fa).unwrap().len(), 1, "a's tags are untouched");
        // matched_files counts only files for which upstream sent an entry;
        // b was cleared but had no entry, so matched_files is 0.
        assert_eq!(
            stats.matched_files, 0,
            "no entries sent — matched_files is 0"
        );
    }

    #[test]
    fn relation_completion_merges_alias_counts() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        // canonical `character:samus`; aliases `samus_aran`, `samus`.
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let a1 = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        let a2 = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        db.add_sibling(a1, ideal, local).unwrap();
        db.add_sibling(a2, ideal, local).unwrap();
        // raw counts: ideal 1, a1 2, a2 3  -> merged 6.
        let f1 = insert_named(&db, b"c1", "c1.png");
        tag_file(&db, f1, local, "character:samus");
        let f2 = insert_named(&db, b"c2", "c2.png");
        tag_file(&db, f2, local, "samus_aran");
        let f3 = insert_named(&db, b"c3", "c3.png");
        tag_file(&db, f3, local, "samus_aran");
        let f4 = insert_named(&db, b"c4", "c4.png");
        tag_file(&db, f4, local, "samus");
        let f5 = insert_named(&db, b"c5", "c5.png");
        tag_file(&db, f5, local, "samus");
        let f6 = insert_named(&db, b"c6", "c6.png");
        tag_file(&db, f6, local, "samus");

        let services = db.included_services(ReadScope::Merged).unwrap();
        let overlay = db.relation_completion(&services).unwrap();
        assert!(!overlay.is_empty());
        assert_eq!(overlay.canonical_of(a1), Some(ideal));
        assert_eq!(overlay.canonical_of(a2), Some(ideal));
        assert_eq!(
            overlay.canonical_of(ideal),
            None,
            "the canonical is not an alias"
        );
        assert_eq!(overlay.merged_count(ideal), Some(6));
    }

    #[test]
    fn relation_completion_is_empty_without_siblings() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let f = insert_named(&db, b"n1", "n1.png");
        tag_file(&db, f, local, "character:samus");
        let services = db.included_services(ReadScope::Merged).unwrap();
        let overlay = db.relation_completion(&services).unwrap();
        assert!(overlay.is_empty());
    }

    // ── A3: relation-aware complete_tags merge tests ──────────────────────────

    fn suggestion_names(rows: &[TagSuggestion]) -> Vec<String> {
        rows.iter()
            .map(|s| {
                Tag {
                    namespace: s.namespace.clone(),
                    subtag: s.subtag.clone(),
                }
                .to_string()
            })
            .collect()
    }

    #[test]
    fn complete_tags_merges_alias_into_canonical_row() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db
            .intern_tag(&Tag::parse("character:samus_aran").unwrap())
            .unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        // ideal raw 1, alias raw 2 -> merged 3.
        let f1 = insert_named(&db, b"m1", "m1.png");
        tag_file(&db, f1, local, "character:samus");
        let f2 = insert_named(&db, b"m2", "m2.png");
        tag_file(&db, f2, local, "character:samus_aran");
        let f3 = insert_named(&db, b"m3", "m3.png");
        tag_file(&db, f3, local, "character:samus_aran");
        let rows = db
            .complete_tags("character:samus", 20, CompletionMode::Prefix)
            .unwrap();
        // Only the canonical appears, and its count is the merged 3.
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].count, 3);
    }

    #[test]
    fn complete_tags_alias_spelling_surfaces_canonical() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f = insert_named(&db, b"s1", "s1.png");
        tag_file(&db, f, local, "samus_aran");
        // Typing the alias spelling surfaces the canonical, never the alias row.
        let rows = db
            .complete_tags("samus_a", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].count, 1);
    }

    #[test]
    fn complete_tags_zero_raw_ideal_completes_but_zero_raw_parent_does_not() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        // Ideal `character:samus` has ZERO raw mappings; all usage is the alias.
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        // Parent `series:metroid` has zero raw mappings (umbrella).
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_parent(ideal, parent, local).unwrap();
        let f = insert_named(&db, b"z1", "z1.png");
        tag_file(&db, f, local, "samus_aran");
        // Ideal completes (merged 1 via its alias).
        let samus = db
            .complete_tags("character:sam", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(
            suggestion_names(&samus),
            vec!["character:samus".to_string()]
        );
        // Bare parent does NOT complete (absent from base scan, never injected).
        let metroid = db
            .complete_tags("series:met", 20, CompletionMode::Prefix)
            .unwrap();
        assert!(metroid.is_empty(), "zero-raw parent must not complete");
    }

    // ── #116: alias_source on alias-surfaced completion rows ─────────────────

    #[test]
    fn complete_tags_alias_surfaced_row_carries_alias_source() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f = insert_named(&db, b"as1", "as1.png");
        tag_file(&db, f, local, "samus_aran");
        // Fragment matches only the alias -> canonical injected via step 4.
        let rows = db
            .complete_tags("samus_a", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].alias_source.as_deref(), Some("samus_aran"));
    }

    #[test]
    fn complete_tags_alias_source_prefers_highest_raw_count() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let a1 = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        let a2 = db.intern_tag(&Tag::parse("samus_a").unwrap()).unwrap();
        db.add_sibling(a1, ideal, local).unwrap();
        db.add_sibling(a2, ideal, local).unwrap();
        // a1 raw 1, a2 raw 2 -> a2 wins.
        let f1 = insert_named(&db, b"hp1", "hp1.png");
        tag_file(&db, f1, local, "samus_aran");
        let f2 = insert_named(&db, b"hp2", "hp2.png");
        tag_file(&db, f2, local, "samus_a");
        let f3 = insert_named(&db, b"hp3", "hp3.png");
        tag_file(&db, f3, local, "samus_a");
        let rows = db
            .complete_tags("samus_", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].alias_source.as_deref(), Some("samus_a"));
    }

    #[test]
    fn complete_tags_alias_source_tie_breaks_lexicographically() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let a1 = db.intern_tag(&Tag::parse("samus_zeta").unwrap()).unwrap();
        let a2 = db.intern_tag(&Tag::parse("samus_alpha").unwrap()).unwrap();
        db.add_sibling(a1, ideal, local).unwrap();
        db.add_sibling(a2, ideal, local).unwrap();
        // Equal raw counts (1 each) -> lexicographically smallest wins.
        let f1 = insert_named(&db, b"tb1", "tb1.png");
        tag_file(&db, f1, local, "samus_zeta");
        let f2 = insert_named(&db, b"tb2", "tb2.png");
        tag_file(&db, f2, local, "samus_alpha");
        let rows = db
            .complete_tags("samus_", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(rows[0].alias_source.as_deref(), Some("samus_alpha"));
    }

    #[test]
    fn complete_tags_direct_match_has_no_alias_source() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db
            .intern_tag(&Tag::parse("character:samus_aran").unwrap())
            .unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f1 = insert_named(&db, b"dm1", "dm1.png");
        tag_file(&db, f1, local, "character:samus");
        let f2 = insert_named(&db, b"dm2", "dm2.png");
        tag_file(&db, f2, local, "character:samus_aran");
        // Fragment matches the canonical directly (and the alias too) -> the
        // canonical row is a step-1 direct emit and must NOT be dressed up.
        let rows = db
            .complete_tags("character:samus", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].alias_source, None);
    }

    #[test]
    fn complete_tags_ideal_name_match_has_no_alias_source() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        // Zero-raw ideal: all usage is the alias; fragment matches the IDEAL name
        // (step-3 injection), so no alias_source even though an alias exists.
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f = insert_named(&db, b"in1", "in1.png");
        tag_file(&db, f, local, "samus_aran");
        let rows = db
            .complete_tags("character:sam", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].alias_source, None);
    }

    #[test]
    fn complete_tags_namespaced_alias_source_round_trip() {
        // Namespaced alias → alias_source must carry the full "namespace:subtag" form.
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db
            .intern_tag(&Tag::parse("character:samus_aran").unwrap())
            .unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f = insert_named(&db, b"ns1", "ns1.png");
        tag_file(&db, f, local, "character:samus_aran");
        // Fragment matches only the alias, not the canonical.
        let rows = db
            .complete_tags("character:samus_a", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(
            rows[0].alias_source.as_deref(),
            Some("character:samus_aran")
        );
    }

    #[test]
    fn complete_tags_overlay_only_alias_sets_alias_source() {
        // Alias with raw 0 (never file-tagged) still supplies alias_source when it
        // surfaces the canonical via the step-2 overlay name scan.
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("metroid_lady").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        // Tag the canonical directly so merged_count(canon) = 1 and step-4
        // injection fires; do NOT tag the alias — it stays at raw 0.
        let f = insert_named(&db, b"oo1", "oo1.png");
        tag_file(&db, f, local, "character:samus");
        // Fragment matches only the alias name — canonical is not in the base scan.
        let rows = db
            .complete_tags("metroid_l", 20, CompletionMode::Prefix)
            .unwrap();
        assert_eq!(suggestion_names(&rows), vec!["character:samus".to_string()]);
        assert_eq!(rows[0].alias_source.as_deref(), Some("metroid_lady"));
    }

    #[test]
    fn complete_tags_double_counts_file_with_canonical_and_alias() {
        // Intended approximation: one file carrying BOTH raws counts twice.
        // Exact per-file dedup would need COUNT(DISTINCT) per candidate per
        // keystroke; the sum reads as "mapping examples backing this concept".
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f = insert_named(&db, b"d1", "d1.png");
        tag_file(&db, f, local, "character:samus");
        tag_file(&db, f, local, "samus_aran");
        let rows = db
            .complete_tags("character:samus", 20, CompletionMode::Prefix)
            .unwrap();
        // raw(ideal)=1 + raw(alias)=1 = 2, though only one file is involved.
        assert_eq!(rows[0].count, 2);
    }

    #[test]
    fn complete_tags_no_relations_uses_plain_scan() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let f1 = insert_named(&db, b"p1", "p1.png");
        tag_file(&db, f1, local, "character:samus");
        let f2 = insert_named(&db, b"p2", "p2.png");
        tag_file(&db, f2, local, "character:sam");
        let rows = db
            .complete_tags("character:sam", 20, CompletionMode::Prefix)
            .unwrap();
        let mut names = suggestion_names(&rows);
        names.sort();
        assert_eq!(
            names,
            vec!["character:sam".to_string(), "character:samus".to_string()]
        );
    }

    #[test]
    fn tag_relations_sections_totals_and_via_alias() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let a1 = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        let a2 = db.intern_tag(&Tag::parse("samus").unwrap()).unwrap();
        db.add_sibling(a1, ideal, local).unwrap();
        db.add_sibling(a2, ideal, local).unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        db.add_parent(ideal, parent, local).unwrap();
        // Give a1 raw counts so ranking is observable.
        let g = insert_named(&db, b"g1", "g1.png");
        tag_file(&db, g, local, "samus_aran");
        // A file carrying the alias raw -> via_alias true.
        let f = insert_named(&db, b"tf", "tf.png");
        tag_file(&db, f, local, "samus_aran");

        let rel = db
            .tag_relations(
                &Tag::parse("character:samus").unwrap(),
                Some(f),
                ReadScope::Merged,
                10,
            )
            .unwrap();
        assert_eq!(rel.canonical.to_string(), "character:samus");
        assert!(rel.via_alias, "file carries an alias raw");
        assert_eq!(rel.aliases.total, 2);
        let alias_tags: Vec<String> = rel
            .aliases
            .items
            .iter()
            .map(|i| i.tag.to_string())
            .collect();
        assert!(alias_tags.contains(&"samus_aran".to_string()));
        assert!(alias_tags.contains(&"samus".to_string()));
        // Alias rows show their OWN raw mapping count, not the canonical's merged
        // total: raw(samus_aran)=2 (two files carry that exact spelling),
        // raw(samus)=0 (never used as a raw spelling; hidden in the UI). Ranked
        // count desc, so samus_aran precedes samus.
        assert_eq!(rel.aliases.items[0].tag.to_string(), "samus_aran");
        assert_eq!(rel.aliases.items[0].count, 2);
        assert_eq!(rel.aliases.items[1].tag.to_string(), "samus");
        assert_eq!(rel.aliases.items[1].count, 0);
        // Top-level count = merged count of character:samus.
        assert_eq!(rel.count, 2);
        assert_eq!(rel.parents.total, 1);
        assert_eq!(rel.parents.items[0].tag.to_string(), "series:metroid");
        assert!(rel.children.items.is_empty());

        // No file -> via_alias false, sections still resolve.
        let no_file = db
            .tag_relations(
                &Tag::parse("character:samus").unwrap(),
                None,
                ReadScope::Merged,
                10,
            )
            .unwrap();
        assert!(!no_file.via_alias);
        assert_eq!(no_file.aliases.total, 2);
        assert_eq!(no_file.count, 2);
    }

    /// Alias rows show their *own* raw mapping count, which is 0 when the
    /// spelling is never used directly (the common case, since files are stored
    /// canonical). The UI hides a 0 count; the informative signal is how many
    /// aliases exist (`aliases.total`), not the per-alias number. The canonical's
    /// own top-level count stays the merged value. This replaced the earlier
    /// behavior where every alias showed the identical canonical merged total.
    #[test]
    fn tag_relations_alias_shows_own_raw_count() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let canonical = db
            .intern_tag(&Tag::parse("creator:tamyra").unwrap())
            .unwrap();
        let alias = db
            .intern_tag(&Tag::parse("artist:tamyra").unwrap())
            .unwrap();
        db.add_sibling(alias, canonical, local).unwrap();
        // Only the canonical is mapped; the alias has 0 raw mappings.
        let f = insert_named(&db, b"af", "af.png");
        tag_file(&db, f, local, "creator:tamyra");

        let rel = db
            .tag_relations(
                &Tag::parse("creator:tamyra").unwrap(),
                None,
                ReadScope::Merged,
                10,
            )
            .unwrap();
        // Top-level count = merged(creator:tamyra) = raw(creator:tamyra=1) = 1.
        assert_eq!(rel.count, 1, "canonical count");
        assert_eq!(rel.aliases.total, 1, "one alternate spelling");
        assert_eq!(rel.aliases.items.len(), 1);
        assert_eq!(rel.aliases.items[0].tag.to_string(), "artist:tamyra");
        // Alias shows its own raw count (0), which the UI hides.
        assert_eq!(
            rel.aliases.items[0].count, 0,
            "alias shows its own raw count, not the merged total"
        );
    }

    /// Regression: a parent tag that itself has an alias must show its own
    /// merged count in the parent row.
    #[test]
    fn tag_relations_parent_with_alias_shows_merged_count() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let child = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let parent = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        let parent_alias = db.intern_tag(&Tag::parse("metroid").unwrap()).unwrap();
        db.add_sibling(parent_alias, parent, local).unwrap();
        db.add_parent(child, parent, local).unwrap();
        // Map files via the parent alias only (parent itself has 0 raw).
        for i in 0u8..3 {
            let pf = insert_named(&db, &[i, 0xaa], &format!("pf{i}.png"));
            tag_file(&db, pf, local, "metroid");
        }

        let rel = db
            .tag_relations(
                &Tag::parse("character:samus").unwrap(),
                None,
                ReadScope::Merged,
                10,
            )
            .unwrap();
        assert_eq!(rel.parents.items.len(), 1);
        assert_eq!(rel.parents.items[0].tag.to_string(), "series:metroid");
        // Parent row shows merged(series:metroid) = raw(0) + raw(metroid alias=3) = 3.
        assert_eq!(
            rel.parents.items[0].count, 3,
            "parent row shows merged count including its alias"
        );
    }

    #[test]
    fn tag_relations_respects_server_cap() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        for i in 0..12 {
            let a = db
                .intern_tag(&Tag::parse(&format!("alias{i}")).unwrap())
                .unwrap();
            db.add_sibling(a, ideal, local).unwrap();
        }
        let rel = db
            .tag_relations(
                &Tag::parse("character:samus").unwrap(),
                None,
                ReadScope::Merged,
                10,
            )
            .unwrap();
        assert_eq!(rel.aliases.total, 12);
        assert_eq!(rel.aliases.items.len(), 10, "capped server-side");
    }

    #[test]
    fn tag_relations_unknown_tag_is_empty_not_error() {
        let db = Db::open_in_memory().unwrap();
        let rel = db
            .tag_relations(
                &Tag::parse("character:nobody").unwrap(),
                None,
                ReadScope::Merged,
                10,
            )
            .unwrap();
        assert_eq!(rel.canonical.to_string(), "character:nobody");
        assert!(!rel.via_alias);
        assert_eq!(rel.aliases.total, 0);
        assert_eq!(rel.parents.total, 0);
        assert_eq!(rel.children.total, 0);
    }

    #[test]
    fn display_tags_detailed_relations_flag() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let ideal = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();
        let alias = db.intern_tag(&Tag::parse("samus_aran").unwrap()).unwrap();
        db.add_sibling(alias, ideal, local).unwrap();
        let f = insert_named(&db, b"df", "df.png");
        tag_file(&db, f, local, "samus_aran"); // effective character:samus (has alias)
        tag_file(&db, f, local, "meta:solo"); // unrelated
        let rows = db.display_tags_detailed(f, ReadScope::Merged).unwrap();
        let samus = rows
            .iter()
            .find(|r| r.tag.to_string() == "character:samus")
            .unwrap();
        assert!(samus.relations, "canonical with an alias shows the glyph");
        let solo = rows
            .iter()
            .find(|r| r.tag.to_string() == "meta:solo")
            .unwrap();
        assert!(!solo.relations, "unrelated tag has no glyph");
    }

    #[test]
    fn sha256_domain_pull_inputs_covers_only_non_null_sha() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let a = hash_bytes(b"file-a");
        let b = hash_bytes(b"file-b");
        db.insert_file(
            &FileRecord::new(a, "/lib/a.png".into(), 6, Some(1)).with_sha256("11".repeat(32)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.insert_file(
            &FileRecord::new(b, "/lib/b.png".into(), 6, Some(1)),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();

        let (keys, map, malformed) = db.sha256_domain_pull_inputs().unwrap();
        assert_eq!(
            keys.len(),
            1,
            "only the file with a sha256 contributes a key"
        );
        assert_eq!(malformed, 0, "no malformed rows");
        assert_eq!(map.get(&"11".repeat(32)).copied(), Some(a));
        assert!(!map.values().any(|h| *h == b), "NULL-sha file excluded");
    }

    // ── Pragma verification ──────────────────────────────────────────────────

    /// Verify that the pragmas added in #135 are actually applied to every
    /// opened Db. Tests the new settings: synchronous=NORMAL (1) and
    /// cache_size=-32768. mmap_size is omitted because SQLite may round it
    /// to a page boundary; journal_mode=WAL requires a real file (covered by
    /// the vacuum_into test which opens with Db::open).
    #[test]
    fn db_init_sets_synchronous_normal_and_cache_size() {
        let db = Db::open_in_memory().unwrap();
        let synchronous: i64 = db
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        // FULL=2, NORMAL=1, OFF=0; we expect NORMAL.
        assert_eq!(
            synchronous, 1,
            "synchronous must be NORMAL (1), got {synchronous}"
        );

        let cache_size: i64 = db
            .conn
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cache_size, -32768,
            "cache_size must be -32768 KiB (32 MiB), got {cache_size}"
        );
    }

    // ── Covering index plan check ────────────────────────────────────────────

    /// Verify that migration 0032 causes the file_ids_with_any_tag query to use
    /// the covering index rather than doing two index lookups plus table reads.
    /// A covering index scan is orders of magnitude faster for large mapping
    /// tables, which is the entire point of this migration.
    #[test]
    fn file_ids_with_any_tag_uses_covering_index() {
        let db = Db::open_in_memory().unwrap();
        // Matches the SQL template in file_ids_with_any_tag; concrete IN values
        // are fine because EXPLAIN QUERY PLAN uses schema only, not data.
        let sql = "EXPLAIN QUERY PLAN
                   SELECT m.file_id, m.tag_id, m.service_id
                     FROM mappings m
                    WHERE m.service_id IN (1) AND m.status = 'current'
                      AND m.tag_id IN (1)";
        let plan: Vec<String> = db
            .conn
            .prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("idx_mappings_tag_svc_status_file"),
            "file_ids_with_any_tag must use the covering index, got plan:\n{plan}"
        );
    }

    #[test]
    fn domain_pull_state_is_independent_per_domain() {
        let db = Db::open_in_memory().unwrap();
        let svc = db
            .add_shared_service("dual", "http://repo.test:9090", None)
            .unwrap();

        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), None);
        assert_eq!(db.mapping_cursor(svc, "sha256").unwrap(), None);

        db.set_mapping_pull_state(svc, "blake3", 12, 99).unwrap();
        db.set_mapping_pull_state(svc, "sha256", 7, 42).unwrap();

        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), Some(12));
        assert_eq!(db.last_pull_file_marker(svc, "blake3").unwrap(), Some(99));
        assert_eq!(db.mapping_cursor(svc, "sha256").unwrap(), Some(7));
        assert_eq!(db.last_pull_file_marker(svc, "sha256").unwrap(), Some(42));

        // Re-setting one domain is an upsert, not a duplicate row.
        db.set_mapping_pull_state(svc, "blake3", 13, 100).unwrap();
        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), Some(13));

        // Clearing one domain must not touch the other.
        db.clear_mapping_pull_state(svc, "blake3").unwrap();
        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), None);
        assert_eq!(
            db.mapping_cursor(svc, "sha256").unwrap(),
            Some(7),
            "the sha256 cursor must survive a blake3 reset"
        );
    }

    #[test]
    fn migration_33_carries_legacy_cursor_into_blake3_row() {
        // Stop one migration short, write the legacy columns the way a
        // pre-dual-domain client did, then migrate the rest of the way.
        let mut conn = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut conn, 32).unwrap();
        conn.execute(
            "INSERT INTO services (name, scope, url) VALUES ('legacy', 'shared', 'http://r:1')",
            [],
        )
        .unwrap();
        let svc: i64 = conn
            .query_row("SELECT id FROM services", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "UPDATE services SET mapping_cursor = 55, last_pull_file_marker = 7 WHERE id = ?1",
            params![svc],
        )
        .unwrap();

        // Stop AT 33: this test is about 0033's carry-forward. Migration 0034
        // deliberately rewinds the state it carries forward, to force one full
        // re-pull that re-establishes per-domain provenance — asserted
        // separately by `migration_0034_backfills_native_and_forces_a_full_repull`.
        MIGRATIONS.to_version(&mut conn, 33).unwrap();

        let (cursor, marker): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT mapping_cursor, last_pull_file_marker
                 FROM service_domain_pull_state WHERE service_id = ?1 AND domain = 'blake3'",
                params![svc],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cursor, Some(55), "legacy cursor carried forward");
        assert_eq!(marker, Some(7), "legacy marker carried forward");

        let sha_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM service_domain_pull_state WHERE domain = 'sha256'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sha_rows, 0, "no sha256 state is invented by the migration");

        // A second service whose legacy columns are both NULL must not produce
        // any row — the WHERE filter must hold.
        conn.execute(
            "INSERT INTO services (name, scope, url) VALUES ('null-state', 'shared', 'http://r:2')",
            [],
        )
        .unwrap();
        let total_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM service_domain_pull_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            total_rows, 1,
            "only the service with non-null legacy columns should have a row"
        );
    }

    #[test]
    fn drop_service_removes_domain_pull_state() {
        let db = Db::open_in_memory().unwrap();
        let svc = db
            .add_shared_service("gone", "http://repo.test:9090", None)
            .unwrap();
        db.set_mapping_pull_state(svc, "blake3", 3, 4).unwrap();
        db.set_mapping_pull_state(svc, "sha256", 5, 6).unwrap();

        db.drop_service(svc).unwrap();

        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), None);
        assert_eq!(db.mapping_cursor(svc, "sha256").unwrap(), None);
    }

    /// `count_files_missing_sha256` counts ALL files with sha256 IS NULL,
    /// regardless of whether they have a present location (#157).
    /// `count_files_missing_sha256_present` is the present-only form for the
    /// backfill-needed decision.
    #[test]
    fn count_files_missing_sha256_includes_offline_files() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.count_files_missing_sha256().unwrap(), 0, "empty library");
        assert_eq!(
            db.count_files_missing_sha256_present().unwrap(),
            0,
            "empty library (present form)"
        );

        let a = naiad_core::hash_bytes(b"a");
        let b = naiad_core::hash_bytes(b"b");
        let c = naiad_core::hash_bytes(b"c");

        // `a` has no sha256 (the #141 shape), `b` has one.
        db.insert_file(
            &naiad_core::FileRecord::new(a, "/lib/a.png".into(), 1, Some(1)),
            1,
        )
        .unwrap();
        db.insert_file(
            &naiad_core::FileRecord::new(b, "/lib/b.png".into(), 1, Some(1))
                .with_sha256("ab".repeat(32)),
            1,
        )
        .unwrap();

        assert_eq!(
            db.count_files_missing_sha256().unwrap(),
            1,
            "only the NULL-sha256 row counts"
        );
        assert_eq!(
            db.count_files_missing_sha256_present().unwrap(),
            1,
            "present form agrees when all missing files are present on disk"
        );

        // (a) GROUP BY dedup: a second location for hash `a` must NOT inflate
        // the count — one file, two paths, still counts as one missing file.
        db.insert_file(
            &naiad_core::FileRecord::new(a, "/lib/a2.png".into(), 1, Some(1)),
            2,
        )
        .unwrap();
        assert_eq!(
            db.count_files_missing_sha256().unwrap(),
            1,
            "two locations for the same file count once (total)"
        );
        assert_eq!(
            db.count_files_missing_sha256_present().unwrap(),
            1,
            "two locations for the same file count once (present)"
        );

        // (b) #157: add a third NULL-sha256 file, then mark it missing. The
        // TOTAL count must STAY AT 2 (offline file is still genuinely missing
        // its sha256), while the PRESENT count drops to 1 (it's no longer
        // backfillable).
        db.insert_file(
            &naiad_core::FileRecord::new(c, "/lib/c.png".into(), 1, Some(1)),
            2,
        )
        .unwrap();
        assert_eq!(
            db.count_files_missing_sha256().unwrap(),
            2,
            "two NULL-sha256 files before mark_missing"
        );
        assert_eq!(
            db.count_files_missing_sha256_present().unwrap(),
            2,
            "two present NULL-sha256 files before mark_missing"
        );
        db.mark_missing_path(std::path::Path::new("/lib/c.png"))
            .unwrap();
        // total stays at 2: offline file is still missing sha256
        assert_eq!(
            db.count_files_missing_sha256().unwrap(),
            2,
            "#157: offline NULL-sha256 file still counted by the total function"
        );
        // present drops to 1: the offline file can't be backfilled
        assert_eq!(
            db.count_files_missing_sha256_present().unwrap(),
            1,
            "present=1 filter: offline file drops out of the backfillable count"
        );

        // The present form must still agree with the list it stands in for.
        assert_eq!(
            db.count_files_missing_sha256_present().unwrap() as usize,
            db.files_missing_sha256().unwrap().len(),
            "count_files_missing_sha256_present must agree with files_missing_sha256"
        );
    }

    /// `files_missing_sha256_after` returns at most `limit` rows, only present
    /// files, and strictly advances past `after_id` so a page of unhashable
    /// rows cannot starve the files behind it (#152).
    #[test]
    fn files_missing_sha256_after_pages_forward_and_excludes_offline() {
        let db = Db::open_in_memory().unwrap();
        let hashes: Vec<naiad_core::Hash> =
            (0..5u8).map(|i| naiad_core::hash_bytes(&[i])).collect();
        // Insert 5 present NULL-sha256 files + 1 offline NULL-sha256 file.
        for (i, h) in hashes.iter().enumerate() {
            db.insert_file(
                &naiad_core::FileRecord::new(*h, format!("/lib/{i}.png").into(), 1, Some(1)),
                1,
            )
            .unwrap();
        }
        let offline_h = naiad_core::hash_bytes(b"offline");
        db.insert_file(
            &naiad_core::FileRecord::new(offline_h, "/lib/offline.png".into(), 1, Some(1)),
            1,
        )
        .unwrap();
        db.mark_missing_path(std::path::Path::new("/lib/offline.png"))
            .unwrap();

        // A page from the start with limit 3 returns exactly 3 rows (not 5 or 6).
        let rows = db.files_missing_sha256_after(0, 3).unwrap();
        assert_eq!(rows.len(), 3, "LIMIT 3 returns exactly 3 rows");
        assert!(
            rows.windows(2).all(|w| w[0].0 < w[1].0),
            "rows must ascend by id so the caller's cursor advances"
        );

        // Paging past the first page's high-water mark yields the rest — and
        // never re-serves the earlier rows, which is what stops an unhashable
        // page from being handed back forever.
        let next = db.files_missing_sha256_after(rows[2].0, 10).unwrap();
        assert_eq!(
            next.len(),
            2,
            "two present rows remain after the first page"
        );
        assert!(
            next.iter().all(|(id, _)| *id > rows[2].0),
            "a page must strictly advance past after_id"
        );

        // A page wide enough for everything returns all 5 present rows, not 6.
        let all = db.files_missing_sha256_after(0, 10).unwrap();
        assert_eq!(
            all.len(),
            5,
            "limit > present count returns all 5 present rows, not the offline one"
        );
        let ids: Vec<i64> = all.iter().map(|(id, _)| *id).collect();
        let offline_id = db.file_id_by_hash(&offline_h).unwrap().unwrap();
        assert!(
            !ids.contains(&offline_id),
            "offline file must not appear in the work list"
        );
    }

    /// A DB with one malformed sha256 row must still return the well-formed
    /// rows and report the malformed count rather than hard-failing (#158).
    #[test]
    fn sha256_domain_pull_inputs_skips_malformed_sha256() {
        let db = Db::open_in_memory().unwrap();
        let good_blake = naiad_core::hash_bytes(b"good");
        let bad_blake = naiad_core::hash_bytes(b"bad");

        // Insert a good file (valid sha256) and a bad file (malformed sha256).
        db.insert_file(
            &naiad_core::FileRecord::new(good_blake, "/lib/good.png".into(), 1, Some(1))
                .with_sha256("aa".repeat(32)),
            1,
        )
        .unwrap();
        db.insert_file(
            &naiad_core::FileRecord::new(bad_blake, "/lib/bad.png".into(), 1, Some(1))
                .with_sha256("not-a-valid-hex-hash".into()),
            1,
        )
        .unwrap();

        let (keys, map, malformed) = db.sha256_domain_pull_inputs().unwrap();
        assert_eq!(malformed, 1, "one malformed row counted");
        assert_eq!(keys.len(), 1, "only the well-formed row contributes a key");
        assert!(
            map.contains_key(&"aa".repeat(32)),
            "well-formed sha256 must be in the map"
        );
    }

    /// `detach_service` must clear `service_domain_pull_state` (#160 item 8b).
    #[test]
    fn detach_service_keeps_domain_pull_state_for_reattach() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://r:1", None).unwrap();
        db.set_mapping_pull_state(svc, "blake3", 42, 10).unwrap();
        db.set_mapping_pull_state(svc, "sha256", 7, 5).unwrap();

        db.detach_service(svc).unwrap();

        // A detach keeps the tags, so it must keep the cursors too: re-attaching
        // the same repo should not force a full re-pull, and the unsubscribe
        // handler's failed-write rollback re-attaches through `set_service_url`.
        assert_eq!(db.mapping_cursor(svc, "blake3").unwrap(), Some(42));
        assert_eq!(db.mapping_cursor(svc, "sha256").unwrap(), Some(7));

        // Re-attaching at the same URL is not a re-point, so state survives.
        db.set_service_url(svc, "http://r:1").unwrap();
        assert_eq!(
            db.mapping_cursor(svc, "blake3").unwrap(),
            Some(42),
            "detach + re-attach at the same URL must not lose the cursor"
        );
    }

    /// `set_service_url` clears `service_domain_pull_state` only when it
    /// replaces a *different* existing URL (#160 item 8b).
    #[test]
    fn set_service_url_clears_domain_pull_state_only_on_a_real_repoint() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://old:1", None).unwrap();
        db.set_mapping_pull_state(svc, "blake3", 99, 20).unwrap();

        // Same URL: nothing about which repo this name refers to has changed.
        db.set_service_url(svc, "http://old:1").unwrap();
        assert_eq!(
            db.mapping_cursor(svc, "blake3").unwrap(),
            Some(99),
            "re-setting the same URL must not discard the cursor"
        );

        // Different URL: the new server's seq sequence is unrelated, so a
        // carried-over cursor would silently skip its early history.
        db.set_service_url(svc, "http://new:2").unwrap();
        assert_eq!(
            db.mapping_cursor(svc, "blake3").unwrap(),
            None,
            "re-pointing to a new URL must clear the old cursor"
        );
    }

    // ── migration 0035 ────────────────────────────────────────────────────────

    #[test]
    fn migration_0035_schema_and_pull_state() {
        // ── schema assertions (fully-migrated DB) ───────────────────────────
        let db = test_db(); // same helper sibling tests use to open a migrated Db
        let conn = db.raw_conn_for_test();
        let has_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('files') WHERE name = 'sha256_seq'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_col, 1, "files.sha256_seq column must exist");
        let counter_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sha256_seq_counter WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            counter_rows, 1,
            "sha256_seq_counter must have its single row"
        );
        // On an empty DB the counter starts at 0 (locks the COALESCE empty-table branch).
        let counter_zero: i64 = conn
            .query_row(
                "SELECT value FROM sha256_seq_counter WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            counter_zero, 0,
            "sha256_seq_counter.value must be 0 on a fresh empty DB"
        );
        // idx_files_sha256 was created by migration 0011 (full index, no WHERE
        // clause); migration 0035 keeps it and does not replace it with a
        // partial variant — the full index also serves NULL-count queries.
        let has_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_files_sha256'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            has_index, 1,
            "idx_files_sha256 must exist (created by 0011, kept by 0035)"
        );

        // ── data assertions (to_version pattern: seed at 34, migrate to 35) ─
        // This mirrors the house pattern established by migration_0034 tests.
        let mut raw = Connection::open_in_memory().unwrap();
        MIGRATIONS.to_version(&mut raw, 34).unwrap();

        // Seed one shared service (id=9, matching the 0034 test's convention;
        // service id=1 is the built-in local service inserted by migration 0001).
        raw.execute(
            "INSERT INTO services (id, name, scope, url, priority)
             VALUES (9, 'ptr', 'shared', 'http://x', 0)",
            [],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO service_domain_pull_state
                 (service_id, domain, mapping_cursor, last_pull_file_marker)
             VALUES (9, 'sha256', 77, 99), (9, 'blake3', 55, 88)",
            [],
        )
        .unwrap();

        // Three files: id=10 has uppercase sha256 (should be lowercased),
        // id=11 has lowercase sha256 (should gain seq=2), id=12 has NULL sha256.
        raw.execute(
            "INSERT INTO files (id, blake3, size, sha256, state, imported_at)
             VALUES
               (10, '1100000000000000000000000000000000000000000000000000000000000000',
                    1, 'AA00000000000000000000000000000000000000000000000000000000000000',
                    'active', 0),
               (11, '2200000000000000000000000000000000000000000000000000000000000000',
                    1, 'bb00000000000000000000000000000000000000000000000000000000000000',
                    'active', 0),
               (12, '3300000000000000000000000000000000000000000000000000000000000000',
                    1, NULL,
                    'active', 0)",
            [],
        )
        .unwrap();

        MIGRATIONS.to_version(&mut raw, 35).unwrap();

        // (a) sha256 lowercase normalisation.
        let sha: String = raw
            .query_row("SELECT sha256 FROM files WHERE id = 10", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            sha, "aa00000000000000000000000000000000000000000000000000000000000000",
            "uppercase sha256 must be lowercased by migration 0035"
        );

        // (b) sha256 pull state zeroed; blake3 rows untouched.
        let (sha_cursor, sha_marker): (i64, Option<i64>) = raw
            .query_row(
                "SELECT mapping_cursor, last_pull_file_marker
                 FROM service_domain_pull_state
                 WHERE service_id = 9 AND domain = 'sha256'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(sha_cursor, 0, "sha256 mapping_cursor must be zeroed");
        assert_eq!(
            sha_marker, None,
            "sha256 last_pull_file_marker must be cleared"
        );

        let (b3_cursor, b3_marker): (i64, Option<i64>) = raw
            .query_row(
                "SELECT mapping_cursor, last_pull_file_marker
                 FROM service_domain_pull_state
                 WHERE service_id = 9 AND domain = 'blake3'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(b3_cursor, 55, "blake3 cursor must be untouched by 0035");
        assert_eq!(
            b3_marker,
            Some(88),
            "blake3 marker must be untouched by 0035"
        );

        // (c) Retroactive sha256_seq stamping: dense, ORDER BY id. id=10 gets
        // seq=1, id=11 gets seq=2 (stable ascending order). id=12 (NULL sha256)
        // must not be stamped. Counter must equal the max assigned seq.
        let seq1: i64 = raw
            .query_row("SELECT sha256_seq FROM files WHERE id = 10", [], |r| {
                r.get(0)
            })
            .unwrap();
        let seq2: i64 = raw
            .query_row("SELECT sha256_seq FROM files WHERE id = 11", [], |r| {
                r.get(0)
            })
            .unwrap();
        let seq3: Option<i64> = raw
            .query_row("SELECT sha256_seq FROM files WHERE id = 12", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(seq1, 1, "lowest-id sha256-bearing row gets seq=1");
        assert_eq!(seq2, 2, "next sha256-bearing row by id gets seq=2");
        assert_eq!(seq3, None, "NULL-sha256 row must not receive a seq");
        let counter: i64 = raw
            .query_row(
                "SELECT value FROM sha256_seq_counter WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(counter, 2, "counter must equal the max assigned sha256_seq");
    }

    // ── sha256_seq stamping tests ─────────────────────────────────────────────

    #[test]
    fn sha256_seq_stamped_once_on_gain_no_churn() {
        let db = test_db();
        let m = db.next_scan_marker().unwrap();
        // Import a file whose sha256 is already known (insert_file path).
        let rec = rec_with_hash(h(1), "a").with_sha256(SHA_A.to_string());
        db.insert_file(&rec, m).unwrap();
        let conn = db.raw_conn_for_test();
        let seq1: i64 = conn
            .query_row(
                "SELECT sha256_seq FROM files WHERE blake3 = ?1",
                [h(1).to_hex()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seq1, 1, "first sha256 gain stamps seq 1");
        // Re-import identical bytes+sha256 — must NOT churn the counter.
        db.insert_file(&rec, db.next_scan_marker().unwrap())
            .unwrap();
        let seq2: i64 = conn
            .query_row(
                "SELECT sha256_seq FROM files WHERE blake3 = ?1",
                [h(1).to_hex()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seq2, 1, "idempotent re-import must not re-stamp");
        assert_eq!(
            db.max_sha256_seq().unwrap(),
            1,
            "counter must not advance on re-confirm"
        );
    }

    #[test]
    fn set_sha256_stamps_null_to_present_only() {
        let db = test_db();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec_with_hash(h(1), "a"), m).unwrap(); // no sha256
        let fid: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT id FROM files WHERE blake3 = ?1",
                [h(1).to_hex()],
                |r| r.get(0),
            )
            .unwrap();
        db.set_sha256(fid, SHA_A).unwrap();
        assert_eq!(db.max_sha256_seq().unwrap(), 1);
        db.set_sha256(fid, SHA_A).unwrap(); // same value again
        assert_eq!(
            db.max_sha256_seq().unwrap(),
            1,
            "no churn on unchanged rewrite"
        );
    }

    #[test]
    fn set_sha256_batch_reserves_contiguous_range_in_id_order() {
        let db = test_db();
        for i in 1..=3 {
            db.insert_file(&rec_with_hash(h(i), "a"), db.next_scan_marker().unwrap())
                .unwrap();
        }
        let ids: Vec<i64> = (1..=3)
            .map(|i| {
                db.raw_conn_for_test()
                    .query_row(
                        "SELECT id FROM files WHERE blake3 = ?1",
                        [h(i).to_hex()],
                        |r| r.get(0),
                    )
                    .unwrap()
            })
            .collect();
        // Batch stamps three rows; pass them out of id order to prove stable ordering.
        let items = vec![
            (ids[2], SHA_C.to_string()),
            (ids[0], SHA_A.to_string()),
            (ids[1], SHA_B.to_string()),
        ];
        db.set_sha256_batch(&items).unwrap();
        let seq = |fid: i64| -> i64 {
            db.raw_conn_for_test()
                .query_row("SELECT sha256_seq FROM files WHERE id = ?1", [fid], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        // Stable ORDER BY id: lowest id gets the lowest reserved seq.
        assert_eq!(seq(ids[0]), 1);
        assert_eq!(seq(ids[1]), 2);
        assert_eq!(seq(ids[2]), 3);
        assert_eq!(db.max_sha256_seq().unwrap(), 3);
    }

    #[test]
    fn max_sha256_seq_survives_row_deletion() {
        let db = test_db();
        let m = db.next_scan_marker().unwrap();
        db.insert_file(&rec_with_hash(h(1), "a").with_sha256(SHA_A.to_string()), m)
            .unwrap();
        assert_eq!(db.max_sha256_seq().unwrap(), 1);
        // Remove locations first so the FK to files(id) does not block deletion.
        db.raw_conn_for_test()
            .execute(
                "DELETE FROM file_locations WHERE file_id = (SELECT id FROM files WHERE blake3 = ?1)",
                [h(1).to_hex()],
            )
            .unwrap();
        db.raw_conn_for_test()
            .execute("DELETE FROM files WHERE blake3 = ?1", [h(1).to_hex()])
            .unwrap();
        // Counter never looks back: a new gain must not reissue seq 1.
        db.insert_file(
            &rec_with_hash(h(2), "b").with_sha256(SHA_B.to_string()),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let seq2: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT sha256_seq FROM files WHERE blake3 = ?1",
                [h(2).to_hex()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seq2, 2, "counter must not reissue a deleted row's seq");
        assert_eq!(db.max_sha256_seq().unwrap(), 2);
    }

    // ── owned_sha256_bucket_keys_after_seq ───────────────────────────────────

    #[test]
    fn owned_sha256_bucket_keys_after_seq_respects_marker() {
        let db = test_db();
        // Two files gain sha256 → seq 1 and 2.
        db.insert_file(
            &rec_with_hash(h(1), "a").with_sha256(SHA_A.to_string()),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        db.insert_file(
            &rec_with_hash(h(2), "b").with_sha256(SHA_B.to_string()),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        // marker = 1 ⇒ only the seq-2 file's bucket is "new".
        let keys = db.owned_sha256_bucket_keys_after_seq(16, 1).unwrap();
        let expect = bucket_key(&SHA_B.parse::<Hash>().unwrap(), 16);
        assert_eq!(keys, vec![expect], "only sha256_seq > marker is new");
        // marker = 0 ⇒ both.
        assert_eq!(
            db.owned_sha256_bucket_keys_after_seq(16, 0).unwrap().len(),
            2
        );
    }

    // ── resolve_staged_mappings domain bit ───────────────────────────────────

    #[test]
    fn resolve_staged_mappings_stamps_sha256_bit() {
        let db = test_db();
        // A file whose sha256 is known, and a staged current mapping keyed by sha256.
        db.insert_file(
            &rec_with_hash(h(1), "a").with_sha256(SHA_A.to_string()),
            db.next_scan_marker().unwrap(),
        )
        .unwrap();
        let svc = db.add_local_service("test-svc", None).unwrap();
        let tag_id = db.intern_tag(&Tag::parse("foo").unwrap()).unwrap();
        db.stage_mapping(SHA_A, tag_id, svc, "current").unwrap();
        let applied = db.resolve_staged_mappings(svc, "sha256").unwrap();
        assert_eq!(applied, 1);
        let bit: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT domains FROM mappings WHERE service_id = ?1",
                [svc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bit,
            domain_bit("sha256"),
            "resolved row must carry the sha256 provenance bit"
        );
    }

    #[test]
    fn migration_0036_is_additive_and_unindexed() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.raw_conn_for_test();
        // origins table exists and starts empty.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM origins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // services.origin column exists.
        let svc_has_origin: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('services') WHERE name = 'origin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(svc_has_origin, 1, "services must have an origin column");
        // mappings.origin_id column exists.
        let map_has_origin_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('mappings') WHERE name = 'origin_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            map_has_origin_id, 1,
            "mappings must have an origin_id column"
        );
        // No index mentions origin_id — the #151 perf invariant.
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND sql LIKE '%origin_id%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 0, "origin_id must not be indexed");
        // staged_mappings deliberately has NO origin_id column.
        let staged_has_origin: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('staged_mappings') WHERE name = 'origin_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            staged_has_origin, 0,
            "staged_mappings must not gain origin_id"
        );
        assert!(MIGRATIONS.validate().is_ok());
    }

    #[test]
    fn intern_origin_dedupes_and_caches() {
        let db = Db::open_in_memory().unwrap();
        let a = db.intern_origin("wd14-tagger").unwrap();
        let b = db.intern_origin("wd14-tagger").unwrap();
        assert_eq!(a, b, "repeated intern must return the same id");
        let mut cache = std::collections::HashMap::new();
        let c = db.intern_origin_cached("wd14-tagger", &mut cache).unwrap();
        assert_eq!(a, c, "cached intern must match direct intern");
        // Verify only one row was inserted.
        let count: i64 = db
            .raw_conn_for_test()
            .query_row("SELECT COUNT(*) FROM origins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "exactly one origins row for the same name");
    }

    #[test]
    fn origin_of_local_mapping_reads_service_origin_and_none_for_my_tags() {
        let db = Db::open_in_memory().unwrap();
        // The seeded "my tags" service has NULL origin and priority 1000.
        let my_tags = db.local_service_id().unwrap();
        // Add a second local service with origin "hydrus" at priority 10.
        let hydrus_svc = db
            .add_local_service("Hydrus: imported tags", Some("hydrus"))
            .unwrap();
        db.raw_conn_for_test()
            .execute(
                "UPDATE services SET priority = 10 WHERE id = ?1",
                [hydrus_svc],
            )
            .unwrap();

        let tag_id = db
            .intern_tag(&Tag::parse("character:samus").unwrap())
            .unwrap();

        // File 1: mapping supplied by hydrus service only.
        let f1 = insert_named(&db, b"file1", "file1.png");
        tag_file(&db, f1, hydrus_svc, "character:samus");
        let origin = db.origin_of_local_mapping(f1, tag_id).unwrap();
        assert_eq!(
            origin,
            Some("hydrus".to_string()),
            "hydrus service origin must be returned"
        );

        // File 2: same tag supplied only by my-tags (NULL origin).
        let f2 = insert_named(&db, b"file2", "file2.png");
        tag_file(&db, f2, my_tags, "character:samus");
        let origin2 = db.origin_of_local_mapping(f2, tag_id).unwrap();
        assert_eq!(origin2, None, "my-tags (NULL origin) must return None");

        // File 3: no local mapping at all.
        let f3 = insert_named(&db, b"file3", "file3.png");
        let origin3 = db.origin_of_local_mapping(f3, tag_id).unwrap();
        assert_eq!(origin3, None, "absent mapping must return None");

        // File 4: contested — BOTH my-tags (priority 1000, NULL origin) and
        // hydrus (priority 10, origin "hydrus") supply the same (file, tag).
        // my-tags wins `priority DESC, id ASC` → result must be None.
        let f4 = insert_named(&db, b"file4", "file4.png");
        tag_file(&db, f4, my_tags, "character:samus");
        tag_file(&db, f4, hydrus_svc, "character:samus");
        let origin4 = db.origin_of_local_mapping(f4, tag_id).unwrap();
        assert_eq!(
            origin4, None,
            "my-tags (priority 1000) wins the tiebreak, so origin must be None"
        );

        // Flip priorities so hydrus wins (priority 2000 > 1000).
        db.raw_conn_for_test()
            .execute(
                "UPDATE services SET priority = 2000 WHERE id = ?1",
                [hydrus_svc],
            )
            .unwrap();
        let origin4_flipped = db.origin_of_local_mapping(f4, tag_id).unwrap();
        assert_eq!(
            origin4_flipped,
            Some("hydrus".to_string()),
            "hydrus (priority 2000) must win after the flip"
        );
    }

    // ── pulled-origin threading (#162, Task 14) ──────────────────────────────

    /// A row supplied by two domains keeps the FIRST domain's origin_id.
    ///
    /// This validates the #151 perf rule: origin_id is INSERT-only, so the
    /// second domain's `DO UPDATE SET domains = domains | ?5` does NOT overwrite
    /// it. A row DELETEd and re-inserted by the next pull of the same domain
    /// gets a fresh origin (the DELETE-then-insert path), which is acceptable.
    #[test]
    fn shared_domain_row_keeps_first_writer_origin() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr", "http://repo", None).unwrap();
        let h: Hash = hash_bytes(b"origin-guard-file");
        let marker = db.next_scan_marker().unwrap();
        db.insert_file(
            &FileRecord::new(h, "/lib/g.png".into(), 1, Some(1)).with_sha256(
                // We need a fake sha256 hex; any 64-char hex will do.
                format!("{:0<64}", "ab"),
            ),
            marker,
        )
        .unwrap();

        let tag = Tag::parse("character:samus").unwrap();

        // First domain (blake3) asserts the tag with origin "wd14-tagger".
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(h, vec![(tag.clone(), Some("wd14-tagger".to_string()))])],
        )
        .unwrap();

        // Second domain (sha256) asserts the same tag with a different origin.
        db.merge_pulled_mappings_in_domain(
            svc,
            "sha256",
            &[(h, vec![(tag.clone(), Some("gelbooru".to_string()))])],
        )
        .unwrap();

        // Both domains supply the row — it has both bits set.
        let fid = db.file_id_by_hash(&h).unwrap().unwrap();
        let tag_id = db.intern_tag(&tag).unwrap();

        let (origin_name, domains): (Option<String>, i64) = db
            .raw_conn_for_test()
            .query_row(
                "SELECT o.name, m.domains
                   FROM mappings m
                   LEFT JOIN origins o ON o.id = m.origin_id
                  WHERE m.file_id = ?1 AND m.tag_id = ?2 AND m.service_id = ?3",
                params![fid, tag_id, svc],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert_eq!(
            origin_name,
            Some("wd14-tagger".to_string()),
            "first domain's origin must be preserved (INSERT-only rule)"
        );
        assert_eq!(
            domains,
            DOMAIN_BIT_BLAKE3 | DOMAIN_BIT_SHA256,
            "row must carry both domain bits"
        );

        // origins table has exactly one row per distinct name.
        let origin_count: i64 = db
            .raw_conn_for_test()
            .query_row("SELECT COUNT(*) FROM origins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(origin_count, 2, "wd14-tagger + gelbooru = 2 origin rows");

        // A manual tag (origin None) leaves origin_id NULL.
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(h, vec![(Tag::parse("series:metroid").unwrap(), None)])],
        )
        .unwrap();
        let manual_tag_id = db
            .intern_tag(&Tag::parse("series:metroid").unwrap())
            .unwrap();
        let manual_origin: Option<i64> = db
            .raw_conn_for_test()
            .query_row(
                "SELECT origin_id FROM mappings
                  WHERE file_id = ?1 AND tag_id = ?2 AND service_id = ?3",
                params![fid, manual_tag_id, svc],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            manual_origin.is_none(),
            "manual tag must have NULL origin_id"
        );
    }

    /// A full-merge (snapshot path) populates origin_id for named origins;
    /// manual tags land with NULL. Repeated origins intern as a single row.
    #[test]
    fn full_merge_snapshot_populates_origin_id() {
        use naiad_core::{FileRecord, hash_bytes};
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("ptr2", "http://repo2", None).unwrap();
        let h1: Hash = hash_bytes(b"snap-origin-file-1");
        let h2: Hash = hash_bytes(b"snap-origin-file-2");
        let marker = db.next_scan_marker().unwrap();
        db.insert_file(
            &FileRecord::new(h1, "/lib/s1.png".into(), 1, Some(1)),
            marker,
        )
        .unwrap();
        db.insert_file(
            &FileRecord::new(h2, "/lib/s2.png".into(), 1, Some(1)),
            marker,
        )
        .unwrap();

        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[
                (
                    h1,
                    vec![
                        (
                            Tag::parse("t:tagged").unwrap(),
                            Some("wd14-tagger".to_string()),
                        ),
                        (
                            Tag::parse("t:tagged").unwrap(),
                            Some("wd14-tagger".to_string()),
                        ),
                    ],
                ),
                (h2, vec![(Tag::parse("t:manual").unwrap(), None)]),
            ],
        )
        .unwrap();

        // Repeated name "wd14-tagger" must intern to exactly one origins row.
        let origin_count: i64 = db
            .raw_conn_for_test()
            .query_row("SELECT COUNT(*) FROM origins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(origin_count, 1, "repeated origin must intern once");

        // h2's manual tag has NULL origin_id.
        let fid2 = db.file_id_by_hash(&h2).unwrap().unwrap();
        let tag_id2 = db.intern_tag(&Tag::parse("t:manual").unwrap()).unwrap();
        let null_origin: Option<i64> = db
            .raw_conn_for_test()
            .query_row(
                "SELECT origin_id FROM mappings WHERE file_id = ?1 AND tag_id = ?2",
                params![fid2, tag_id2],
                |r| r.get(0),
            )
            .unwrap();
        assert!(null_origin.is_none(), "manual tag must have NULL origin_id");
    }

    /// `add_shared_service` with `origin = Some("hydrus")` persists the origin
    /// name to `services.origin`; absent (None) leaves the column NULL.
    /// Verifies the Task 16 acceptance criterion: service-create round-trip.
    #[test]
    fn add_shared_service_persists_origin() {
        let db = Db::open_in_memory().unwrap();

        let _id = db
            .add_shared_service("src-a", "http://repo-a/", Some("hydrus"))
            .unwrap();
        let stored: Option<String> = db
            .raw_conn_for_test()
            .query_row(
                "SELECT origin FROM services WHERE name = 'src-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("hydrus"),
            "services.origin must store the supplied origin name"
        );

        let _id2 = db
            .add_shared_service("src-b", "http://repo-b/", None)
            .unwrap();
        let null_stored: Option<String> = db
            .raw_conn_for_test()
            .query_row(
                "SELECT origin FROM services WHERE name = 'src-b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            null_stored.is_none(),
            "services.origin must be NULL when no origin supplied"
        );
    }

    // ── store-generation tracking (#194) ─────────────────────────────────────

    #[test]
    fn store_generation_absent_on_fresh_service() {
        let db = test_db();
        let svc = db
            .add_shared_service("gen-repo", "http://gen-repo/", None)
            .unwrap();
        assert!(
            db.service_store_generation(svc).unwrap().is_none(),
            "store_generation must be NULL for a freshly-added service"
        );
    }

    #[test]
    fn set_and_get_store_generation() {
        let db = test_db();
        let svc = db
            .add_shared_service("gen-repo2", "http://gen-repo2/", None)
            .unwrap();
        db.set_service_store_generation(svc, "deadbeef1234567890abcdef01234567")
            .unwrap();
        let got = db.service_store_generation(svc).unwrap();
        assert_eq!(
            got.as_deref(),
            Some("deadbeef1234567890abcdef01234567"),
            "set_service_store_generation must persist and be readable back"
        );
    }

    #[test]
    fn set_store_generation_updates_existing_value() {
        let db = test_db();
        let svc = db
            .add_shared_service("gen-repo3", "http://gen-repo3/", None)
            .unwrap();
        db.set_service_store_generation(svc, "gen-v1-aaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap();
        db.set_service_store_generation(svc, "gen-v2-bbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap();
        let got = db.service_store_generation(svc).unwrap();
        assert_eq!(
            got.as_deref(),
            Some("gen-v2-bbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            "second set_service_store_generation must overwrite the first"
        );
    }

    #[test]
    fn reset_service_cursors_clears_relation_cursor_and_domain_state() {
        let db = test_db();
        let svc = db
            .add_shared_service("cursor-repo", "http://cursor-repo/", None)
            .unwrap();

        // Write a relation_cursor.
        db.raw_conn_for_test()
            .execute(
                "UPDATE services SET relation_cursor = 42 WHERE id = ?1",
                rusqlite::params![svc],
            )
            .unwrap();

        // Write a service_domain_pull_state row.
        db.raw_conn_for_test()
            .execute(
                "INSERT INTO service_domain_pull_state(service_id, domain, mapping_cursor)
                 VALUES(?1, 'blake3', 9000)",
                rusqlite::params![svc],
            )
            .unwrap();

        // Verify both are set.
        assert_eq!(db.relation_cursor(svc).unwrap(), Some(42));
        let domain_count: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM service_domain_pull_state WHERE service_id = ?1",
                rusqlite::params![svc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(domain_count, 1);

        // Reset.
        db.reset_service_cursors(svc).unwrap();

        // Both must be cleared.
        assert_eq!(
            db.relation_cursor(svc).unwrap(),
            None,
            "reset_service_cursors must NULL the relation_cursor"
        );
        let domain_count_after: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM service_domain_pull_state WHERE service_id = ?1",
                rusqlite::params![svc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            domain_count_after, 0,
            "reset_service_cursors must delete all service_domain_pull_state rows for the service"
        );
    }

    #[test]
    fn reset_service_cursors_does_not_affect_other_services() {
        let db = test_db();
        let svc_a = db
            .add_shared_service("cursor-a", "http://cursor-a/", None)
            .unwrap();
        let svc_b = db
            .add_shared_service("cursor-b", "http://cursor-b/", None)
            .unwrap();

        // Set cursors on both.
        db.raw_conn_for_test()
            .execute(
                "UPDATE services SET relation_cursor = 10 WHERE id = ?1",
                rusqlite::params![svc_a],
            )
            .unwrap();
        db.raw_conn_for_test()
            .execute(
                "UPDATE services SET relation_cursor = 20 WHERE id = ?1",
                rusqlite::params![svc_b],
            )
            .unwrap();
        db.raw_conn_for_test()
            .execute(
                "INSERT INTO service_domain_pull_state(service_id, domain, mapping_cursor)
                 VALUES(?1, 'sha256', 500)",
                rusqlite::params![svc_b],
            )
            .unwrap();

        // Reset only svc_a.
        db.reset_service_cursors(svc_a).unwrap();

        // svc_a cleared.
        assert_eq!(db.relation_cursor(svc_a).unwrap(), None);
        // svc_b untouched.
        assert_eq!(db.relation_cursor(svc_b).unwrap(), Some(20));
        let b_domain_count: i64 = db
            .raw_conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM service_domain_pull_state WHERE service_id = ?1",
                rusqlite::params![svc_b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            b_domain_count, 1,
            "reset_service_cursors must not touch other services' domain state"
        );
    }

    // ── origin_files_matching / search origin predicate tests (#165) ────────

    /// Build a Query with a single System(Origin{name}) predicate.
    fn origin_query(name: Option<&str>) -> Query {
        Query {
            predicates: vec![Predicate::System(SystemPredicate::Origin {
                name: name.map(|s| s.to_string()),
            })],
        }
    }

    /// Build a Query with a single NotSystem(Origin{name}) predicate.
    fn not_origin_query(name: Option<&str>) -> Query {
        Query {
            predicates: vec![Predicate::NotSystem(SystemPredicate::Origin {
                name: name.map(|s| s.to_string()),
            })],
        }
    }

    #[test]
    fn origin_search_named_matches_files_with_that_origin() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();

        insert_named(&db, b"origin-a", "oa.png");
        insert_named(&db, b"origin-b", "ob.png");
        let fc = insert_named(&db, b"origin-c", "oc.png");
        let ha = hash_bytes(b"origin-a");
        let hb = hash_bytes(b"origin-b");

        // fa tagged with origin "wd14-tagger", fb with NULL origin, fc not tagged.
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[
                (
                    ha,
                    vec![(Tag::parse("x:a").unwrap(), Some("wd14-tagger".to_string()))],
                ),
                (hb, vec![(Tag::parse("x:b").unwrap(), None)]),
            ],
        )
        .unwrap();
        // fc: add local tag (null origin via add_mapping)
        tag_file(&db, fc, db.local_service_id().unwrap(), "x:c");

        let q = origin_query(Some("wd14-tagger"));
        let res: Vec<_> = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].hash, ha);
    }

    #[test]
    fn origin_search_named_case_insensitive() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();

        let ha = hash_bytes(b"ci-file");
        insert_named(&db, b"ci-file", "ci.png");
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(Tag::parse("x:ci").unwrap(), Some("WD14-Tagger".to_string()))],
            )],
        )
        .unwrap();

        // Query with different case — should still match.
        let q = origin_query(Some("wd14-tagger"));
        let res = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(
            res.len(),
            1,
            "case-insensitive: should match despite case difference"
        );
    }

    #[test]
    fn origin_search_manual_matches_null_origin_files() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();

        let fa = insert_named(&db, b"manual-a", "ma.png");
        insert_named(&db, b"manual-b", "mb.png");
        let hb = hash_bytes(b"manual-b");
        let hfa = hash_bytes(b"manual-a");

        // fa: local (null origin), fb: pulled with named origin.
        tag_file(&db, fa, local, "t:manual");
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                hb,
                vec![(
                    Tag::parse("t:pulled").unwrap(),
                    Some("some-tagger".to_string()),
                )],
            )],
        )
        .unwrap();

        // system:origin=manual → None → matches fa (null origin)
        let q = origin_query(None);
        let res = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        let hashes: Vec<_> = res.iter().map(|l| l.hash).collect();
        assert!(
            hashes.contains(&hfa),
            "manual should match file with null origin"
        );
        assert!(
            !hashes.contains(&hb),
            "manual should not match file with named origin only"
        );
    }

    #[test]
    fn origin_search_unknown_name_returns_empty() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();
        let ha = hash_bytes(b"some-file");
        insert_named(&db, b"some-file", "sf.png");
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(Tag::parse("t:x").unwrap(), Some("real-tagger".to_string()))],
            )],
        )
        .unwrap();

        let q = origin_query(Some("nonesuch"));
        let res = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert!(
            res.is_empty(),
            "unknown origin name should return empty set"
        );
    }

    #[test]
    fn origin_search_empty_scope_returns_empty() {
        let db = Db::open_in_memory().unwrap();
        // Exercise the `services.is_empty()` short-circuit directly: no real
        // `search` path produces an empty scope, so call the resolver itself.
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();
        let ha = hash_bytes(b"escope-file");
        insert_named(&db, b"escope-file", "es.png");
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(Tag::parse("t:y").unwrap(), Some("some-tagger".to_string()))],
            )],
        )
        .unwrap();

        let blocks = BlockMatcher {
            suppressed_tag_ids: HashSet::new(),
            local_service_ids: HashSet::new(),
        };
        let res = db
            .origin_files_matching(Some("some-tagger"), &[], &blocks, None)
            .unwrap();
        assert!(
            res.is_empty(),
            "empty service scope must short-circuit to empty"
        );
        // Sanity: the same origin DOES match once a scope is supplied.
        let res = db
            .origin_files_matching(Some("some-tagger"), &[svc], &blocks, None)
            .unwrap();
        assert_eq!(res.len(), 1, "non-empty scope finds the pulled mapping");
    }

    #[test]
    fn origin_search_negation_subtracts() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();
        let local = db.local_service_id().unwrap();

        let ha = hash_bytes(b"neg-a");
        let hb = hash_bytes(b"neg-b");
        insert_named(&db, b"neg-a", "neg-a.png");
        let fb = insert_named(&db, b"neg-b", "neg-b.png");

        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(Tag::parse("t:na").unwrap(), Some("hydrus".to_string()))],
            )],
        )
        .unwrap();
        tag_file(&db, fb, local, "t:nb");

        // -system:origin=hydrus should exclude fa, keep fb.
        let q = not_origin_query(Some("hydrus"));
        let res = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        let hashes: Vec<_> = res.iter().map(|l| l.hash).collect();
        assert!(
            !hashes.contains(&ha),
            "-origin=hydrus must exclude file with that origin"
        );
        assert!(
            hashes.contains(&hb),
            "-origin=hydrus must keep file without that origin"
        );
    }

    #[test]
    fn origin_search_negation_only_seeds_all_files() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();
        let local = db.local_service_id().unwrap();

        let ha = hash_bytes(b"seed-a");
        let hb = hash_bytes(b"seed-b");
        insert_named(&db, b"seed-a", "seed-a.png");
        let fb = insert_named(&db, b"seed-b", "seed-b.png");

        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(Tag::parse("t:hydrus").unwrap(), Some("hydrus".to_string()))],
            )],
        )
        .unwrap();
        tag_file(&db, fb, local, "t:local");

        // A lone -system:origin=hydrus should seed all files and subtract hydrus-origin ones.
        let q = not_origin_query(Some("hydrus"));
        let res = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        let hashes: Vec<_> = res.iter().map(|l| l.hash).collect();
        assert!(
            !hashes.contains(&ha),
            "file with hydrus origin must be subtracted"
        );
        assert!(
            hashes.contains(&hb),
            "file without hydrus origin must be in result"
        );
    }

    #[test]
    fn origin_search_positive_combined_with_tag_predicate() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();

        let ha = hash_bytes(b"combo-a");
        let hb = hash_bytes(b"combo-b");
        insert_named(&db, b"combo-a", "combo-a.png");
        insert_named(&db, b"combo-b", "combo-b.png");

        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[
                (
                    ha,
                    vec![(
                        Tag::parse("rating:safe").unwrap(),
                        Some("wd14-tagger".to_string()),
                    )],
                ),
                (
                    hb,
                    vec![(
                        Tag::parse("rating:safe").unwrap(),
                        Some("other-tagger".to_string()),
                    )],
                ),
            ],
        )
        .unwrap();

        // system:origin=wd14-tagger AND rating:safe → only ha.
        let q = Query {
            predicates: vec![
                Predicate::System(SystemPredicate::Origin {
                    name: Some("wd14-tagger".into()),
                }),
                Predicate::Tag(Tag::parse("rating:safe").unwrap(), MatchMode::Expanded),
            ],
        };
        let res = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].hash, ha);
    }

    #[test]
    fn origin_search_visibility_block_rule_excludes_mapping() {
        let db = Db::open_in_memory().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();

        let ha = hash_bytes(b"block-origin-file");
        insert_named(&db, b"block-origin-file", "bof.png");
        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(
                    Tag::parse("blocked:tag").unwrap(),
                    Some("bad-tagger".to_string()),
                )],
            )],
        )
        .unwrap();

        // Without block rule: file is found.
        let q = origin_query(Some("bad-tagger"));
        let before_block = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert_eq!(before_block.len(), 1);

        // Add block rule on the tag — mapping is suppressed → not visible.
        db.add_block_rule(BlockKind::Tag, "blocked:tag", None)
            .unwrap();
        let after_block = db
            .search(&q, ReadScope::Merged, Expansion::Expanded)
            .unwrap();
        assert!(
            after_block.is_empty(),
            "block rule must suppress visibility of origin match"
        );
    }

    #[test]
    fn origin_search_local_only_scope_excludes_pulled_named_origin() {
        let db = Db::open_in_memory().unwrap();
        let local = db.local_service_id().unwrap();
        let svc = db.add_shared_service("repo", "http://repo/", None).unwrap();

        let ha = hash_bytes(b"scope-test-a");
        let fb = insert_named(&db, b"scope-test-b", "st-b.png");
        insert_named(&db, b"scope-test-a", "st-a.png");

        db.merge_pulled_mappings_in_domain(
            svc,
            "blake3",
            &[(
                ha,
                vec![(
                    Tag::parse("t:pulled").unwrap(),
                    Some("tagger-x".to_string()),
                )],
            )],
        )
        .unwrap();
        // fb: local mapping (null origin)
        tag_file(&db, fb, local, "t:local");

        // LocalOnly: named-origin query must return empty (shared svc excluded).
        let q_named = origin_query(Some("tagger-x"));
        let res_named = db
            .search(&q_named, ReadScope::LocalOnly, Expansion::Expanded)
            .unwrap();
        assert!(
            res_named.is_empty(),
            "LocalOnly excludes shared service, named origin must yield empty"
        );

        // LocalOnly: manual query must return fb (local mapping has null origin).
        let q_manual = origin_query(None);
        let res_manual = db
            .search(&q_manual, ReadScope::LocalOnly, Expansion::Expanded)
            .unwrap();
        let hashes: Vec<_> = res_manual.iter().map(|l| l.hash).collect();
        assert!(
            hashes.contains(&hash_bytes(b"scope-test-b")),
            "LocalOnly manual must return local file"
        );
    }
}
