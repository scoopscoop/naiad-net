//! Hash-domain routing (design §1): which hash domains this node serves, and
//! which backend answers the added SHA-256 domain.
//!
//! The bridge is **additive**. A repo always serves its native domain
//! (`[serve].hash_domain`, unchanged in every respect); enabling the bridge in
//! snapshot mode *adds* a SHA-256 domain answered straight out of a read-only
//! Hydrus snapshot, with no seed and no materialized store. Mirror mode is
//! unchanged: there the native store is itself SHA-256-keyed, so the repo
//! serves exactly one domain and no extra backend exists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use naiad_core::LockRecover;
use naiad_netproto::HashDomain;
use naiad_plugin_hydrus::HydrusDb;
use naiad_plugin_hydrus::schema::SERVICE_TYPE_TAG_REPOSITORY;

use crate::bridge::sidecar::Sidecar;
use crate::settings::{BridgeConfig, BridgeMode, DEFAULT_BRIDGE_MAX_QUERY_BITS};

/// Minimum `max_query_bits` for snapshot-mode SHA-256 bucket queries.
///
/// A 0-bit bucket query is a whole-snapshot scan through a single Mutex lock —
/// the same door that `GET /repo/snapshot` closes for this domain. Any
/// configured ceiling below this value is raised once at startup by
/// [`DomainConfig::from_settings`] so that caps, the floor check in the HTTP
/// layer, and effective query bits always agree: ceiling ≥ floor ⇒ every query
/// that passes the floor check is answered at ≥ floor bits, never at 0.
pub const SNAPSHOT_MIN_QUERY_BITS: u32 = 8;

/// A static Hydrus snapshot serving SHA-256-domain queries.
///
/// The three snapshot files are opened `SQLITE_OPEN_READ_ONLY` with
/// `?immutable=1` by [`HydrusDb::open`], which is exactly right for a static
/// download: no writer exists, so SQLite skips all locking and WAL/shm work.
/// The connection is `Send` but not `Sync`, so it lives behind a `Mutex`;
/// queries are short range scans, and `LockRecover` keeps a poisoned lock from
/// taking the server down.
pub struct SnapshotBackend {
    db: Mutex<HydrusDb>,
    dir: PathBuf,
    service_id: i64,
}

impl SnapshotBackend {
    /// Open the snapshot at `dir` and pin the tag service to query.
    ///
    /// An explicit `service_id` is used as-is (after checking it exists); it is
    /// the operator's deliberate choice and bypasses all auto-discovery guards.
    ///
    /// `service_id` of `None` auto-discovers — see [`Self::discover_service_id`].
    /// On a full Hydrus *client* database this prefers the tag **repository**
    /// (the PTR) over the low-id local tag services, and refuses to silently
    /// serve an empty service while a populated one exists (#167).
    ///
    /// # Errors
    /// Returns an error naming `dir` if the directory or any of the three
    /// `client*.db` files is missing or unreadable, if the snapshot has no tag
    /// service at all, if auto-discovery is ambiguous (several tag
    /// repositories), or if the auto-discovered service is empty while another
    /// candidate is not. Callers must treat this as fatal at startup — a repo
    /// must never start and then serve empty SHA-256 results (spec §6).
    pub fn open(dir: &Path, service_id: Option<i64>) -> anyhow::Result<Self> {
        let db = HydrusDb::open(dir)
            .with_context(|| format!("opening Hydrus snapshot at {}", dir.display()))?;
        let mut ids = db.tag_service_ids().with_context(|| {
            format!(
                "discovering tag services in Hydrus snapshot {}",
                dir.display()
            )
        })?;
        ids.sort_unstable();
        let service_id = match service_id {
            Some(id) => {
                if !ids.contains(&id) {
                    return Err(anyhow::anyhow!(
                        "Hydrus snapshot {} has no tag service with id {}; \
                         available ids are: {:?}",
                        dir.display(),
                        id,
                        ids
                    ));
                }
                id
            }
            None => Self::discover_service_id(&db, dir, &ids)?,
        };
        tracing::info!(
            target: "bridge",
            snapshot_dir = %dir.display(),
            service_id,
            "snapshot-mode sha256 backend ready (no seed, no store)"
        );
        Ok(Self {
            db: Mutex::new(db),
            dir: dir.to_path_buf(),
            service_id,
        })
    }

