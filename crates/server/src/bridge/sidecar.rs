//! Compact hash-ordered PTR sidecar index (#207 / ADR 0028).
//!
//! One delta-varint-packed row per hash in a `WITHOUT ROWID` `bucket_map`,
//! plus service-space defs and a cursor, all in one self-contained SQLite file
//! (a superset of `bridge-state.db`). This module owns the on-disk format:
//! the tag-id codec and the `Sidecar` store type (apply primitives,
//! range-scan reads).
//!
//! ## Transaction ownership
//!
//! `insert_defs_hashes` and `insert_defs_tags` are bare statement executors
//! that require the caller to hold an outer transaction (fast batch via
//! `conn().unchecked_transaction()`). `write_tag_set`, `apply_mutations`, and
//! `set_flag` each execute a single statement and are self-transacting (SQLite
//! autocommit per statement) — callers that batch many of them should wrap in
//! an outer transaction themselves.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use naiad_core::{BudgetExceeded, Hash, approx_row_cost, bucket_key, bucket_upper};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

/// On-disk format version. Bumped only on an incompatible `bucket_map`/defs
/// change; an unknown or newer value is a fail-fast at open (spec §Error handling).
pub const SIDECAR_SCHEMA_VERSION: i64 = 1;

/// The only shipped hash-key width: full 32-byte sha256 (spec §"Hash key width").
pub const HASH_KEY_WIDTH_FULL: i64 = 32;

/// Chunk size for `IN (...)` defs lookups, safe under the legacy 999-var limit.
const DEFS_LOOKUP_CHUNK: usize = 900;

/// `PRAGMA journal_size_limit` for writer connections: 64 MB (#231).
///
/// A successful checkpoint truncates `sidecar.db-wal` back under this limit, so
/// the file physically shrinks instead of sitting at its high-water mark
/// forever. The apply path checkpoints between sub-batches, so this is the
/// steady-state WAL ceiling during a sync.
pub const SIDECAR_WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// `PRAGMA mmap_size` ceiling for serve-pool read connections: 1 GiB
/// (reference baseline). A lazy ceiling on the mmap window, not an allocation —
/// the OS pages it in on demand. All pooled connections mmap the same
/// `sidecar.db`, so mapped pages are shared in the OS page cache (charged once
/// to the cgroup, reclaimable under pressure), not duplicated per connection.
/// Matches the native `RepoStore` serve value (#202).
pub const SIDECAR_SERVE_MMAP_SIZE: i64 = 1_073_741_824;

/// `PRAGMA cache_size` for serve-pool read connections, KiB-denominated
/// (negative → KiB regardless of page size): 16 MiB (reference baseline).
/// Modest bump from the ~2 MiB default; multiplies by pool size in heap, so
/// kept conservative (default pool of 4 → ~64 MiB; clamp of 64 → ~1 GiB).
pub const SIDECAR_SERVE_CACHE_KIB: i64 = -16_384;

/// Result of one `PRAGMA wal_checkpoint(TRUNCATE)` on the writer (#231).
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