    /// Pick the tag service to serve when none is configured.
    ///
    /// `ids` is the sorted list of ids that have a `current_mappings_<id>`
    /// table. Resolution order:
    ///
    /// 1. **Prefer the Hydrus `services` table.** If it lists exactly one tag
    ///    *repository* (`service_type` [`SERVICE_TYPE_TAG_REPOSITORY`]) among
    ///    the candidates, use it. This is what fixes #167: on a full client DB
    ///    the low ids are local tag services and the PTR sits higher, so a
    ///    lowest-id guess lands on an empty "my tags" service.
    /// 2. **Several repositories → refuse**, listing them, so the operator pins
    ///    one with `snapshot_service_id` /
    ///    `NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID`.
    /// 3. **No `services` table, or no repository among the candidates →**
    ///    fall back to the lowest id (the historical behaviour), *but* refuse
    ///    if that pick has zero mappings while another candidate is non-empty —
    ///    the tell-tale of a misdirected full client DB.
    ///
    /// # Errors
    /// Returns an error when there is no tag service, when discovery is
    /// ambiguous, or when the fallback pick is empty while another candidate
    /// holds mappings.
    fn discover_service_id(db: &HydrusDb, dir: &Path, ids: &[i64]) -> anyhow::Result<i64> {
        if ids.is_empty() {
            return Err(anyhow::anyhow!(
                "Hydrus snapshot {} has no tag service (no current_mappings_<id> \
                 table); it is not a tag-repository snapshot",
                dir.display()
            ));
        }

        // (1)/(2): trust the services table when it is present.
        let types = db.service_types().with_context(|| {
            format!(
                "reading the services table of Hydrus snapshot {}",
                dir.display()
            )
        })?;
        if let Some(types) = types {
            let repos: Vec<i64> = ids
                .iter()
                .copied()
                .filter(|id| types.get(id) == Some(&SERVICE_TYPE_TAG_REPOSITORY))
                .collect();
            match repos.as_slice() {
                [only] => {
                    tracing::info!(
                        target: "bridge",
                        service_id = *only,
                        "snapshot auto-discovery: chose the tag repository named by the \
                         Hydrus services table"
                    );
                    // A correctly-typed repository is trusted even if empty (a
                    // freshly downloaded PTR may not be processed yet); the
                    // empty-service guard is only for the structural guess.
                    return Ok(*only);
                }
                [] => {
                    // Fall through to the structural guess + empty guard below.
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Hydrus snapshot {} has {} tag repositories (service ids {:?}); \
                         auto-discovery cannot choose. Set [bridge].snapshot_service_id \
                         (or NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID) to the one to serve.",
                        dir.display(),
                        repos.len(),
                        repos
                    ));
                }
            }
        }

        // (3): structural fallback — lowest id, guarded against the empty trap.
        let picked = ids[0];
        Self::reject_empty_auto_pick(db, dir, picked, ids)?;
        Ok(picked)
    }

    /// Refuse a structural auto-pick that is empty while another candidate is
    /// not — the #167 signature of a full client DB whose PTR lives elsewhere.
    ///
    /// A deliberately-set `snapshot_service_id` never reaches here, so an
    /// operator can always choose an empty service on purpose.
    ///
    /// # Errors
    /// Returns an error naming both the empty pick and a non-empty candidate.
    fn reject_empty_auto_pick(
        db: &HydrusDb,
        dir: &Path,
        picked: i64,
        ids: &[i64],
    ) -> anyhow::Result<()> {
        let picked_count = db.current_mapping_count(picked).with_context(|| {
            format!(
                "counting mappings for auto-discovered service {picked} in Hydrus snapshot {}",
                dir.display()
            )
        })?;
        if picked_count > 0 {
            return Ok(());
        }
        let non_empty = ids
            .iter()
            .copied()
            .find(|&id| id != picked && db.current_mapping_count(id).unwrap_or(0) > 0);
        if let Some(other) = non_empty {
            return Err(anyhow::anyhow!(
                "Hydrus snapshot {} auto-discovered tag service {} but it has zero \
                 mappings, while service {} is non-empty. This is almost certainly a \
                 full Hydrus client database whose tag repository (e.g. the public tag \
                 repository) sits under a different id. Set [bridge].snapshot_service_id \
                 (or NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID) explicitly to the repository's id.",
                dir.display(),
                picked,
                other
            ));
        }
        Ok(())
    }

    /// The directory this backend reads.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The Hydrus tag-service id being queried.
    #[must_use]
    pub fn service_id(&self) -> i64 {
        self.service_id
    }

    /// Every `sha256_hex → tags` mapping in the bucket `lo_hex` at
    /// `prefix_bits`, shaped like a partial [`naiad_netproto::Snapshot`] body.
    /// Tags are sorted and deduped so the response is deterministic.
    ///
    /// # Errors
    /// Returns an error if `lo_hex` is not a 64-char hex hash or the query
    /// fails.
    pub fn bucket(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        budget: usize,
    ) -> anyhow::Result<(BTreeMap<String, Vec<String>>, usize)> {
        let (rows, spent) = {
            let db = self.db.lock_recover();
            db.mappings_for_prefix(lo_hex, prefix_bits, self.service_id, budget)
                .with_context(|| {
                    format!(
                        "querying Hydrus snapshot {} at {prefix_bits} prefix bits",
                        self.dir.display()
                    )
                })?
        };
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (sha, tag) in rows {
            out.entry(sha).or_default().push(tag);
        }
        for tags in out.values_mut() {
            tags.sort();
            tags.dedup();
        }
        Ok((out, spent))
    }
}

impl std::fmt::Debug for SnapshotBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotBackend")
            .field("dir", &self.dir)
            .field("service_id", &self.service_id)
            .finish_non_exhaustive()
    }
}

/// A SHA-256-domain bucket backend: snapshot (direct Hydrus read) or sidecar.
/// Both answer the same `(sha256_hex → tags)` shape, so the HTTP layer treats
/// them uniformly through this trait (F4).
pub trait Sha256Backend: Send + Sync + std::fmt::Debug {
    /// Every `sha256_hex → tags` mapping in the bucket `lo_hex` at `prefix_bits`
    /// (256 = exact), sorted/deduped, charging `budget` per emitted row.
    fn bucket(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        budget: usize,
    ) -> anyhow::Result<(BTreeMap<String, Vec<String>>, usize)>;
}

impl Sha256Backend for SnapshotBackend {
    fn bucket(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        budget: usize,
    ) -> anyhow::Result<(BTreeMap<String, Vec<String>>, usize)> {
        SnapshotBackend::bucket(self, lo_hex, prefix_bits, budget)
    }
}

/// The sidecar SHA-256 backend: a round-robin pool of read-only `Sidecar`
/// connections (#208).
///
/// Previously a single `Mutex<Sidecar>` serialised every concurrent bucket
/// query for the full duration of its range scan + defs resolution (tens of
/// ms cold on spinning disk). The pool eliminates that bottleneck: N
/// concurrent callers each hold a different slot's lock simultaneously.
///
/// Each connection is opened `SQLITE_OPEN_READ_ONLY`; SQLite WAL allows any
/// number of concurrent read-only openers on the same file.
pub struct SidecarBackend {
    /// Round-robin pool; slot `i` is `pool[i % pool.len()]`.
    pool: Vec<Mutex<Sidecar>>,
    /// Monotonically incrementing cursor; wrapped mod pool length on use.
    /// Named `cursor` to match the in-crate `ReadPool` idiom (`http.rs`).
    cursor: AtomicUsize,
    path: PathBuf,
}

impl SidecarBackend {
    /// Open `pool_size` read-only sidecar connections at `path`, validating
    /// schema stamps on every connection.
    ///
    /// `pool_size` is clamped to `[1, 64]` so a misconfigured zero never
    /// deadlocks and an absurd value does not exhaust file descriptors silently.
    ///
    /// # Errors
    /// Returns an error (naming `path`) if the file is missing, unreadable, a
    /// stamp-less legacy `bridge-state.db`, or a version/width mismatch — the
    /// caller must treat this as fatal at startup (spec §Error handling).
    pub fn open(path: &Path, pool_size: usize) -> anyhow::Result<Self> {
        let pool_size = pool_size.clamp(1, 64);
        let mut pool = Vec::with_capacity(pool_size);
        for i in 0..pool_size {
            let conn = Sidecar::open_readonly(path).with_context(|| {
                format!(
                    "opening sidecar read-only connection {}/{} at {}",
                    i + 1,
                    pool_size,
                    path.display()
                )
            })?;
            if let Err(e) = conn.apply_read_only_serve_pragmas() {
                tracing::warn!(
                    target: "bridge",
                    sidecar = %path.display(),
                    slot = i + 1, pool_size,
                    "apply_read_only_serve_pragmas failed; serving untuned: {e:#}",
                );
            }
            pool.push(Mutex::new(conn));
        }
        tracing::info!(
            target: "bridge",
            sidecar = %path.display(),
            pool_size,
            "sidecar sha256 backend ready (read-pool)"
        );
        Ok(Self {
            pool,
            cursor: AtomicUsize::new(0),
            path: path.to_path_buf(),
        })
    }