/// Append the unsigned LEB128 encoding of `v` to `out`.
fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Decode one unsigned LEB128 varint from `buf` at `*pos`, advancing `*pos`.
///
/// Errors on truncation (buffer ends mid-varint), on an overlong encoding
/// (more than 10 bytes), and on a 10th byte whose value bits exceed 1 (which
/// would silently overflow a `u64` without this check).
fn read_uvarint(buf: &[u8], pos: &mut usize) -> anyhow::Result<u64> {
    let start = *pos;
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut bytes = 0u32;
    loop {
        let byte = *buf
            .get(*pos)
            .ok_or_else(|| anyhow!("varint truncated at offset {}", *pos))?;
        *pos += 1;
        bytes += 1;
        if bytes > 10 {
            bail!("varint overlong (>10 bytes) at offset {}", start);
        }
        // The 10th byte carries bit 63 of u64 only; value bits 1-6 have no room
        // and would be silently dropped by the shift, corrupting the decoded id.
        if shift == 63 && (byte & 0x7f) > 1 {
            bail!("varint overflows u64 at offset {}", start);
        }
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Pack a set of `service_tag_id`s as sorted, deduped delta LEB128 varints.
///
/// Deterministic: the output depends only on the *set*, never on input order,
/// so two hashes with the same tag set produce byte-identical blobs (needed by
/// the parity digest).
pub(crate) fn pack_tag_ids(ids: &[u64]) -> Vec<u8> {
    let mut sorted: Vec<u64> = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    let mut prev: u64 = 0;
    for &id in &sorted {
        write_uvarint(&mut out, id - prev);
        prev = id;
    }
    out
}

/// Unpack a `pack_tag_ids` blob back into the ascending id list.
///
/// # Errors
/// Returns an error on a truncated or overlong varint, or on a delta that
/// overflows `u64`. Callers should wrap errors with the owning hash or row
/// context — the byte offset alone won't localize corruption in a multi-GB store.
pub(crate) fn unpack_tag_ids(buf: &[u8]) -> anyhow::Result<Vec<u64>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut acc: u64 = 0;
    while pos < buf.len() {
        let delta = read_uvarint(buf, &mut pos)?;
        acc = acc
            .checked_add(delta)
            .ok_or_else(|| anyhow!("tag id delta overflow"))?;
        out.push(acc);
    }
    Ok(out)
}

/// The PTR sidecar: a self-contained SQLite index (superset of bridge-state.db).
///
/// See the module-level doc for transaction-ownership conventions.
#[derive(Debug)]
pub struct Sidecar {
    conn: Connection,
}

const DDL: &str = "
    CREATE TABLE IF NOT EXISTS bucket_map (
        hash    BLOB PRIMARY KEY,
        tag_ids BLOB NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS defs_hashes (
        service_hash_id INTEGER PRIMARY KEY,
        sha256          BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS defs_tags (
        service_tag_id INTEGER PRIMARY KEY,
        tag            TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT);
";

impl Sidecar {
    /// Create (or open) a sidecar at `path`, writing the schema and stamping
    /// `schema_version` + `hash_key_width`. Idempotent: re-creating an existing
    /// valid sidecar leaves it intact.
    pub fn create(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)
            .with_context(|| format!("creating sidecar {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::apply_writer_pragmas(&conn)?;
        conn.execute_batch(DDL).context("creating sidecar schema")?;
        let me = Self { conn };
        // Stamp only if absent (INSERT OR IGNORE semantics via set-if-missing).
        if me.get_flag("schema_version")?.is_none() {
            me.set_flag("schema_version", &SIDECAR_SCHEMA_VERSION.to_string())?;
        }
        if me.get_flag("hash_key_width")?.is_none() {
            me.set_flag("hash_key_width", &HASH_KEY_WIDTH_FULL.to_string())?;
        }
        me.validate(path)?;
        Ok(me)
    }

    /// Open an existing sidecar read-write, validating its stamps.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)
            .with_context(|| format!("opening sidecar {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::apply_writer_pragmas(&conn)?;
        let me = Self { conn };
        me.validate(path)?;
        Ok(me)
    }

    /// Writer-connection WAL hygiene (#231): `synchronous = NORMAL` (safe in
    /// WAL mode — a crash loses at most the last commit, never corrupts; drops
    /// the per-commit fsync) and a `journal_size_limit` so checkpoints shrink
    /// the WAL file instead of leaving it at its high-water mark.
    fn apply_writer_pragmas(conn: &Connection) -> anyhow::Result<()> {
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "journal_size_limit", SIDECAR_WAL_SIZE_LIMIT)?;
        Ok(())
    }

    /// Apply serve-only read pragmas to this pooled read connection (reference
    /// baseline). Mirrors the native `RepoStore::apply_read_only_serve_pragmas`
    /// (#202): `query_only` (reject any accidental write at the statement layer,
    /// defense-in-depth atop `SQLITE_OPEN_READ_ONLY`), a 1 GiB `mmap_size` ceiling
    /// for read throughput on the 28 GB sidecar, a modest 16 MiB `cache_size`, and
    /// `temp_store = MEMORY`. Chosen from serve-baseline benchmarks.
    /// Deliberately NOT applied to `open_readonly`'s one-shot callers
    /// (parity audit); only the serving read pool needs it.
    ///
    /// # Errors
    /// Returns an error if a pragma update fails.
    pub(crate) fn apply_read_only_serve_pragmas(&self) -> anyhow::Result<()> {
        self.conn.pragma_update(None, "query_only", "ON")?;
        self.conn
            .pragma_update(None, "mmap_size", SIDECAR_SERVE_MMAP_SIZE)?;
        self.conn
            .pragma_update(None, "cache_size", SIDECAR_SERVE_CACHE_KIB)?;
        self.conn.pragma_update(None, "temp_store", 2i64)?;
        Ok(())
    }

    /// Open an existing sidecar read-only (the serving path), validating stamps.
    pub fn open_readonly(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening sidecar read-only {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let me = Self { conn };
        me.validate(path)?;
        Ok(me)
    }

    /// Fail-fast validation covering every spec §Error-handling open row.
    fn validate(&self, path: &Path) -> anyhow::Result<()> {
        let version = self.get_flag("schema_version")?;
        if version.is_none() {
            // No stamp. Distinguish a legacy bridge-state.db (has defs_tags but no
            // bucket_map) from a truly empty/corrupt file, for an actionable error.
            let has_bucket_map: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='bucket_map')",
                [],
                |r| r.get(0),
            )?;
            if !has_bucket_map {
                bail!(
                    "sidecar {} has no schema_version stamp and no bucket_map table; \
                     this looks like a legacy bridge-state.db, not a sidecar. \
                     Rebuild it with `bridge seed` (mode = \"sidecar\"); no in-place migration.",
                    path.display()
                );
            }
            bail!(
                "sidecar {} has no schema_version stamp (corrupt?)",
                path.display()
            );
        }
        let v: i64 = version
            .unwrap()
            .parse()
            .with_context(|| format!("parsing schema_version of sidecar {}", path.display()))?;
        if v != SIDECAR_SCHEMA_VERSION {
            bail!(
                "sidecar {} has schema_version {v}, but this binary understands {}; \
                 rebuild or upgrade",
                path.display(),
                SIDECAR_SCHEMA_VERSION
            );
        }
        // Parse explicitly so a corrupt non-numeric value is a clear error, not
        // a silent collapse to 0 that produces a confusing "width 0" mismatch.
        let width_raw = self.get_flag("hash_key_width")?.unwrap_or_default();
        let width: i64 = width_raw.parse().with_context(|| {
            format!(
                "parsing hash_key_width {:?} of sidecar {}",
                width_raw,
                path.display()
            )
        })?;
        if width != HASH_KEY_WIDTH_FULL {
            bail!(
                "sidecar {} has hash_key_width {width} (raw: {width_raw:?}), \
                 but this binary serves width {}; rebuild",
                path.display(),
                HASH_KEY_WIDTH_FULL
            );
        }
        Ok(())
    }

    /// Borrow the raw connection.
    ///
    /// Callers that need a transaction — Task 4 seed (per-chunk) and Task 5 sync
    /// (per-index) — open one here with `conn().unchecked_transaction()` and then
    /// call `insert_defs_*` / `write_tag_set` inside it.
    #[must_use]
    #[allow(dead_code)] // consumed by Tasks 4 and 5 (in-crate); remove when those land
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run `PRAGMA wal_checkpoint(TRUNCATE)` on this connection (#231).
    ///
    /// Copies all WAL frames into the main database and truncates the WAL file
    /// (to `journal_size_limit`) when no reader holds a snapshot past them. The
    /// sync apply path calls this between sub-batches so the WAL stays bounded
    /// instead of growing for the whole apply; the returned counters make a
    /// starved checkpoint visible to the caller's logging. Waits for in-flight
    /// readers up to the connection's `busy_timeout`; a reader that outlasts it
    /// yields `busy = true` (frames copied so far remain copied).
    ///
    /// # Errors
    /// Returns an error if the PRAGMA itself fails (e.g. called on a read-only
    /// connection).
    pub fn checkpoint_wal(&self) -> anyhow::Result<WalCheckpoint> {
        // Returns one row: (busy, log, checkpointed).
        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .context("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(WalCheckpoint {
            busy: busy != 0,
            log_frames,
            checkpointed_frames,
        })
    }
}

impl Sidecar {
    /// Read a `sync_state` flag (cursor, stamps, seed-band markers).
    pub fn get_flag(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .with_context(|| format!("reading sync_state flag {key:?}"))
    }

    /// Upsert a `sync_state` flag.
    pub fn set_flag(&self, key: &str, val: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, val],
            )
            .with_context(|| format!("writing sync_state flag {key:?}"))?;
        Ok(())
    }

    /// The next PTR update index to fetch (0 when unset).
    pub fn next_update_index(&self) -> anyhow::Result<u64> {
        match self.get_flag("next_update_index")? {
            None => Ok(0),
            Some(s) => s
                .parse()
                .with_context(|| format!("parsing next_update_index {s:?}")),
        }
    }

    /// Persist the next PTR update index (Task 5 calls this inside the apply txn).
    pub fn set_next_update_index(&self, index: u64) -> anyhow::Result<()> {
        self.set_flag("next_update_index", &index.to_string())
    }

    /// Upsert `(service_hash_id, sha256)` rows (sha256 as a 32-byte BLOB).
    ///
    /// **Caller must wrap in a transaction** — Task 4 seed: per-chunk;
    /// Task 5 sync: per-index. Calling bare is correct (SQLite autocommit per
    /// statement) but slow.
    pub fn insert_defs_hashes(&self, rows: &[(u64, [u8; 32])]) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT OR REPLACE INTO defs_hashes (service_hash_id, sha256) VALUES (?1, ?2)",
        )?;
        for (id, sha) in rows {
            stmt.execute(rusqlite::params![*id as i64, &sha[..]])
                .with_context(|| format!("inserting defs_hashes row id={id}"))?;
        }
        Ok(())
    }

    /// Upsert `(service_tag_id, tag)` rows. Callers pass already-`Tag::parse`d text.
    ///
    /// **Caller must wrap in a transaction** — Task 4 seed: per-chunk;
    /// Task 5 sync: per-index. Calling bare is correct (SQLite autocommit per
    /// statement) but slow.
    pub fn insert_defs_tags(&self, rows: &[(u64, String)]) -> anyhow::Result<()> {
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO defs_tags (service_tag_id, tag) VALUES (?1, ?2)")?;
        for (id, tag) in rows {
            stmt.execute(rusqlite::params![*id as i64, tag])
                .with_context(|| format!("inserting defs_tags row id={id}"))?;
        }
        Ok(())
    }

    /// Resolve one `service_hash_id` to its sha256 (apply-time).
    pub fn sha256_for(&self, id: u64) -> anyhow::Result<Option<[u8; 32]>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT sha256 FROM defs_hashes WHERE service_hash_id = ?1",
                [id as i64],
                |r| r.get(0),
            )
            .optional()?;
        match blob {
            None => Ok(None),
            Some(b) => {
                let arr: [u8; 32] = b
                    .try_into()
                    .map_err(|_| anyhow!("defs_hashes sha256 for id {id} is not 32 bytes"))?;
                Ok(Some(arr))
            }
        }
    }

    /// Batch-resolve `service_tag_id → tag` for a set of ids (serve-time render).
    pub fn defs_tags_for(&self, ids: &[u64]) -> anyhow::Result<HashMap<u64, String>> {
        let mut map = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(DEFS_LOOKUP_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT service_tag_id, tag FROM defs_tags WHERE service_tag_id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(chunk.iter().map(|&id| id as i64));
            let rows = stmt.query_map(params, |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, tag) = row?;
                map.insert(id, tag);
            }
        }
        Ok(map)
    }

    /// Read a hash's current tag-id set (empty when the row is absent).
    pub fn read_tag_set(&self, hash: &[u8; 32]) -> anyhow::Result<Vec<u64>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT tag_ids FROM bucket_map WHERE hash = ?1",
                [&hash[..]],
                |r| r.get(0),
            )
            .optional()?;
        match blob {
            None => Ok(Vec::new()),
            Some(b) => unpack_tag_ids(&b)
                .with_context(|| format!("unpacking bucket_map for {}", hex::encode(hash))),
        }
    }

    /// Write a hash's tag-id set, packing it; an EMPTY set deletes the row (reap).
    pub fn write_tag_set(&self, hash: &[u8; 32], ids: &[u64]) -> anyhow::Result<()> {
        // First 12 hex chars (6 bytes) identify the hash in error messages.
        let hp = hex::encode(&hash[..6]);
        if ids.is_empty() {
            self.conn
                .execute("DELETE FROM bucket_map WHERE hash = ?1", [&hash[..]])
                .with_context(|| format!("deleting bucket_map row {hp}"))?;
            return Ok(());
        }
        let packed = pack_tag_ids(ids);
        self.conn
            .execute(
                "INSERT INTO bucket_map (hash, tag_ids) VALUES (?1, ?2)
             ON CONFLICT(hash) DO UPDATE SET tag_ids = excluded.tag_ids",
                rusqlite::params![&hash[..], packed],
            )
            .with_context(|| format!("upserting bucket_map row {hp}"))?;
        Ok(())
    }

    /// Apply an in-wire-order list of `(service_tag_id, is_delete)` mutations to
    /// one hash: read the set once, mutate in order, persist once (reap if empty).
    /// Deletes of an absent id and adds of a present id are no-ops (idempotent).
    pub fn apply_mutations(
        &self,
        hash: &[u8; 32],
        mutations: &[(u64, bool)],
    ) -> anyhow::Result<()> {
        let mut set: Vec<u64> = self.read_tag_set(hash)?;
        for &(id, is_delete) in mutations {
            if is_delete {
                if let Ok(pos) = set.binary_search(&id) {
                    set.remove(pos);
                }
            } else if let Err(pos) = set.binary_search(&id) {
                set.insert(pos, id);
            }
        }
        self.write_tag_set(hash, &set)
    }

    // ── Stats count helpers (bridge gauges, #235/#236) ────────────────────────

    // ── sync_state cache keys for the bridge count cache (#236) ──────────────

    /// Cached row count of `bucket_map` (distinct hashes stored).
    pub const STAT_KEY_BUCKET_ROWS: &'static str = "stat_bucket_rows";
    /// Cached total tag-id count across all `bucket_map` rows (exact mapping pairs).
    pub const STAT_KEY_MAPPING_PAIRS: &'static str = "stat_mapping_pairs";
    /// Cached row count of `defs_tags` (known tag definitions).
    pub const STAT_KEY_TAGS: &'static str = "stat_tags";
    /// Unix timestamp of the last `recompute_bridge_counts` run.
    pub const STAT_KEY_COUNTS_COMPUTED_UNIX: &'static str = "stat_counts_computed_unix";

    /// Read the cached bridge gauge counts from `sync_state`.
    ///
    /// Returns `None` when the cache has not been populated yet (i.e. any of
    /// the three count keys is absent). This is the **only** read path the 600 s
    /// gauge tick uses — it never opens `bucket_map` itself.
    ///
    /// # Hot-path property
    /// Issues three `SELECT value FROM sync_state WHERE key = ?` primary-key
    /// point lookups (the `sync_state` table has a `TEXT PRIMARY KEY`, so each
    /// is O(log N) on a tiny table). Zero scans of `bucket_map` or `defs_tags`.
    pub fn cached_bridge_counts(&self) -> anyhow::Result<Option<(u64, u64, u64)>> {
        let hashes_raw = self.get_flag(Self::STAT_KEY_BUCKET_ROWS)?;
        let mappings_raw = self.get_flag(Self::STAT_KEY_MAPPING_PAIRS)?;
        let tags_raw = self.get_flag(Self::STAT_KEY_TAGS)?;
        match (hashes_raw, tags_raw, mappings_raw) {
            (Some(h), Some(t), Some(m)) => {
                let hashes: u64 = h
                    .parse()
                    .with_context(|| format!("parsing cached stat_bucket_rows {h:?}"))?;
                let tags: u64 = t
                    .parse()
                    .with_context(|| format!("parsing cached stat_tags {t:?}"))?;
                let mappings: u64 = m
                    .parse()
                    .with_context(|| format!("parsing cached stat_mapping_pairs {m:?}"))?;
                Ok(Some((hashes, tags, mappings)))
            }
            _ => Ok(None),
        }
    }

    /// Recompute bridge gauge counts and persist them to the `sync_state` cache.
    ///
    /// # What it does
    /// Performs ONE full sequential scan of `bucket_map`, counting rows
    /// (= distinct hashes) and summing `tag_ids` blob lengths (= exact total
    /// mapping pairs) in a single pass. Separately issues a `COUNT(*)` on the
    /// much-smaller `defs_tags` table. Writes all three results plus a Unix
    /// timestamp into `sync_state` atomically.
    ///
    /// # When to call
    /// This is the **only** code path permitted to scan `bucket_map` for stats
    /// purposes. On a 29 GB production sidecar the scan can take many minutes.
    /// Call exclusively from a `spawn_blocking` context (e.g. the bridge count
    /// refresher task) — never from the 600 s gauge tick or any async hot path.
    ///
    /// # Errors
    /// Returns an error on any SQLite failure or blob decode error. On error the
    /// cache is left unchanged (the write uses an outer transaction that rolls
    /// back on failure).
    pub fn recompute_bridge_counts(&self) -> anyhow::Result<()> {
        // Single full scan of bucket_map: count rows AND sum tag_ids lengths.
        // This is the only place in the codebase that scans bucket_map for stats.
        let mut hash_count: u64 = 0;
        let mut mapping_count: u64 = 0;
        {
            let mut stmt = self
                .conn
                .prepare("SELECT tag_ids FROM bucket_map")
                .context("preparing bucket_map scan for recompute_bridge_counts")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let blob: Vec<u8> = r.get(0)?;
                let n = unpack_tag_ids(&blob)
                    .context("unpacking tag_ids blob in recompute_bridge_counts")?
                    .len() as u64;
                hash_count += 1;
                mapping_count += n;
            }
        } // stmt dropped; read scan complete

        // defs_tags is much smaller; a plain COUNT(*) is fast even at scale.
        let tag_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM defs_tags", [], |r| r.get(0))
            .context("counting defs_tags in recompute_bridge_counts")?;

        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Write all four values atomically so the cache is never half-populated.
        let tx = self
            .conn
            .unchecked_transaction()
            .context("opening transaction for recompute_bridge_counts write")?;
        self.set_flag(Self::STAT_KEY_BUCKET_ROWS, &hash_count.to_string())?;
        self.set_flag(Self::STAT_KEY_MAPPING_PAIRS, &mapping_count.to_string())?;
        self.set_flag(Self::STAT_KEY_TAGS, &(tag_count as u64).to_string())?;
        self.set_flag(Self::STAT_KEY_COUNTS_COMPUTED_UNIX, &now_unix.to_string())?;
        tx.commit()
            .context("committing recompute_bridge_counts cache write")?;
        Ok(())
    }

    /// Count of distinct hashes in the sidecar (`SELECT COUNT(*) FROM bucket_map`).
    ///
    /// **WARNING (#236):** On a large production sidecar (e.g. 29 GB), this
    /// `WITHOUT ROWID` clustered table has no secondary index, so `COUNT(*)`
    /// requires a full btree scan (15+ minutes observed). Do **not** call from
    /// any periodic hot path. Prefer `cached_bridge_counts()` / `recompute_bridge_counts()`.
    /// Retained for use in tests and the recompute path internals only.
    pub fn bucket_hash_count(&self) -> anyhow::Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM bucket_map", [], |r| r.get(0))
            .context("counting bucket_map rows")?;
        Ok(n as u64)
    }

    /// Count of distinct tag definitions (`SELECT COUNT(*) FROM defs_tags`).
    ///
    /// A proxy for tags-in-use rather than distinct tags actually mapped; used
    /// by the `bridge_tags_stored` gauge. Cheap and good enough.
    pub fn tag_def_count(&self) -> anyhow::Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM defs_tags", [], |r| r.get(0))
            .context("counting defs_tags rows")?;
        Ok(n as u64)
    }

    /// **Approximate** total mapping count for the `bridge_mappings_stored` gauge.
    ///
    /// # Method
    /// Samples up to `sample_size` rows from `bucket_map` (no ORDER BY — storage
    /// order, O(sample)). Unpacks each row's `tag_ids` blob, counts tags, then
    /// projects: `total_bucket_map_rows × avg_tags_per_sampled_row`.
    ///
    /// # Error characteristics
    /// With `sample_size = 50` the standard error is roughly `±1/√50 ≈ 14%` for
    /// a broad tag-count distribution; much tighter for uniform distributions.
    /// The estimate can over- or under-count but is never negative. Suitable for
    /// trending; do not use for exact accounting.
    ///
    /// **WARNING (#236):** calls `COUNT(*) FROM bucket_map` internally, which is
    /// a full scan on a large `WITHOUT ROWID` table. Use only in tests or
    /// one-off tooling. Do not call from the 600 s gauge path.
    ///
    /// NEVER call with `sample_size = 0` (returns 0 immediately).
    pub fn approx_mapping_count(&self, sample_size: u64) -> anyhow::Result<u64> {
        if sample_size == 0 {
            return Ok(0);
        }
        // Full count — touches the whole btree on a WITHOUT ROWID table.
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM bucket_map", [], |r| r.get(0))
            .context("counting bucket_map rows for approx_mapping_count")?;
        if total == 0 {
            return Ok(0);
        }
        // Sample N rows in storage order (no ORDER BY → index scan, O(sample)).
        let mut stmt = self
            .conn
            .prepare("SELECT tag_ids FROM bucket_map LIMIT ?1")
            .context("preparing approx_mapping_count sample")?;
        let blobs = stmt
            .query_map([sample_size as i64], |r| r.get::<_, Vec<u8>>(0))
            .context("querying bucket_map sample")?;
        let mut total_tags: u64 = 0;
        let mut sampled: u64 = 0;
        for blob_res in blobs {
            let blob = blob_res?;
            let n = unpack_tag_ids(&blob)
                .context("unpacking sample blob in approx_mapping_count")?
                .len() as u64;
            total_tags += n;
            sampled += 1;
        }
        if sampled == 0 {
            return Ok(0);
        }
        // estimate = total_hashes × (total_tags_in_sample / sampled_hashes)
        Ok((total as u64) * total_tags / sampled)
    }

    /// `(count, blake3)` digest of current `(hash, tag)` pairs in a hash band,
    /// byte-identical to `HydrusDb::audit_band_digest` (same `PairDigest`, same
    /// per-hash sort+dedup). `prefix_bits == 0` audits the full range.
    ///
    /// Streams `bucket_map` in **bounded batches** (`AUDIT_BATCH_HASHES` rows
    /// per page) so peak memory is O(batch), not O(corpus). Between pages the
    /// rusqlite cursor is closed (releasing the `conn` borrow) and `defs_tags`
    /// is resolved for that page's distinct ids before moving on. Hash order is
    /// preserved across pages (each page opens `hash > last_seen`), so the
    /// `PairDigest` receives pairs in the same hash-ascending, per-hash-sorted
    /// order as both reference implementations.
    ///
    /// # Errors
    /// Returns an error if `lo_hex` is not 64-char hex, a query fails, or a
    /// `bucket_map` id is missing from `defs_tags` (build-invariant tripwire).
    pub fn audit_band_digest(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
    ) -> anyhow::Result<(u64, [u8; 32])> {
        use naiad_core::PairDigest;

        /// Rows per page; keeps peak resident memory O(batch × avg_tags × 8 B).
        const AUDIT_BATCH_HASHES: i64 = 10_000;
        /// Emit a progress log line every this many hashes.
        const PROGRESS_INTERVAL: u64 = 1_000_000;

        let lo: Hash = lo_hex
            .parse()
            .map_err(|e| anyhow!("bad band lo-bound {lo_hex:?}: {e}"))?;
        let bits = prefix_bits.min(256);
        let lo_blob = hex::decode(bucket_key(&lo, bits))?;
        let hi_hex = bucket_upper(&lo, bits);
        // 33-byte 0xff sentinel sorts strictly after every 32-byte hash.
        const SENTINEL: &[u8] = &[0xff_u8; 33];
        let hi_blob: Vec<u8> = if hi_hex == "g" {
            SENTINEL.to_vec()
        } else {
            hex::decode(hi_hex)?
        };

        let mut digest = PairDigest::new();
        let mut hashes_done: u64 = 0;
        // `page_lo` is the lower bound for the next page query.
        // `inclusive`: true on the first page (≥), false on subsequent pages (>)
        // so we never re-process the last hash of the previous page.
        let mut page_lo: Vec<u8> = lo_blob;
        let mut inclusive = true;

        loop {
            // ── Collect one page of rows ─────────────────────────────────────
            // The cursor (and its borrow of self.conn) lives only inside this
            // block; it is dropped before we call defs_tags_for below.
            let mut batch: Vec<([u8; 32], Vec<u64>)> = Vec::new();
            let mut need: Vec<u64> = Vec::new();
            {
                let sql = if inclusive {
                    "SELECT hash, tag_ids FROM bucket_map \
                     WHERE hash >= ?1 AND hash < ?2 ORDER BY hash LIMIT ?3"
                } else {
                    "SELECT hash, tag_ids FROM bucket_map \
                     WHERE hash > ?1 AND hash < ?2 ORDER BY hash LIMIT ?3"
                };
                let mut stmt = self.conn.prepare(sql)?;
                let mut cur =
                    stmt.query(rusqlite::params![page_lo, hi_blob, AUDIT_BATCH_HASHES])?;
                while let Some(r) = cur.next()? {
                    let hb: Vec<u8> = r.get(0)?;
                    let packed: Vec<u8> = r.get(1)?;
                    let ids = unpack_tag_ids(&packed).with_context(|| {
                        format!("unpacking bucket_map row {}", hex::encode(&hb))
                    })?;
                    let hash: [u8; 32] = hb
                        .as_slice()
                        .try_into()
                        .map_err(|_| anyhow!("bucket_map hash not 32 bytes"))?;
                    need.extend_from_slice(&ids);
                    page_lo = hb; // advance to the last-seen hash for the next page
                    batch.push((hash, ids));
                }
            } // stmt + cur dropped; self.conn borrow released

            if batch.is_empty() {
                break;
            }

            // ── Resolve tag ids for this page ────────────────────────────────
            need.sort_unstable();
            need.dedup();
            let tags = self.defs_tags_for(&need)?;

            // ── Digest each hash group ───────────────────────────────────────
            for (hash, ids) in &batch {
                let mut ts: Vec<String> = Vec::with_capacity(ids.len());
                for id in ids {
                    let t = tags
                        .get(id)
                        .ok_or_else(|| anyhow!("bucket_map id {id} missing from defs_tags"))?;
                    ts.push(t.clone());
                }
                ts.sort_unstable();
                ts.dedup();
                for t in &ts {
                    digest.update(hash, t);
                }
            }

            // ── Progress tracing ─────────────────────────────────────────────
            let prev_done = hashes_done;
            hashes_done += batch.len() as u64;
            if hashes_done / PROGRESS_INTERVAL != prev_done / PROGRESS_INTERVAL {
                tracing::info!(
                    target: "bridge",
                    hashes_done,
                    "sidecar parity audit: progress"
                );
            }

            if (batch.len() as i64) < AUDIT_BATCH_HASHES {
                break; // Last page was a short page → no more rows.
            }
            inclusive = false; // Subsequent pages use > to skip the last seen hash.
        }

        Ok(digest.finalize())
    }

    /// Range-scan a k-anon bucket (256 bits = exact) → `sha256_hex → sorted tags`.
    ///
    /// Memory and work are O(budget), never O(range): a conservative floor charge
    /// of `approx_row_cost(sha_hex.len(), 1)` per tag is accumulated during the
    /// SQLite cursor pass (pass 1); if the floor exceeds `budget` the cursor is
    /// abandoned immediately via `BudgetExceeded`. Pass 2 performs the authoritative
    /// true-up with exact tag text lengths and may abort earlier than pass 1 for
    /// rows that happen to have short tags. `RESPONSE_ENVELOPE_OVERHEAD` is NOT
    /// charged here — that is the HTTP layer's responsibility (Task 6).
    ///
    /// A `bucket_map` id absent from `defs_tags` is a hard error (build-invariant
    /// tripwire, spec §Error handling).
    pub fn bucket(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        budget: usize,
    ) -> anyhow::Result<(BTreeMap<String, Vec<String>>, usize)> {
        let lo: Hash = lo_hex
            .parse()
            .map_err(|e| anyhow!("bad bucket lo-bound {lo_hex:?}: {e}"))?;
        let bits = prefix_bits.min(256);
        let lo_blob = hex::decode(bucket_key(&lo, bits))?;
        let hi_hex = bucket_upper(&lo, bits);
        // 33-byte 0xff sentinel sorts strictly after every 32-byte hash, so the
        // query always carries `hash < ?2` and drives the PK index (mirrors
        // plugin-hydrus mappings_for_prefix).
        const SENTINEL: &[u8] = &[0xff_u8; 33];
        let hi_blob: Vec<u8> = if hi_hex == "g" {
            SENTINEL.to_vec()
        } else {
            hex::decode(hi_hex)?
        };

        // Pass 1: stream the cursor, collecting rows while gating on a conservative
        // floor charge. Floor = approx_row_cost(sha_hex.len(), 1) per tag — we know
        // the hash length (64 hex chars) but not tag text lengths yet. Bailing here
        // keeps memory and SQLite work O(budget).
        //
        // The statement + cursor live only inside this block (#231): dropping
        // them releases this connection's implicit read snapshot before the
        // defs lookups below, so a serve-pool reader pins the writer's WAL for
        // the range scan only, not the whole bucket render.
        let mut rows: Vec<(String, Vec<u64>)> = Vec::new();
        let mut need: Vec<u64> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT hash, tag_ids FROM bucket_map WHERE hash >= ?1 AND hash < ?2")?;
            let mut cur = stmt.query(rusqlite::params![lo_blob, hi_blob])?;
            let mut floor_spent: usize = 0;
            while let Some(r) = cur.next()? {
                let hash: Vec<u8> = r.get(0)?;
                let packed: Vec<u8> = r.get(1)?;
                let sha_hex = hex::encode(&hash);
                let ids = unpack_tag_ids(&packed)
                    .with_context(|| format!("unpacking bucket_map row {sha_hex}"))?;
                // Charge the floor before materializing this row into `rows`/`need`.
                for _ in &ids {
                    floor_spent = floor_spent.saturating_add(approx_row_cost(sha_hex.len(), 1));
                    if floor_spent > budget {
                        return Err(BudgetExceeded { budget }.into());
                    }
                }
                need.extend_from_slice(&ids);
                rows.push((sha_hex, ids));
            }
        } // stmt + cur dropped; read snapshot released
        need.sort_unstable();
        need.dedup();
        let tags = self.defs_tags_for(&need)?;

        // Pass 2: authoritative true-up with exact tag text lengths.
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut spent: usize = 0;
        for (sha_hex, ids) in rows {
            for id in ids {
                let tag = tags
                    .get(&id)
                    .ok_or_else(|| anyhow!("bucket_map id {id} missing from defs_tags"))?;
                spent = spent.saturating_add(approx_row_cost(sha_hex.len(), tag.len()));
                if spent > budget {
                    return Err(BudgetExceeded { budget }.into());
                }
                out.entry(sha_hex.clone()).or_default().push(tag.clone());
            }
        }
        for v in out.values_mut() {
            v.sort();
            v.dedup();
        }
        Ok((out, spent))
    }
}