    /// The sidecar file this backend serves.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Advance the cursor one step and return the slot index that was selected.
    /// Equivalent to one `bucket()` call's slot-selection without the I/O.
    /// Used in tests to verify round-robin ordering without DB work.
    #[cfg(test)]
    pub(crate) fn advance_cursor(&self) -> usize {
        self.cursor.fetch_add(1, Ordering::Relaxed) % self.pool.len()
    }

    /// Number of connections in the pool. Used in tests to verify clamping.
    #[cfg(test)]
    pub(crate) fn pool_len(&self) -> usize {
        self.pool.len()
    }

    /// Query a PRAGMA on the given pool slot. Used in tests to verify that
    /// `SidecarBackend::open` applies `apply_read_only_serve_pragmas` to each
    /// connection.
    #[cfg(test)]
    pub(crate) fn pool_pragma(&self, slot: usize, name: &str) -> i64 {
        self.pool[slot]
            .lock_recover()
            .conn()
            .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .expect("PRAGMA query in pool_pragma must succeed")
    }
}

impl Sha256Backend for SidecarBackend {
    fn bucket(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        budget: usize,
    ) -> anyhow::Result<(BTreeMap<String, Vec<String>>, usize)> {
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.pool.len();
        self.pool[idx]
            .lock_recover()
            .bucket(lo_hex, prefix_bits, budget)
            .with_context(|| {
                format!(
                    "querying sidecar {} at {prefix_bits} bits",
                    self.path.display()
                )
            })
    }
}

impl std::fmt::Debug for SidecarBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarBackend")
            .field("path", &self.path)
            .field("pool_size", &self.pool.len())
            .finish_non_exhaustive()
    }
}

/// Which hash domains this node serves, and how.
#[derive(Clone, Debug)]
pub struct DomainConfig {
    /// The repo's native domain — the domain its own `RepoStore` is keyed by.
    /// Advertised as `caps.hash_domain`, i.e. what a pre-dual-domain client
    /// uses. Never rewritten by the bridge.
    pub native: HashDomain,
    /// The added SHA-256 backend (snapshot OR sidecar), when the bridge adds a
    /// sha256 domain beside the native one. A single field so every caps/HTTP
    /// gate that reads it covers both backends (F2).
    pub added_sha256: Option<Arc<dyn Sha256Backend>>,
    /// Server-set precision ceiling for SHA-256-domain queries, in prefix bits.
    pub max_query_bits: u32,
    /// Server-set minimum prefix bits required for SHA-256-domain bucket queries.
    /// Requests below this floor are rejected with 400. Raised from the configured
    /// value to `[SNAPSHOT_MIN_QUERY_BITS, max_query_bits]` by `from_settings`.
    pub min_query_bits: u32,
}

impl DomainConfig {
    /// A node with no bridge: one native domain, nothing added.
    #[must_use]
    pub fn native_only(native: HashDomain) -> Self {
        Self {
            native,
            added_sha256: None,
            max_query_bits: DEFAULT_BRIDGE_MAX_QUERY_BITS,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
        }
    }