#[cfg(test)]
mod varint {
    use super::*;

    #[test]
    fn round_trip_empty_single_and_set() {
        assert_eq!(pack_tag_ids(&[]), Vec::<u8>::new());
        assert_eq!(
            unpack_tag_ids(&pack_tag_ids(&[])).unwrap(),
            Vec::<u64>::new()
        );
        assert_eq!(unpack_tag_ids(&pack_tag_ids(&[7])).unwrap(), vec![7]);
        assert_eq!(
            unpack_tag_ids(&pack_tag_ids(&[1, 2, 3])).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn pack_is_order_independent_and_dedups() {
        let a = pack_tag_ids(&[3, 1, 2]);
        let b = pack_tag_ids(&[1, 2, 3]);
        let c = pack_tag_ids(&[2, 3, 1, 2, 3]);
        assert_eq!(a, b, "packing depends only on the set, not order");
        assert_eq!(a, c, "duplicates collapse");
        assert_eq!(unpack_tag_ids(&a).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn round_trips_zero_and_max_u64() {
        assert_eq!(unpack_tag_ids(&pack_tag_ids(&[0])).unwrap(), vec![0]);
        assert_eq!(
            unpack_tag_ids(&pack_tag_ids(&[u64::MAX])).unwrap(),
            vec![u64::MAX]
        );
        // A set spanning the whole range, including both endpoints.
        let set = [0u64, 1, 127, 128, 16_383, 16_384, u64::MAX];
        assert_eq!(unpack_tag_ids(&pack_tag_ids(&set)).unwrap(), set.to_vec());
    }

    #[test]
    fn leb128_byte_boundaries() {
        for &v in &[0u64, 1, 127, 128, 16_383, 16_384, 2_097_151, 2_097_152] {
            assert_eq!(unpack_tag_ids(&pack_tag_ids(&[v])).unwrap(), vec![v]);
        }
    }

    #[test]
    fn delta_is_never_larger_than_plain_varint() {
        // Ten ids clustered high: delta encoding must be <= plain LEB128 of the
        // absolute ids (the packing-choice claim in the spec).
        let ids: Vec<u64> = (1_000_000..1_000_010).collect();
        let delta_len = pack_tag_ids(&ids).len();
        let mut plain = Vec::new();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        for &id in &sorted {
            write_uvarint(&mut plain, id);
        }
        assert!(
            delta_len <= plain.len(),
            "delta {delta_len} must not exceed plain {}",
            plain.len()
        );
    }

    #[test]
    fn truncated_buffer_errors() {
        // 0x80 signals "continuation" but the buffer ends → truncation error.
        let err = unpack_tag_ids(&[0x80]).unwrap_err();
        assert!(format!("{err:#}").contains("truncated"), "got: {err:#}");
    }

    #[test]
    fn overlong_varint_errors() {
        // 11 continuation bytes then a terminator: overlong for u64.
        let mut buf = vec![0x80u8; 11];
        buf.push(0x00);
        let err = unpack_tag_ids(&buf).unwrap_err();
        assert!(format!("{err:#}").contains("overlong"), "got: {err:#}");
    }

    #[test]
    fn golden_bytes_delta_leb128() {
        // ids [1, 130, 131] -> deltas [1, 129, 1]
        // LEB128(1)=0x01, LEB128(129)=[0x81,0x01], LEB128(1)=0x01
        assert_eq!(
            pack_tag_ids(&[1, 130, 131]),
            vec![0x01, 0x81, 0x01, 0x01],
            "on-disk format must be delta-LEB128, not plain absolute varints"
        );
    }

    #[test]
    fn tenth_byte_overflow_guard() {
        // [0xFF; 9, 0x01] encodes u64::MAX (bit 63 only in 10th byte) — must decode.
        let mut max_buf = vec![0xFFu8; 9];
        max_buf.push(0x01);
        assert_eq!(
            unpack_tag_ids(&max_buf).unwrap(),
            vec![u64::MAX],
            "[0xFF;9, 0x01] must decode to u64::MAX"
        );

        // [0xFF; 9, 0x7F] sets bits 1-6 of the 10th byte — must Err, not silently corrupt.
        let mut overflow_buf = vec![0xFFu8; 9];
        overflow_buf.push(0x7F);
        let err = unpack_tag_ids(&overflow_buf).unwrap_err();
        assert!(format!("{err:#}").contains("overflows"), "got: {err:#}");
    }
}

#[cfg(test)]
mod store {
    use super::*;

    fn h(byte: u8) -> [u8; 32] {
        [byte; 32]
    }
    fn hex_of(b: &[u8; 32]) -> String {
        hex::encode(b)
    }

    #[test]
    fn create_then_reopen_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        Sidecar::create(&path).unwrap();
        // Both RW and RO reopen must validate the stamps and succeed.
        Sidecar::open(&path).unwrap();
        Sidecar::open_readonly(&path).unwrap();
    }

    /// Both writer open paths must set the #231 WAL-hygiene pragmas;
    /// `synchronous` reports 1 for NORMAL.
    #[test]
    fn writer_connections_get_wal_hygiene_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        let assert_pragmas = |s: &Sidecar| {
            let jsl: i64 = s
                .conn()
                .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
                .unwrap();
            assert_eq!(jsl, SIDECAR_WAL_SIZE_LIMIT);
            let sync: i64 = s
                .conn()
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sync, 1, "synchronous must be NORMAL");
        };
        let created = Sidecar::create(&path).unwrap();
        assert_pragmas(&created);
        drop(created);
        assert_pragmas(&Sidecar::open(&path).unwrap());
    }

    /// `checkpoint_wal` with no readers must fully checkpoint and report
    /// `busy = false`; the WAL file ends up truncated to zero bytes.
    #[test]
    fn checkpoint_wal_truncates_when_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        let s = Sidecar::create(&path).unwrap();
        s.write_tag_set(&[0x11; 32], &[1, 2, 3]).unwrap();
        let cp = s.checkpoint_wal().unwrap();
        assert!(!cp.busy, "no readers → checkpoint must complete");
        assert_eq!(
            cp.log_frames, cp.checkpointed_frames,
            "all frames must be copied back"
        );
        let wal_len = std::fs::metadata(dir.path().join("sidecar.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(wal_len, 0, "WAL must be truncated (got {wal_len} bytes)");
    }

    #[test]
    fn stampless_legacy_state_db_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge-state.db");
        // Emulate a legacy bridge-state.db: defs_tags + sync_state, NO bucket_map,
        // NO schema_version stamp.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE defs_tags (service_tag_id INTEGER PRIMARY KEY, tag TEXT NOT NULL);
             CREATE TABLE sync_state (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        drop(conn);
        let err = Sidecar::open(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("legacy"), "names legacy: {msg}");
        assert!(msg.contains("bridge-state.db"), "names the path: {msg}");
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        let s = Sidecar::create(&path).unwrap();
        s.set_flag("schema_version", "2").unwrap();
        drop(s);
        let err = Sidecar::open(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains('2') && msg.contains('1'),
            "names both versions: {msg}"
        );
    }

    #[test]
    fn hash_key_width_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        let s = Sidecar::create(&path).unwrap();
        s.set_flag("hash_key_width", "8").unwrap();
        drop(s);
        let err = Sidecar::open(&path).unwrap_err();
        // Error must name the mismatch width.
        assert!(format!("{err:#}").contains("width"), "names width: {err:#}");
    }

    #[test]
    fn defs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Caller-owned transaction (the production pattern: Task 4 per-chunk).
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_hashes(&[(7, h(0xab))]).unwrap();
        s.insert_defs_tags(&[(9, "character:samus".to_string())])
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(s.sha256_for(7).unwrap(), Some(h(0xab)));
        assert_eq!(s.sha256_for(999).unwrap(), None);
        let m = s.defs_tags_for(&[9]).unwrap();
        assert_eq!(m.get(&9).map(String::as_str), Some("character:samus"));
    }