    /// Build the routing config from resolved settings, failing fast on a
    /// snapshot that cannot be opened.
    ///
    /// A relative `snapshot_dir` resolves beside the repo database, the same
    /// rule `[bridge].state_db` uses.
    ///
    /// # Errors
    /// Returns an error when `mode = "snapshot"` and `snapshot_dir` is unset,
    /// or when the snapshot cannot be opened (spec §6 row 3). The caller must
    /// exit rather than start and serve empty results.
    ///
    /// `sidecar_pool_size` is forwarded to [`SidecarBackend::open`] when
    /// `mode = "sidecar"`.  Pass `serve.read_connections` so operators who
    /// raise the native-store pool size (#202) scale the sidecar pool too.
    pub fn from_settings(
        native: HashDomain,
        bridge: &BridgeConfig,
        db_path: &Path,
        sidecar_pool_size: usize,
    ) -> anyhow::Result<Self> {
        // Reject the impossible combination: snapshot mode adds a sha256 domain
        // *beside* the native domain, so the native domain must not itself be
        // sha256. If it were, the two would collide: `served()` deduplicates
        // them to one entry, the SnapshotBackend becomes unreachable from every
        // handler, and all sha256 queries would fall through to the empty
        // RepoStore — returning 200 `{"tags":{}}` for every bucket. Refuse at
        // startup rather than come up serving silent empty results (spec §6).
        if bridge.enabled
            && matches!(bridge.mode, BridgeMode::Snapshot | BridgeMode::Sidecar)
            && native == HashDomain::Sha256
        {
            let mode = bridge.mode;
            return Err(anyhow::anyhow!(
                "[serve] hash_domain = \"sha256\" and [bridge] mode = \"{mode}\" are \
                 incompatible: {mode} mode adds a sha256 domain beside the repo's native \
                 domain, so the native domain must not itself be sha256. \
                 Fix: either use [bridge] mode = \"mirror\" (to run a PTR mirror whose \
                 native store is sha256-keyed), or set [serve] hash_domain = \"blake3\" \
                 (or omit hash_domain to use the default) so {mode} mode adds sha256 \
                 alongside a blake3 native domain."
            ));
        }
        let max_query_bits = bridge.max_query_bits.min(256);
        // Compute the effective query floor. The clamp ceiling for this
        // calculation uses max(max_query_bits, SNAPSHOT_MIN_QUERY_BITS) so
        // the result is consistent whether max is raised below in the snapshot
        // path or not. This is computed once before the early-return branch so
        // disabled/mirror paths carry the value too (it is inert there, but
        // keeps the struct honest).
        let min_clamp_ceil = max_query_bits.max(SNAPSHOT_MIN_QUERY_BITS);
        let min_query_bits = {
            let f = bridge.min_query_bits;
            if f < SNAPSHOT_MIN_QUERY_BITS {
                tracing::warn!(
                    target: "bridge",
                    configured = f,
                    raised_to = SNAPSHOT_MIN_QUERY_BITS,
                    "[bridge].min_query_bits ({}) is below the hard minimum ({}); \
                     raising to {}",
                    f,
                    SNAPSHOT_MIN_QUERY_BITS,
                    SNAPSHOT_MIN_QUERY_BITS,
                );
                SNAPSHOT_MIN_QUERY_BITS
            } else if f > min_clamp_ceil {
                tracing::warn!(
                    target: "bridge",
                    configured = f,
                    lowered_to = min_clamp_ceil,
                    "[bridge].min_query_bits ({}) is above the effective ceiling ({}); \
                     lowering to match the ceiling",
                    f,
                    min_clamp_ceil,
                );
                min_clamp_ceil
            } else {
                f
            }
        };
        // Sidecar mode: open the sidecar index at [bridge].state_db and add
        // sha256 via the shared Sha256Backend trait.
        if bridge.enabled && bridge.mode == BridgeMode::Sidecar {
            let path = crate::settings::resolve_beside_db(db_path, &bridge.state_db);
            let backend = SidecarBackend::open(&path, sidecar_pool_size)?;
            // Same floor as snapshot mode: never advertise a ceiling below the hard min.
            let max_query_bits = if max_query_bits < SNAPSHOT_MIN_QUERY_BITS {
                tracing::warn!(
                    target: "bridge",
                    configured = max_query_bits,
                    raised_to = SNAPSHOT_MIN_QUERY_BITS,
                    "[bridge].max_query_bits below the sidecar floor; raising"
                );
                SNAPSHOT_MIN_QUERY_BITS
            } else {
                max_query_bits
            };
            return Ok(Self {
                native,
                added_sha256: Some(Arc::new(backend) as Arc<dyn Sha256Backend>),
                max_query_bits,
                min_query_bits,
            });
        }
        if !bridge.enabled || bridge.mode != BridgeMode::Snapshot {
            // Disabled, or mirror mode: the native store answers everything,
            // exactly as it does today.
            // Raise max to at least SNAPSHOT_MIN_QUERY_BITS so the struct
            // always satisfies min_query_bits <= max_query_bits — the same
            // guarantee the snapshot path provides after its explicit raise.
            // This is inert on the disabled/mirror path (no snapshot backend
            // means no floor check ever runs), but keeps the invariant intact.
            return Ok(Self {
                native,
                added_sha256: None,
                max_query_bits: max_query_bits.max(SNAPSHOT_MIN_QUERY_BITS),
                min_query_bits,
            });
        }
        let configured = bridge
            .snapshot_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "[bridge].mode = \"snapshot\" requires [bridge].snapshot_dir \
                     (or NAIAD_REPO_BRIDGE_SNAPSHOT_DIR): the directory holding \
                     client.db, client.master.db and client.mappings.db"
                )
            })?;
        let dir = crate::settings::resolve_beside_db(db_path, configured);
        let backend = SnapshotBackend::open(&dir, bridge.snapshot_service_id)?;
        // Enforce a floor: a configured ceiling below SNAPSHOT_MIN_QUERY_BITS
        // would let the HTTP floor check produce an inverted range in its error
        // message ("served range: 8..=4") and would allow queries that pass the
        // floor to be served at fewer effective bits than the floor (e.g. a
        // ceiling of 4 clamped down a floor-passing 8-bit request to 4 bits,
        // giving a whole-prefix-4 scan). Raise exactly once at startup so caps,
        // the floor check, and effective bits automatically agree.
        let max_query_bits = if max_query_bits < SNAPSHOT_MIN_QUERY_BITS {
            tracing::warn!(
                target: "bridge",
                configured = max_query_bits,
                raised_to = SNAPSHOT_MIN_QUERY_BITS,
                "[bridge].max_query_bits is below the snapshot floor ({}); \
                 raising to {} so caps and bucket queries are consistent",
                SNAPSHOT_MIN_QUERY_BITS,
                SNAPSHOT_MIN_QUERY_BITS,
            );
            SNAPSHOT_MIN_QUERY_BITS
        } else {
            max_query_bits
        };
        Ok(Self {
            native,
            added_sha256: Some(Arc::new(backend) as Arc<dyn Sha256Backend>),
            max_query_bits,
            min_query_bits,
        })
    }

    /// The domains this node serves, native first.
    ///
    /// A snapshot backend adds `sha256` unless the native domain already *is*
    /// `sha256` (a mirror-mode repo), in which case there is one domain, not a
    /// duplicate.
    #[must_use]
    pub fn served(&self) -> Vec<HashDomain> {
        let mut out = vec![self.native];
        if self.added_sha256.is_some() && self.native != HashDomain::Sha256 {
            out.push(HashDomain::Sha256);
        }
        out
    }

    /// Clamp a client-requested prefix width to this server's ceiling (and to
    /// the 256-bit hash width), warning when the clamp actually bites.
    ///
    /// Mirrors the client-side clamp in `ops.rs`: querying *coarser* than
    /// requested is always correct — the response is a superset the client
    /// filters locally — so clamping costs bandwidth, never correctness
    /// (spec §6, last row).
    #[must_use]
    pub fn clamp_query_bits(&self, requested: u32) -> u32 {
        let effective = requested.min(self.max_query_bits).min(256);
        if effective != requested {
            tracing::warn!(
                target: "bridge",
                requested,
                ceiling = self.max_query_bits,
                effective,
                "sha256-domain query finer than max_query_bits; clamping"
            );
        }
        effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_netproto::HashDomain;

    const SHA_A: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
    const SHA_B: &str = "aabbccddeeff00112233445566778899aabbccddeeff001122334455667788ff";

    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(
            dir.path(),
            9,
            &[
                (SHA_A, "character:samus"),
                (SHA_A, "maid"),
                (SHA_B, "series:metroid"),
            ],
        )
        .expect("write snapshot fixture");
        dir
    }

    #[test]
    fn snapshot_backend_opens_fixture_and_answers_prefix() {
        let dir = fixture_dir();
        let backend = SnapshotBackend::open(dir.path(), Some(9)).expect("open snapshot backend");
        assert_eq!(backend.service_id(), 9);

        // Exact-hash query for SHA_A.
        let hits = backend
            .bucket(SHA_A, 256, usize::MAX)
            .expect("exact-hash bucket")
            .0;
        let tags = hits.get(SHA_A).expect("SHA_A present");
        assert_eq!(
            tags,
            &vec!["character:samus".to_string(), "maid".to_string()],
            "tags sorted and deduped"
        );
        assert!(!hits.contains_key(SHA_B), "other hashes are out of bucket");

        // An 8-bit bucket over 0xaa picks up SHA_B only.
        let lo = "aa".to_string() + &"00".repeat(31);
        let hits = backend.bucket(&lo, 8, usize::MAX).expect("prefix bucket").0;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits.get(SHA_B).map(Vec::len), Some(1));

        // A prefix nothing lives under is empty, never an error.
        let lo = "ff".to_string() + &"00".repeat(31);
        assert!(
            backend
                .bucket(&lo, 8, usize::MAX)
                .expect("empty bucket")
                .0
                .is_empty()
        );
    }

    /// A snapshot modelling a full Hydrus **client** DB: a zero-row local tag
    /// service at the low id 9 and the populated tag repository (the PTR) at the
    /// higher id 14 — the exact #167 shape. `service_type` distinguishes them.
    fn full_client_fixture() -> tempfile::TempDir {
        use naiad_plugin_hydrus::fixture::{
            SERVICE_TYPE_LOCAL_TAG, SERVICE_TYPE_TAG_REPOSITORY as REPO, SnapshotService,
            write_snapshot_with_services,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        write_snapshot_with_services(
            dir.path(),
            &[
                SnapshotService {
                    service_id: 9,
                    service_type: SERVICE_TYPE_LOCAL_TAG,
                    name: "my tags",
                    mappings: &[], // the empty local service that #167 wrongly picks
                },
                SnapshotService {
                    service_id: 14,
                    service_type: REPO,
                    name: "public tag repository",
                    mappings: &[
                        (SHA_A, "character:samus"),
                        (SHA_A, "maid"),
                        (SHA_B, "series:metroid"),
                    ],
                },
            ],
        )
        .expect("write full-client fixture");
        dir
    }

    /// #167: on a full client DB, auto-discovery must pick the tag REPOSITORY
    /// (id 14, populated) rather than the lowest id (9, an empty "my tags").
    #[test]
    fn auto_discovery_prefers_tag_repository_over_low_id_local_service() {
        let dir = full_client_fixture();
        let backend =
            SnapshotBackend::open(dir.path(), None).expect("auto-discovery must open the snapshot");
        assert_eq!(
            backend.service_id(),
            14,
            "must serve the populated PTR (id 14), not the empty local 'my tags' (id 9)"
        );
        // And it actually answers with the repository's tags, not empty.
        let hits = backend
            .bucket(SHA_A, 256, usize::MAX)
            .expect("exact-hash bucket")
            .0;
        let tags = hits.get(SHA_A).expect("SHA_A present in the PTR");
        assert_eq!(
            tags,
            &vec!["character:samus".to_string(), "maid".to_string()],
            "discovered service serves the repository's mappings"
        );
    }

    /// When two tag repositories are present, auto-discovery cannot choose and
    /// must refuse, naming the candidate ids so the operator can pin one.
    #[test]
    fn auto_discovery_refuses_when_multiple_tag_repositories() {
        use naiad_plugin_hydrus::fixture::{
            SERVICE_TYPE_TAG_REPOSITORY as REPO, SnapshotService, write_snapshot_with_services,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        write_snapshot_with_services(
            dir.path(),
            &[
                SnapshotService {
                    service_id: 14,
                    service_type: REPO,
                    name: "public tag repository",
                    mappings: &[(SHA_A, "maid")],
                },
                SnapshotService {
                    service_id: 15,
                    service_type: REPO,
                    name: "another tag repository",
                    mappings: &[(SHA_B, "series:metroid")],
                },
            ],
        )
        .expect("write two-repository fixture");

        let err = SnapshotBackend::open(dir.path(), None)
            .expect_err("two tag repositories must not auto-resolve");
        let msg = format!("{err:#}");
        assert!(msg.contains("14"), "error must list candidate id 14: {msg}");
        assert!(msg.contains("15"), "error must list candidate id 15: {msg}");
        assert!(
            msg.contains("snapshot_service_id") || msg.contains("SNAPSHOT_SERVICE_ID"),
            "error must tell the operator how to pin one: {msg}"
        );
    }

    /// An explicit `service_id` overrides discovery entirely: the operator can
    /// pin the empty local service (id 9) on the very DB where auto-discovery
    /// would refuse it. This is the known-good workaround's counterpart.
    #[test]
    fn explicit_service_id_bypasses_discovery_on_full_client_db() {
        let dir = full_client_fixture();
        let backend = SnapshotBackend::open(dir.path(), Some(9))
            .expect("an explicit id is the operator's choice and must be honoured");
        assert_eq!(backend.service_id(), 9);
    }

    /// Structural fallback (no `services` table): the lowest id is empty while a
    /// higher id holds mappings. Auto-discovery must refuse rather than serve
    /// the empty service — the #167 backstop for snapshots lacking type info.
    #[test]
    fn auto_discovery_refuses_empty_low_id_when_higher_id_nonempty_without_services_table() {
        // write_snapshot writes NO services table; service 9 is left empty.
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(dir.path(), 9, &[])
            .expect("write empty low-id snapshot");
        // Add a populated current_mappings_14 (row count is all the guard reads).
        let mappings = rusqlite::Connection::open(dir.path().join("client.mappings.db"))
            .expect("open mappings");
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_14 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_14 VALUES (1, 1);",
            )
            .expect("add populated higher-id service");
        drop(mappings);

        let err = SnapshotBackend::open(dir.path(), None)
            .expect_err("empty low-id pick while a higher id is populated must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains('9'), "error names the empty pick (9): {msg}");
        assert!(
            msg.contains("14"),
            "error names the non-empty candidate (14): {msg}"
        );
        assert!(
            msg.contains("snapshot_service_id") || msg.contains("SNAPSHOT_SERVICE_ID"),
            "error tells the operator how to pin the right one: {msg}"
        );
    }

    /// A correctly-typed tag repository is trusted even when empty: a freshly
    /// downloaded PTR may not be processed yet. Discovery must NOT refuse just
    /// because a local service happens to carry rows.
    #[test]
    fn auto_discovery_trusts_typed_repository_even_if_empty() {
        use naiad_plugin_hydrus::fixture::{
            SERVICE_TYPE_LOCAL_TAG, SERVICE_TYPE_TAG_REPOSITORY as REPO, SnapshotService,
            write_snapshot_with_services,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        write_snapshot_with_services(
            dir.path(),
            &[
                SnapshotService {
                    service_id: 9,
                    service_type: SERVICE_TYPE_LOCAL_TAG,
                    name: "my tags",
                    mappings: &[(SHA_A, "maid")], // local has rows
                },
                SnapshotService {
                    service_id: 14,
                    service_type: REPO,
                    name: "public tag repository",
                    mappings: &[], // repo not yet processed
                },
            ],
        )
        .expect("write empty-repo fixture");

        let backend = SnapshotBackend::open(dir.path(), None)
            .expect("a typed repository is trusted even when empty");
        assert_eq!(
            backend.service_id(),
            14,
            "the tag repository is chosen by type, not by row count"
        );
    }

    #[test]
    fn snapshot_backend_explicit_bogus_service_id_fails_naming_id_and_available() {
        let dir = fixture_dir(); // fixture contains service id 9
        let err = SnapshotBackend::open(dir.path(), Some(42))
            .expect_err("an explicit wrong service id must fail fast at open, not on first query");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("42"),
            "error must name the configured (wrong) id: {msg}"
        );
        assert!(
            msg.contains('9') || msg.contains("available"),
            "error must list the ids actually present: {msg}"
        );
    }

    #[test]
    fn snapshot_backend_explicit_valid_service_id_opens_ok() {
        let dir = fixture_dir(); // fixture contains service id 9
        let backend = SnapshotBackend::open(dir.path(), Some(9))
            .expect("an explicit correct service id must open successfully");
        assert_eq!(backend.service_id(), 9);
    }

    #[test]
    fn snapshot_backend_missing_dir_fails_fast_naming_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("not-a-snapshot");
        let err = SnapshotBackend::open(&missing, None)
            .expect_err("a missing snapshot directory must fail fast");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not-a-snapshot"),
            "message must name the configured path: {msg}"
        );
    }

    #[test]
    fn snapshot_backend_without_mappings_table_fails() {
        // Three valid files, but no current_mappings_<id> table at all.
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(dir.path(), 9, &[])
            .expect("write empty snapshot");
        // write_snapshot(.., &[]) still creates current_mappings_9, so drop it.
        let mappings = rusqlite::Connection::open(dir.path().join("client.mappings.db")).unwrap();
        mappings
            .execute_batch("DROP TABLE current_mappings_9;")
            .unwrap();
        drop(mappings);

        let err = SnapshotBackend::open(dir.path(), None)
            .expect_err("a snapshot with no tag service must fail fast");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no tag service") || msg.contains("current_mappings"),
            "message must explain what is missing: {msg}"
        );
    }

    #[test]
    fn domain_config_served_list() {
        let native = DomainConfig::native_only(HashDomain::Blake3);
        assert_eq!(native.served(), vec![HashDomain::Blake3]);

        let mirror = DomainConfig::native_only(HashDomain::Sha256);
        assert_eq!(
            mirror.served(),
            vec![HashDomain::Sha256],
            "a mirror-mode repo serves only sha256 — today's behaviour, new vocabulary"
        );

        let dir = fixture_dir();
        let backend = SnapshotBackend::open(dir.path(), Some(9)).expect("open backend");
        let dual = DomainConfig {
            native: HashDomain::Blake3,
            added_sha256: Some(std::sync::Arc::new(backend) as Arc<dyn Sha256Backend>),
            max_query_bits: 256,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
        };
        assert_eq!(
            dual.served(),
            vec![HashDomain::Blake3, HashDomain::Sha256],
            "the bridge ADDS sha256; blake3 stays first and stays native"
        );
    }

    #[test]
    fn domain_config_clamps_query_bits() {
        let cfg = DomainConfig {
            native: HashDomain::Blake3,
            added_sha256: None,
            max_query_bits: 24,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
        };
        assert_eq!(cfg.clamp_query_bits(12), 12, "under the ceiling: untouched");
        assert_eq!(cfg.clamp_query_bits(24), 24, "at the ceiling: untouched");
        assert_eq!(cfg.clamp_query_bits(256), 24, "over the ceiling: clamped");

        let wide = DomainConfig {
            max_query_bits: 4096,
            ..cfg
        };
        assert_eq!(wide.clamp_query_bits(9999), 256, "never wider than a hash");
    }

    #[test]
    fn from_settings_requires_snapshot_dir_in_snapshot_mode() {
        use crate::settings::{BridgeConfig, BridgeMode};

        let bridge = BridgeConfig {
            enabled: true,
            mode: BridgeMode::Snapshot,
            snapshot_dir: None,
            snapshot_service_id: None,
            max_query_bits: 256,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
            ptr_url: String::new(),
            ptr_key: String::new(),
            state_db: "bridge-state.db".into(),
        };
        let err = DomainConfig::from_settings(
            HashDomain::Blake3,
            &bridge,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect_err("snapshot mode without a directory must not start");
        let msg = format!("{err:#}");
        assert!(msg.contains("snapshot_dir"), "message names the key: {msg}");

        // Disabled bridge: no backend, no error, native-only.
        let off = BridgeConfig {
            enabled: false,
            ..bridge
        };
        let cfg = DomainConfig::from_settings(
            HashDomain::Blake3,
            &off,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("a disabled bridge changes nothing");
        assert_eq!(cfg.served(), vec![HashDomain::Blake3]);
    }

    /// On the disabled/mirror early-return path, `min_query_bits` must never
    /// exceed `max_query_bits` — the struct invariant `min <= max` must hold
    /// even when the configured ceiling is below `SNAPSHOT_MIN_QUERY_BITS`.
    #[test]
    fn from_settings_disabled_mirror_maintains_min_le_max_invariant() {
        use crate::settings::{BridgeConfig, BridgeMode};

        // Configured ceiling=4 (below SNAPSHOT_MIN_QUERY_BITS=8).
        // Without the fix, min_query_bits would be clamped to 8 while
        // max_query_bits stays at 4, violating min <= max.
        let bridge_disabled = BridgeConfig {
            enabled: false,
            mode: BridgeMode::Mirror,
            snapshot_dir: None,
            snapshot_service_id: None,
            max_query_bits: 4,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
            ptr_url: String::new(),
            ptr_key: String::new(),
            state_db: "bridge-state.db".into(),
        };
        let cfg = DomainConfig::from_settings(
            HashDomain::Blake3,
            &bridge_disabled,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("disabled bridge with sub-floor ceiling must not error");
        assert!(
            cfg.min_query_bits <= cfg.max_query_bits,
            "min_query_bits ({}) must be <= max_query_bits ({}) on the disabled path",
            cfg.min_query_bits,
            cfg.max_query_bits,
        );

        // Also check mirror mode with ceiling=4.
        let bridge_mirror = BridgeConfig {
            enabled: true,
            mode: BridgeMode::Mirror,
            max_query_bits: 4,
            ..bridge_disabled
        };
        let cfg_mirror = DomainConfig::from_settings(
            HashDomain::Blake3,
            &bridge_mirror,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("mirror mode with sub-floor ceiling must not error");
        assert!(
            cfg_mirror.min_query_bits <= cfg_mirror.max_query_bits,
            "min_query_bits ({}) must be <= max_query_bits ({}) on the mirror path",
            cfg_mirror.min_query_bits,
            cfg_mirror.max_query_bits,
        );
    }

    #[test]
    fn from_settings_clamps_sub_floor_max_query_bits_upward() {
        use crate::settings::{BridgeConfig, BridgeMode};

        // Use an absolute path so resolve_beside_db returns it unchanged.
        let dir = fixture_dir();
        let snapshot_path = dir.path().to_str().expect("utf8 path").to_owned();

        let make_bridge = |bits: u32| BridgeConfig {
            enabled: true,
            mode: BridgeMode::Snapshot,
            snapshot_dir: Some(snapshot_path.clone()),
            snapshot_service_id: Some(9),
            max_query_bits: bits,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
            ptr_url: String::new(),
            ptr_key: String::new(),
            state_db: "bridge-state.db".into(),
        };

        // A configured value of 4 (below SNAPSHOT_MIN_QUERY_BITS=8) must be raised.
        let cfg4 = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(4),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg4.max_query_bits, SNAPSHOT_MIN_QUERY_BITS,
            "sub-floor ceiling (4) must be raised to SNAPSHOT_MIN_QUERY_BITS (8)"
        );

        // Zero is also raised.
        let cfg0 = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(0),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg0.max_query_bits, SNAPSHOT_MIN_QUERY_BITS,
            "zero ceiling must also be raised to SNAPSHOT_MIN_QUERY_BITS (8)"
        );

        // A value exactly at the floor is kept as-is.
        let cfg8 = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(SNAPSHOT_MIN_QUERY_BITS),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg8.max_query_bits, SNAPSHOT_MIN_QUERY_BITS,
            "a ceiling exactly at the floor must not be changed"
        );

        // A value above the floor is kept as-is.
        let cfg32 = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(32),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg32.max_query_bits, 32,
            "a ceiling above the floor must not be changed"
        );
    }

    /// Decision table for `min_query_bits` clamping:
    ///   f < 8          → 8  (warn: below hard minimum)
    ///   8 ≤ f ≤ max   → f  (no warn)
    ///   f > max        → max (warn: floor above ceiling)
    #[test]
    fn from_settings_clamps_min_query_bits_into_floor_ceiling() {
        use crate::settings::{BridgeConfig, BridgeMode};

        let dir = fixture_dir();
        let snapshot_path = dir.path().to_str().expect("utf8 path").to_owned();

        let make_bridge = |max: u32, min: u32| BridgeConfig {
            enabled: true,
            mode: BridgeMode::Snapshot,
            snapshot_dir: Some(snapshot_path.clone()),
            snapshot_service_id: Some(9),
            max_query_bits: max,
            min_query_bits: min,
            ptr_url: String::new(),
            ptr_key: String::new(),
            state_db: "bridge-state.db".into(),
        };

        // Row 1: f = 4 (< 8) → effective floor raised to 8.
        let cfg_below = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(32, 4),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg_below.min_query_bits, SNAPSHOT_MIN_QUERY_BITS,
            "f=4 below hard minimum must be raised to 8"
        );

        // Row 2: f = 16, max = 32 (8 ≤ f ≤ max) → floor kept as-is.
        let cfg_in = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(32, 16),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg_in.min_query_bits, 16,
            "f=16 within [8, 32] must be kept unchanged"
        );

        // Row 3: f = 40, max = 32 (f > max) → floor lowered to ceiling.
        let cfg_above = DomainConfig::from_settings(
            HashDomain::Blake3,
            &make_bridge(32, 40),
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("snapshot opens");
        assert_eq!(
            cfg_above.min_query_bits, 32,
            "f=40 above ceiling (32) must be lowered to match the ceiling"
        );
    }

    #[test]
    fn bucket_deduplicates_repeated_raw_rows() {
        // write_snapshot inserts two mapping rows when the same (hash, tag) pair
        // appears twice; bucket() must collapse them to one entry via .dedup().
        // This test fails if .dedup() is removed from bucket().
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(
            dir.path(),
            9,
            &[
                (SHA_A, "maid"),
                (SHA_A, "maid"), // second entry → duplicate mapping row in the DB
            ],
        )
        .expect("write snapshot with duplicate mapping rows");
        let backend = SnapshotBackend::open(dir.path(), Some(9)).expect("open backend");
        let hits = backend.bucket(SHA_A, 256, usize::MAX).expect("bucket").0;
        let tags = hits.get(SHA_A).expect("SHA_A present");
        assert_eq!(
            tags,
            &vec!["maid".to_string()],
            "two identical raw rows must be deduped to one tag"
        );
    }

    #[test]
    fn bucket_sorts_tags_ascending() {
        // Insert tags in reverse canonical order; bucket() must return them
        // ascending. This test fails if .sort() is removed from bucket().
        let dir = tempfile::tempdir().expect("tempdir");
        naiad_plugin_hydrus::fixture::write_snapshot(
            dir.path(),
            9,
            &[
                (SHA_A, "series:metroid"), // 's' > 'c': descending order
                (SHA_A, "character:samus"),
            ],
        )
        .expect("write snapshot with reversed tag order");
        let backend = SnapshotBackend::open(dir.path(), Some(9)).expect("open backend");
        let hits = backend.bucket(SHA_A, 256, usize::MAX).expect("bucket").0;
        let tags = hits.get(SHA_A).expect("SHA_A present");
        assert_eq!(
            tags,
            &vec!["character:samus".to_string(), "series:metroid".to_string()],
            "tags must be sorted ascending regardless of insertion order"
        );
    }

    #[test]
    fn served_does_not_duplicate_sha256_when_native_is_sha256_and_snapshot_present() {
        // Even when a snapshot backend is attached, a node whose native domain
        // is already sha256 must advertise it exactly once — not twice.
        // NOTE: this constructs DomainConfig directly, bypassing from_settings.
        // from_settings now rejects this combination (#149); served()'s dedup
        // logic still belongs here as a guard.
        let dir = fixture_dir();
        let backend = SnapshotBackend::open(dir.path(), Some(9)).expect("open backend");
        let dual_sha = DomainConfig {
            native: HashDomain::Sha256,
            added_sha256: Some(std::sync::Arc::new(backend) as Arc<dyn Sha256Backend>),
            max_query_bits: 256,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
        };
        assert_eq!(
            dual_sha.served(),
            vec![HashDomain::Sha256],
            "snapshot backend present but native is sha256: must not produce a duplicate sha256 entry"
        );
    }

    #[test]
    fn sidecar_backend_serves_a_bucket_and_from_settings_opens_it() {
        use crate::bridge::sidecar::Sidecar;
        use crate::bridge::sidecar_seed;
        use crate::settings::{BridgeConfig, BridgeMode};

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sc = Sidecar::create(&sc_path).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
        drop(sc);

        let bridge = BridgeConfig {
            enabled: true,
            mode: BridgeMode::Sidecar,
            snapshot_dir: None,
            snapshot_service_id: None,
            max_query_bits: 32,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
            ptr_url: String::new(),
            ptr_key: String::new(),
            state_db: sc_path.to_str().unwrap().to_string(),
        };
        // db_path is only used to resolve a relative state_db; state_db here is absolute.
        let cfg = DomainConfig::from_settings(
            HashDomain::Blake3,
            &bridge,
            dir.path().join("repo.db").as_path(),
            4,
        )
        .unwrap();
        assert_eq!(cfg.served(), vec![HashDomain::Blake3, HashDomain::Sha256]);
        let backend = cfg.added_sha256.as_ref().expect("sidecar backend present");
        let h1 = format!("11{}", "00".repeat(31));
        let (hits, _) = backend.bucket(&h1, 256, usize::MAX).unwrap();
        assert_eq!(
            hits.get(&h1),
            Some(&vec!["character:samus".to_string(), "maid".to_string()])
        );
    }

    /// #208: pool_size=0 is clamped to 1 at open time.
    #[test]
    fn sidecar_backend_pool_clamp_zero_to_one() {
        use crate::bridge::sidecar::Sidecar;
        use crate::bridge::sidecar_seed;

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sc = Sidecar::create(&sc_path).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
        drop(sc);

        let b0 = SidecarBackend::open(&sc_path, 0).unwrap();
        assert_eq!(b0.pool_len(), 1, "pool_size 0 must be clamped to 1");
    }

    /// #208: a 4-slot pool cycles through all four slots in order; the 5th
    /// call wraps to slot 0 (round-robin). Mirrors the `ReadPool` test idiom
    /// in `http.rs` to prove scheduling, not just correctness.
    #[test]
    fn sidecar_backend_pool_round_robin_ordering() {
        use crate::bridge::sidecar::Sidecar;
        use crate::bridge::sidecar_seed;

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sc = Sidecar::create(&sc_path).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
        drop(sc);

        let b = SidecarBackend::open(&sc_path, 4).unwrap();
        assert_eq!(b.pool_len(), 4);

        // 5 consecutive advance_cursor() calls on a fresh (cursor=0) 4-slot pool
        // must yield [0, 1, 2, 3, 0]: all four slots distinct, then wrap.
        let slots: Vec<usize> = (0..5).map(|_| b.advance_cursor()).collect();
        assert_eq!(
            slots,
            vec![0, 1, 2, 3, 0],
            "4-slot pool must cycle 0→1→2→3→0; got {slots:?}"
        );
    }

    /// #208: concurrent bucket queries through the pooled backend must not
    /// serialise on a single lock and must all return the correct result.
    #[test]
    fn sidecar_backend_pool_concurrent_queries() {
        use crate::bridge::sidecar::Sidecar;
        use crate::bridge::sidecar_seed;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sc = Sidecar::create(&sc_path).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
        drop(sc);

        // Pool of 4 connections; queries run from 8 threads simultaneously.
        let backend: Arc<dyn Sha256Backend> = Arc::new(SidecarBackend::open(&sc_path, 4).unwrap());
        let h1 = format!("11{}", "00".repeat(31));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let b = Arc::clone(&backend);
            let key = h1.clone();
            handles.push(std::thread::spawn(move || b.bucket(&key, 256, usize::MAX)));
        }
        for handle in handles {
            let (hits, _) = handle.join().expect("thread panicked").unwrap();
            assert_eq!(
                hits.get(&h1),
                Some(&vec!["character:samus".to_string(), "maid".to_string()]),
                "each thread must get the correct result from the pool"
            );
        }
    }

    /// Regression for #149: `from_settings` must reject `hash_domain = "sha256"`
    /// combined with `mode = "snapshot"` at startup, not let it silently serve
    /// empty results. The error must name both conflicting settings.
    #[test]
    fn from_settings_rejects_sha256_native_with_snapshot_mode() {
        use crate::settings::{BridgeConfig, BridgeMode};

        let bridge = BridgeConfig {
            enabled: true,
            mode: BridgeMode::Snapshot,
            // snapshot_dir must be set so the check fires before the
            // missing-dir guard (we want the new check, not the old one).
            snapshot_dir: Some("/some/nonexistent/path".into()),
            snapshot_service_id: None,
            max_query_bits: 256,
            min_query_bits: SNAPSHOT_MIN_QUERY_BITS,
            ptr_url: String::new(),
            ptr_key: String::new(),
            state_db: "bridge-state.db".into(),
        };
        let err = DomainConfig::from_settings(
            HashDomain::Sha256,
            &bridge,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect_err("sha256 native + snapshot mode must fail fast at startup");
        let msg = format!("{err:#}");
        assert!(msg.contains("sha256"), "error must mention sha256: {msg}");
        assert!(
            msg.contains("snapshot"),
            "error must mention snapshot: {msg}"
        );
        assert!(
            msg.contains("hash_domain") || msg.contains("native"),
            "error must explain the incompatibility: {msg}"
        );

        // Blake3 native + snapshot is fine (the standard snapshot setup).
        // We only verify that from_settings does NOT return this new error;
        // it may return the missing-dir error for a nonexistent path, which is
        // expected and unrelated.
        let blake3_err = DomainConfig::from_settings(
            HashDomain::Blake3,
            &bridge,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect_err("nonexistent snapshot dir still fails");
        let blake3_msg = format!("{blake3_err:#}");
        // The error must NOT be the sha256-incompatibility error — it must be
        // the missing-snapshot-dir error (or open failure).
        assert!(
            !blake3_msg.contains("hash_domain = \"sha256\""),
            "blake3 native + snapshot must not trigger the sha256-incompatibility error: {blake3_msg}"
        );

        // Disabled bridge: the check does not fire regardless of native domain.
        let off = BridgeConfig {
            enabled: false,
            ..bridge
        };
        DomainConfig::from_settings(
            HashDomain::Sha256,
            &off,
            std::path::Path::new("/srv/naiad/repo.db"),
            1,
        )
        .expect("disabled bridge + sha256 native is fine: no bridge, no conflict");
    }

    /// `SidecarBackend::open` must apply `apply_read_only_serve_pragmas` to
    /// every pooled connection: verify `query_only` and `mmap_size` on slot 0.
    #[test]
    fn serve_pool_connections_are_tuned() {
        use crate::bridge::sidecar::{SIDECAR_SERVE_MMAP_SIZE, Sidecar};
        use crate::bridge::sidecar_seed;

        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc_path = dir.path().join("sidecar.db");
        let sc = Sidecar::create(&sc_path).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
        drop(sc);

        let backend = SidecarBackend::open(&sc_path, 3).unwrap();

        assert_eq!(
            backend.pool_pragma(0, "query_only"),
            1,
            "slot 0: query_only must be ON (1) after open"
        );
        assert_eq!(
            backend.pool_pragma(0, "mmap_size"),
            SIDECAR_SERVE_MMAP_SIZE,
            "slot 0: mmap_size must match SIDECAR_SERVE_MMAP_SIZE after open"
        );
    }
}