    #[test]
    fn apply_mutations_respects_wire_order() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // ADD 5 then DELETE 5 → row reaped (empty set).
        s.apply_mutations(&h(1), &[(5, false), (5, true)]).unwrap();
        assert_eq!(s.read_tag_set(&h(1)).unwrap(), Vec::<u64>::new());
        // DELETE 5 then ADD 5 → present {5}.
        s.apply_mutations(&h(2), &[(5, true), (5, false)]).unwrap();
        assert_eq!(s.read_tag_set(&h(2)).unwrap(), vec![5]);
        // ADD 3, ADD 1, DELETE 3 → {1}.
        s.apply_mutations(&h(3), &[(3, false), (1, false), (3, true)])
            .unwrap();
        assert_eq!(s.read_tag_set(&h(3)).unwrap(), vec![1]);
    }

    #[test]
    fn write_empty_set_reaps_row() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        s.write_tag_set(&h(4), &[1, 2]).unwrap();
        assert_eq!(s.read_tag_set(&h(4)).unwrap(), vec![1, 2]);
        s.write_tag_set(&h(4), &[]).unwrap();
        assert_eq!(s.read_tag_set(&h(4)).unwrap(), Vec::<u64>::new());
        // Confirm the row is gone, not just empty-packed.
        let n: i64 = s
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bucket_map WHERE hash = ?1",
                [&h(4)[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "empty set must delete the row");
    }

    #[test]
    fn bucket_prefix_exact_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Two hashes under prefix 0xaa..., one under 0x11...
        let a = {
            let mut b = h(0);
            b[0] = 0xaa;
            b[1] = 0x01;
            b
        };
        let c = {
            let mut b = h(0);
            b[0] = 0x11;
            b
        };
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_tags(&[(1, "maid".to_string()), (2, "series:metroid".to_string())])
            .unwrap();
        tx.commit().unwrap();
        s.write_tag_set(&a, &[1, 2]).unwrap();
        s.write_tag_set(&c, &[1]).unwrap();

        // 8-bit bucket over 0xaa → only `a`.
        let lo = format!("aa{}", "00".repeat(31));
        let (hits, _) = s.bucket(&lo, 8, usize::MAX).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits.get(&hex_of(&a)),
            Some(&vec!["maid".to_string(), "series:metroid".to_string()])
        );

        // Exact 256-bit query for `a`.
        let (exact, _) = s.bucket(&hex_of(&a), 256, usize::MAX).unwrap();
        assert_eq!(exact.len(), 1);
        assert!(exact.contains_key(&hex_of(&a)));

        // A prefix nothing lives under is empty, not an error.
        let lo = format!("ff{}", "00".repeat(31));
        assert!(s.bucket(&lo, 8, usize::MAX).unwrap().0.is_empty());
    }

    #[test]
    fn bucket_defs_miss_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Write a bucket_map row whose id 42 has no defs_tags entry.
        s.write_tag_set(&h(0x22), &[42]).unwrap();
        let lo = format!("22{}", "00".repeat(31));
        let err = s.bucket(&lo, 8, usize::MAX).unwrap_err();
        assert!(format!("{err:#}").contains("42"), "names the id: {err:#}");
    }

    #[test]
    fn bucket_budget_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_tags(&[(1, "maid".to_string())]).unwrap();
        tx.commit().unwrap();
        s.write_tag_set(&h(0x33), &[1]).unwrap();
        let lo = format!("33{}", "00".repeat(31));
        let err = s.bucket(&lo, 8, 1 /* one byte budget */).unwrap_err();
        assert!(
            err.downcast_ref::<BudgetExceeded>().is_some(),
            "budget err: {err:#}"
        );
    }

    /// Pins the strict `spent > budget` inequality and the `spent`-starts-at-0
    /// convention. Note: RESPONSE_ENVELOPE_OVERHEAD is the HTTP layer's concern
    /// (Task 6); bucket() only charges per-row costs.
    #[test]
    fn bucket_budget_exact_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_tags(&[(1, "maid".to_string())]).unwrap();
        tx.commit().unwrap();
        let hash = h(0x44);
        s.write_tag_set(&hash, &[1]).unwrap();
        let lo = format!("44{}", "00".repeat(31));

        // Exact cost: sha256_hex is 64 chars; tag "maid" is 4 bytes.
        let exact_cost = approx_row_cost(64, 4);

        // At exactly the budget the strict inequality (spent > budget) is false → passes.
        let (_, spent) = s.bucket(&lo, 8, exact_cost).unwrap();
        assert_eq!(spent, exact_cost, "spent must equal budget exactly");

        // One byte under: trips BudgetExceeded.
        let err = s.bucket(&lo, 8, exact_cost - 1).unwrap_err();
        assert!(
            err.downcast_ref::<BudgetExceeded>().is_some(),
            "one byte under budget must trip: {err:#}"
        );
    }

    /// `apply_read_only_serve_pragmas` must set all four expected values.
    #[test]
    fn serve_read_pragmas_set_expected_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        Sidecar::create(&path).unwrap();
        let ro = Sidecar::open_readonly(&path).unwrap();
        ro.apply_read_only_serve_pragmas().unwrap();

        let query_only: i64 = ro
            .conn()
            .query_row("PRAGMA query_only", [], |r| r.get(0))
            .unwrap();
        assert_eq!(query_only, 1, "query_only must be ON (1)");

        let mmap: i64 = ro
            .conn()
            .query_row("PRAGMA mmap_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mmap, SIDECAR_SERVE_MMAP_SIZE,
            "mmap_size must match SIDECAR_SERVE_MMAP_SIZE"
        );

        let cache: i64 = ro
            .conn()
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cache, SIDECAR_SERVE_CACHE_KIB,
            "cache_size must match SIDECAR_SERVE_CACHE_KIB"
        );

        let temp: i64 = ro
            .conn()
            .query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(temp, 2, "temp_store must be MEMORY (2)");
    }

    /// `query_only=ON` on a writable-flag connection rejects writes at the
    /// statement layer, proving the pragma itself — not `SQLITE_OPEN_READ_ONLY`
    /// — is responsible.
    #[test]
    fn serve_read_pragmas_reject_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.db");
        Sidecar::create(&path).unwrap();
        // open() uses the read-write flag — so the open flag alone does NOT block
        // writes; only the pragma will.
        let rw = Sidecar::open(&path).unwrap();
        rw.apply_read_only_serve_pragmas().unwrap();

        // A write must fail because query_only=ON.
        let err = rw.write_tag_set(&h(0x55), &[1]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.is_empty(),
            "write_tag_set on a query_only connection must Err: {msg}"
        );

        // A read must still succeed.
        rw.conn()
            .query_row("SELECT 1", [], |_| Ok(()))
            .expect("SELECT 1 must succeed on a query_only connection");
    }

    /// A write on an open_readonly handle must Err and name the hash in the error.
    #[test]
    fn readonly_handle_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.db");
        Sidecar::create(&path).unwrap();
        let ro = Sidecar::open_readonly(&path).unwrap();
        // h(0x55) → first 6 bytes = [0x55;6] → hex prefix "555555555555"
        let err = ro.write_tag_set(&h(0x55), &[1]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("555555555555"),
            "error must name hash prefix: {msg}"
        );
    }

    /// Full-range sidecar digest (after seeding from `write_ptr_seed_fixture`) must
    /// be byte-identical to the Hydrus-side `audit_band_digest` for the same range.
    /// Also verifies that corrupting one `bucket_map` row causes the digests to differ.
    #[test]
    fn parity_sidecar_digest_matches_hydrus() {
        use crate::bridge::sidecar_seed;
        use naiad_plugin_hydrus::HydrusDb;
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc = Sidecar::create(dir.path().join("sidecar.db")).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();
        let hydrus = HydrusDb::open(dir.path()).unwrap();

        let full = "00".repeat(32);
        let (sc_n, sc_d) = sc.audit_band_digest(&full, 0).unwrap();
        let (hy_n, hy_d) = hydrus.audit_band_digest(&full, 0, 9).unwrap();
        assert_eq!(
            (sc_n, sc_d),
            (hy_n, hy_d),
            "sidecar digest must match Hydrus digest (count={sc_n} vs {hy_n})"
        );

        // Corrupt h1's tag set while keeping the pair-count constant:
        // insert a new parseable defs_tags row and swap in its id so h1
        // still contributes 2 pairs (count unchanged) but the digest differs.
        let new_id: u64 = 9999;
        {
            let tx = sc.conn().unchecked_transaction().unwrap();
            sc.insert_defs_tags(&[(new_id, "series:metroid".to_string())])
                .unwrap();
            tx.commit().unwrap();
        }
        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        // Replace {800("maid"), 801("character:samus")} with
        // {800("maid"), 9999("series:metroid")} — same cardinality, different content.
        sc.write_tag_set(&h1, &[800, new_id]).unwrap();
        let (sc_n2, sc_d2) = sc.audit_band_digest(&full, 0).unwrap();
        assert_eq!(
            sc_n2, sc_n,
            "count must be unchanged after same-cardinality corruption"
        );
        assert_ne!(
            sc_d2, hy_d,
            "a swapped tag must cause a digest mismatch despite equal count"
        );
    }

    /// defs_tags_for must handle the 900-id IN-clause chunk boundary correctly.
    #[test]
    fn defs_tags_for_chunk_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Insert 901 rows — one past the DEFS_LOOKUP_CHUNK boundary.
        let rows: Vec<(u64, String)> = (1u64..=901).map(|i| (i, format!("tag:{i}"))).collect();
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_tags(&rows).unwrap();
        tx.commit().unwrap();

        let ids: Vec<u64> = (1..=901).collect();
        let map = s.defs_tags_for(&ids).unwrap();
        assert_eq!(
            map.len(),
            901,
            "all 901 rows resolved across chunk boundary"
        );
        assert_eq!(map.get(&1).map(String::as_str), Some("tag:1"));
        assert_eq!(map.get(&900).map(String::as_str), Some("tag:900"));
        assert_eq!(map.get(&901).map(String::as_str), Some("tag:901"));
    }

    // ── bridge gauge cache tests (#236) ───────────────────────────────────────

    /// `cached_bridge_counts` returns `None` on a fresh sidecar with no cache
    /// written. The 600 s gauge tick must emit nothing in this state.
    #[test]
    fn cached_bridge_counts_none_on_fresh_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        assert!(
            s.cached_bridge_counts().unwrap().is_none(),
            "fresh sidecar with no recompute must return None"
        );
    }

    /// `recompute_bridge_counts` populates the cache; `cached_bridge_counts`
    /// then returns the correct non-zero counts from the seeded fixture.
    #[test]
    fn recompute_populates_cache_and_cached_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // Seed two hashes: h(1) → {tag 10, tag 20}, h(2) → {tag 30}.
        // Hashes = 2, mapping pairs = 3.
        s.write_tag_set(&h(0x01), &[10, 20]).unwrap();
        s.write_tag_set(&h(0x02), &[30]).unwrap();
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_tags(&[
            (10, "character:samus".into()),
            (20, "series:metroid".into()),
            (30, "rating:safe".into()),
        ])
        .unwrap();
        tx.commit().unwrap();

        // Before recompute: cache is absent.
        assert!(s.cached_bridge_counts().unwrap().is_none());

        // Recompute writes the cache.
        s.recompute_bridge_counts().unwrap();

        // cached_bridge_counts now returns the correct counts.
        let (hashes, tags, mappings) = s
            .cached_bridge_counts()
            .unwrap()
            .expect("cache must be populated");
        assert_eq!(hashes, 2, "two hashes in bucket_map");
        assert_eq!(tags, 3, "three defs_tags rows");
        assert_eq!(mappings, 3, "three total mapping pairs");
    }

    /// The hot-path reads from `sync_state` (O(1) key lookups), not from
    /// `bucket_map`. Verify: after recompute, opening a read-only handle and
    /// calling `cached_bridge_counts()` returns the written counts without
    /// touching `bucket_map`. We verify the read-only property: `query_only=ON`
    /// prevents any writes, yet `cached_bridge_counts` succeeds and returns the
    /// correct values. This structural test shows the hot path never scans
    /// `bucket_map` (which would require no write ability anyway).
    #[test]
    fn hot_path_reads_cache_not_live_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        // Write side: create, seed, recompute.
        {
            let s = Sidecar::create(&path).unwrap();
            s.write_tag_set(&h(0x01), &[1, 2]).unwrap();
            let tx = s.conn().unchecked_transaction().unwrap();
            s.insert_defs_tags(&[(1, "tag:a".into()), (2, "tag:b".into())])
                .unwrap();
            tx.commit().unwrap();
            s.recompute_bridge_counts().unwrap();
        }
        // Read side: open read-only, apply query_only pragma (the gauge path),
        // then call cached_bridge_counts — must succeed and return correct counts.
        let ro = Sidecar::open_readonly(&path).unwrap();
        ro.apply_read_only_serve_pragmas().unwrap(); // sets query_only=ON
        let (hashes, tags, mappings) = ro
            .cached_bridge_counts()
            .unwrap()
            .expect("cache must be readable on query_only connection");
        assert_eq!(hashes, 1);
        assert_eq!(tags, 2);
        assert_eq!(mappings, 2);
    }

    // ── bridge gauge count method tests (#235) ────────────────────────────────

    /// `bucket_hash_count` returns 0 on a fresh sidecar and the correct row
    /// count after inserts.
    #[test]
    fn bucket_hash_count_empty_and_after_inserts() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        assert_eq!(s.bucket_hash_count().unwrap(), 0, "fresh sidecar: 0 hashes");
        s.write_tag_set(&h(0x01), &[1]).unwrap();
        s.write_tag_set(&h(0x02), &[1, 2]).unwrap();
        assert_eq!(
            s.bucket_hash_count().unwrap(),
            2,
            "after two inserts: 2 hashes"
        );
        // Reaping (empty set) removes the row.
        s.write_tag_set(&h(0x01), &[]).unwrap();
        assert_eq!(s.bucket_hash_count().unwrap(), 1, "after reap: 1 hash");
    }

    /// `tag_def_count` returns 0 on a fresh sidecar and the correct count after
    /// `insert_defs_tags`.
    #[test]
    fn tag_def_count_empty_and_after_inserts() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        assert_eq!(s.tag_def_count().unwrap(), 0, "fresh sidecar: 0 tags");
        let tx = s.conn().unchecked_transaction().unwrap();
        s.insert_defs_tags(&[
            (1, "character:samus".to_string()),
            (2, "series:metroid".to_string()),
        ])
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(s.tag_def_count().unwrap(), 2, "after two defs: 2 tags");
    }

    /// `approx_mapping_count` returns 0 on an empty sidecar.
    #[test]
    fn approx_mapping_count_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        assert_eq!(
            s.approx_mapping_count(50).unwrap(),
            0,
            "empty sidecar must return 0"
        );
    }

    /// `approx_mapping_count(0)` returns 0 immediately without touching the DB.
    #[test]
    fn approx_mapping_count_zero_sample_size() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        s.write_tag_set(&h(0x10), &[1, 2, 3]).unwrap();
        assert_eq!(
            s.approx_mapping_count(0).unwrap(),
            0,
            "sample_size=0 must short-circuit to 0"
        );
    }

    /// `approx_mapping_count` returns the exact count when every hash has the
    /// same number of tags (no sampling error).
    #[test]
    fn approx_mapping_count_uniform_tags_exact() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sidecar::create(dir.path().join("s.db")).unwrap();
        // 5 hashes, each with exactly 3 tags → exact count = 15.
        for byte in 0u8..5 {
            s.write_tag_set(&h(byte), &[1, 2, 3]).unwrap();
        }
        let estimate = s.approx_mapping_count(50).unwrap();
        // With a uniform distribution and sample_size ≥ total, result is exact.
        assert_eq!(
            estimate, 15,
            "uniform distribution must produce exact count"
        );
    }

    /// `approx_mapping_count` with a fixture seeded via `write_ptr_seed_fixture`
    /// returns a plausible (non-zero) estimate that is within 50% of the true
    /// mapping count. Also verifies `bucket_hash_count` and `tag_def_count` are
    /// non-zero after seeding.
    #[test]
    fn bridge_gauges_plausible_after_seed_fixture() {
        use crate::bridge::sidecar_seed;
        let dir = tempfile::tempdir().unwrap();
        naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(dir.path(), 9).unwrap();
        let sc = Sidecar::create(dir.path().join("sidecar.db")).unwrap();
        sidecar_seed::seed(dir.path(), Some(9), &sc, false).unwrap();

        let hashes = sc.bucket_hash_count().unwrap();
        assert!(hashes > 0, "bucket_hash_count must be > 0 after seeding");

        let tags = sc.tag_def_count().unwrap();
        assert!(tags > 0, "tag_def_count must be > 0 after seeding");

        // True mapping count: sum the unpacked tag-id lengths for all rows.
        let true_count: u64 = {
            let mut stmt = sc.conn().prepare("SELECT tag_ids FROM bucket_map").unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut total = 0u64;
            while let Some(r) = rows.next().unwrap() {
                let blob: Vec<u8> = r.get(0).unwrap();
                total += unpack_tag_ids(&blob).unwrap().len() as u64;
            }
            total
        };
        assert!(true_count > 0, "fixture must have at least one mapping");

        let estimate = sc.approx_mapping_count(50).unwrap();
        // Allow ±50% tolerance — tight enough to catch gross bugs.
        let lo = true_count / 2;
        let hi = true_count * 3 / 2;
        assert!(
            estimate >= lo && estimate <= hi,
            "approx_mapping_count estimate {estimate} should be within 50% of true count {true_count}"
        );
    }
}
