//! The repository's SQLite store: accounts, submissions log, repo_mappings,
//! reports, relations. v6 simple client/server model (spec §3, ADR 0021).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use naiad_core::{
    BudgetExceeded, Hash, RESPONSE_ENVELOPE_OVERHEAD, Tag, approx_row_cost, json_escaped_len,
};
use naiad_netproto::{
    Account, AuthoredEdge, DeltaEdge, DeltaMapping, EdgeStatus, MappingStatus, Op, OriginTag,
    PROTOCOL_VERSION, PullMode, RelKind, RelationGraph, RelationSubmission, ReportRow, Submission,
    verify,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use rusqlite_migration::{Error as MigrationError, M, MigrationDefinitionError, Migrations};

static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_baseline.sql")),
        M::up(include_str!("../migrations/0002_submission_origin.sql")),
        M::up(include_str!("../migrations/0003_intern_mappings.sql")),
        M::up(include_str!(
            "../migrations/0004_repo_hashes_explicit_index.sql"
        )),
        M::up(include_str!("../migrations/0005_repo_meta.sql")),
    ])
});

/// Named unique index on `repo_hashes(hash)` created by migration 0004.
///
/// Shared constant referenced by the open-time assertion, and by the
/// Task-2 drop/build helpers (`drop_hash_unique_index` /
/// `build_hash_unique_index`). Using the name in one place prevents the
/// assertion and the builder from diverging silently.
pub(crate) const HASH_UNIQUE_INDEX: &str = "repo_hashes_hash_unique";

/// Convenience result type for this crate.
pub type Result<T> = anyhow::Result<T>;

/// A handle to a repository node's store.
pub struct RepoStore {
    pub(crate) conn: Connection,
}

impl std::fmt::Debug for RepoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoStore").finish_non_exhaustive()
    }
}

/// Summary returned by [`RepoStore::apply_mappings_bulk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BulkApplyStats {
    /// Rows upserted to `current` (is_delete = false).
    pub applied: u64,
    /// Rows upserted to `deleted` (is_delete = true).
    pub deleted: u64,
}

/// Serde default for `SeedCheckpoint::v` (forward-compat schema tag).
fn one() -> u32 {
    1
}

/// Which phase-1 pass a seed checkpoint belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedPass {
    Current,
    Deleted,
}

/// A phase-1 seed checkpoint (repo_meta key `seed_ckpt`).
///
/// Written in the same transaction as each chunk flush (I1). `high_water` is a
/// fully-ingested source `hash_id` (I2); resume streams `WHERE m.hash_id > high_water`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeedCheckpoint {
    /// Schema tag for forward-compat; current value 1.
    #[serde(default = "one")]
    pub v: u32,
    pub pass: SeedPass,
    pub high_water: u64,
    pub service_id: i64,
    /// Snapshot fingerprint (seed.rs §6). Opaque; compared byte-for-byte.
    pub fp: String,
}

/// Caller-owned intern caches for [`RepoStore::apply_mappings_bulk_cached`].
///
/// `hash` is per-batch (cleared at the start of each call to bound RAM; the
/// hash-major ingest locality makes it a near-100% hit within a 250 k-row
/// chunk anyway). `tag` persists across calls for the whole seed lifetime
/// (~1.1 M entries × ~100 MB), eliminating repeated `SELECT repo_tags` probes.
/// `hash_carry` is used only by the `DeferredAppend` path (see
/// [`RepoStore::apply_current_mappings_deferred`]): it holds the last
/// (hash-bytes, id) pair resolved across chunk boundaries. It is intentionally
/// NOT cleared when `hash` is cleared — it must survive chunk calls.
#[derive(Default)]
pub struct InternCaches {
    /// Hash blob → `repo_hashes.id`. Cleared at the start of each Indexed
    /// chunk call. Unused by the DeferredAppend path.
    pub hash: HashMap<[u8; 32], i64>,
    /// Tag string → `repo_tags.id`. Persists across chunk calls.
    pub tag: HashMap<String, i64>,
    /// Cross-chunk carry for the DeferredAppend path: `(hash_bytes, id)` of
    /// the last interned hash. Survives chunk boundaries (not cleared with
    /// `hash`). `None` until the first hash is appended. Ignored by Indexed.
    pub hash_carry: Option<([u8; 32], i64)>,
}

/// Hash-resolution strategy for the shared per-row mapping loop.
///
/// `Indexed` is the normal path: requires `repo_hashes_hash_unique` to be
/// present; resolves each hash with a read-first `SELECT` + `INSERT ON
/// CONFLICT`. `DeferredAppend` is the fresh-seed fast path: the unique index
/// is absent; appends each new hash with `INSERT … RETURNING id` (no
/// `ON CONFLICT`, no `SELECT`) and carries a single `(hash, id)` across chunk
/// boundaries so a run of same-hash rows within a chunk avoids repeated inserts.
#[derive(Clone, Copy)]
enum HashResolve {
    Indexed,
    DeferredAppend,
}

/// Summary returned by [`RepoStore::seed_mappings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedSummary {
    /// Mappings newly inserted (not previously `current`).
    pub inserted: u64,
    /// Mappings skipped because they were already `current`.
    pub skipped: u64,
    /// Total mappings seen (`inserted + skipped`).
    pub total: u64,
}

/// An account row from the `accounts` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub pubkey: String,
    /// `"contributor"` | `"moderator"`
    pub role: String,
    pub banned: bool,
    pub created_at: i64,
    pub note: Option<String>,
}

/// Reject databases that are not naiad repo stores before migrations run.
///
/// A client library handed to the server would otherwise surface as
/// `DatabaseTooFarAhead` (the client chain is ~31 migrations deep, the repo
/// chain is 1), which reads as a version mismatch rather than the wrong file.
/// Checking for the `repo_mappings` table (repo signature) and the `files`
/// table (client signature) makes the error self-explanatory. Note: if the
/// client ever renames its `files` table, that arm falls through to the
/// generic "not a naiad repo database" message — still actionable, just less
/// specific.
fn reject_foreign_db(conn: &Connection, path: &Path) -> Result<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .with_context(|| format!("cannot read schema version from {}", path.display()))?;
    if version == 0 {
        return Ok(()); // fresh or empty file: migrations will build it
    }
    let table_exists = |name: &str| -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |r| r.get(0),
        )
    };
    if table_exists("repo_mappings")? {
        return Ok(());
    }
    if table_exists("files")? {
        anyhow::bail!(
            "{} is a naiad client library, not a repo database; \
             point --db at the repo's repo.db (or a new path to create one)",
            path.display()
        );
    }
    anyhow::bail!(
        "{} is not a naiad repo database (schema version {version}, no repo tables)",
        path.display()
    );
}

/// Check whether the named unique index (`repo_hashes_hash_unique`) exists.
///
/// A single `sqlite_master` lookup — cheap, lock-free read. Returns `true`
/// when the index is present and `false` when it is absent (e.g. during
/// an in-progress deferred seed or after a crash mid-current-pass).
///
/// Called from the serving/sync openers (`open`, `open_readonly`) to refuse
/// a store that cannot safely serve bucket/snapshot queries. NOT called from
/// `open_bulk_ingest` or `init` — the seed manages the index lifecycle itself.
///
/// This is the low-level connection-level check. The store-level wrapper is
/// [`RepoStore::has_hash_unique_index`].
fn hash_unique_index_present(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [HASH_UNIQUE_INDEX],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
    .map_err(Into::into)
}

/// Bail if the named unique index is absent, with an actionable message.
fn assert_hash_index_present(conn: &Connection) -> Result<()> {
    if !hash_unique_index_present(conn)? {
        anyhow::bail!(
            "repo.db has an incomplete bridge seed: the repo_hashes uniqueness \
             index is missing. Resume the seed (`naiad-repo bridge seed …` \
             self-heals the index) or delete repo.db and re-seed."
        );
    }
    Ok(())
}

impl RepoStore {
    /// Open (creating if absent) the store at `path` and apply migrations.
    ///
    /// Runs a preflight check to reject foreign databases (client libraries,
    /// unknown files) with a self-explanatory error before migrations run —
    /// otherwise a client library surfaces as `DatabaseTooFarAhead`, which
    /// reads as a version mismatch rather than the wrong file.
    ///
    /// Refuses a store whose `repo_hashes_hash_unique` index is absent:
    /// that indicates an incomplete or aborted bridge seed. Resume the seed
    /// (which self-heals the index) or delete `repo.db` and re-seed.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be opened, the file is not a
    /// naiad repo database, migrations fail, or the uniqueness index is absent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        reject_foreign_db(&conn, path)?;
        let store = Self::init(conn)?;
        assert_hash_index_present(&store.conn)?;
        Ok(store)
    }

    /// Open an in-memory store (for tests) and apply migrations.
    ///
    /// # Errors
    /// Returns an error if migrations fail.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// Open an existing repo DB read-only, for the server's read path.
    ///
    /// Applies the same preflight as [`RepoStore::open`] so pointing
    /// `--db` at a client library fails immediately rather than silently.
    ///
    /// Refuses a store whose `repo_hashes_hash_unique` index is absent (same
    /// contract as [`RepoStore::open`]): bucket/snapshot range scans on
    /// `repo_hashes(hash)` are unindexed without it.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened read-only, the file is
    /// not a naiad repo database, the path is not found, or the uniqueness
    /// index is absent.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
        let conn = Connection::open_with_flags(path, flags)?;
        reject_foreign_db(&conn, path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        let store = Self { conn };
        assert_hash_index_present(&store.conn)?;
        Ok(store)
    }

    /// Apply serve-only read pragmas to this connection (#202): `query_only` (reject
    /// any accidental write) and a large `mmap_size` for read throughput on a big
    /// store. Deliberately does NOT set `immutable=1` — the store may be written by
    /// a separate process (the nightly `bridge sync`), which `immutable=1` would
    /// make unsafe (design §10.1).
    ///
    /// # Errors
    /// Returns an error if a pragma update fails.
    pub fn apply_read_only_serve_pragmas(&self) -> Result<()> {
        self.conn.pragma_update(None, "query_only", "ON")?;
        // 1 GiB mmap window; the OS pages it lazily, so this is a ceiling not an alloc.
        self.conn
            .pragma_update(None, "mmap_size", 1_073_741_824i64)?;
        Ok(())
    }

    /// Open (creating if absent) the store at `path` for high-throughput bulk
    /// ingest, such as a bridge seed.
    ///
    /// Applies the same preflight and migrations as [`RepoStore::open`], then
    /// overrides PRAGMAs on this connection only:
    ///
    /// | PRAGMA | Value |
    /// |---|---|
    /// | `cache_size` | 256 MiB default; `NAIAD_REPO_BULK_CACHE_MB` overrides |
    /// | `wal_autocheckpoint` | 50000 (~200 MB WAL before checkpoint) |
    /// | `temp_store` | MEMORY |
    /// | `synchronous` | NORMAL (default), or OFF when `unsafe_fast = true` |
    ///
    /// The page cache must hold the hot interior of the `repo_hashes`
    /// `UNIQUE(hash)` index — its keys are random sha bytes, so its inserts are
    /// the one write surface hash-major ordering cannot make sequential. Once
    /// that index outgrows the cache the seed rate decays toward random-I/O.
    /// Size `NAIAD_REPO_BULK_CACHE_MB` ≈ the expected index size (~45 B/hash;
    /// e.g. ~550 MB for 12 M hashes) where RAM allows. Values are clamped to
    /// [64, 16384] MB; unparsable values fall back to 256 with a warning.
    ///
    /// # Safety contract — `synchronous = OFF` (`unsafe_fast = true`)
    ///
    /// With `--unsafe-fast` the connection uses `synchronous=OFF`. On an OS
    /// crash or power loss *during the seed*, partially-written WAL frames may
    /// not be durable; a subsequent checkpoint can leave `repo.db` **corrupted**
    /// (not merely missing the last transaction). If that happens, **delete
    /// `repo.db` and re-seed from scratch** — phase 1 restarts safely from an
    /// empty store.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be opened, the file is not a
    /// naiad repo database, or migrations fail.
    pub fn open_bulk_ingest(path: impl AsRef<Path>, unsafe_fast: bool) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        reject_foreign_db(&conn, path)?;
        let store = Self::init(conn)?;
        // Override the serving profile with bulk-ingest tunings on this
        // connection only.  The serving daemon uses open()/open_readonly() and
        // is never affected.
        let cache_mb: i64 = match std::env::var("NAIAD_REPO_BULK_CACHE_MB") {
            Ok(v) => match v.trim().parse::<i64>() {
                Ok(n) => n.clamp(64, 16_384),
                Err(_) => {
                    tracing::warn!(
                        target: "db",
                        value = %v,
                        "NAIAD_REPO_BULK_CACHE_MB is not a number; using 256"
                    );
                    256
                }
            },
            Err(_) => 256,
        };
        store
            .conn
            .pragma_update(None, "cache_size", -(cache_mb * 1024))?;
        store
            .conn
            .pragma_update(None, "wal_autocheckpoint", 50_000i64)?;
        store.conn.pragma_update(None, "temp_store", "MEMORY")?;
        let sync_mode = if unsafe_fast { "OFF" } else { "NORMAL" };
        store.conn.pragma_update(None, "synchronous", sync_mode)?;
        Ok(store)
    }

    /// Drop the named unique index on `repo_hashes(hash)`.
    ///
    /// Called at the start of a fresh deferred seed to remove the uniqueness
    /// B-tree before the no-SELECT current-pass append. The index is rebuilt
    /// via [`Self::build_hash_unique_index`] after the current pass completes.
    ///
    /// # Errors
    /// Returns an error if the DDL statement fails (e.g. the index is already
    /// absent — callers are responsible for not calling this twice).
    pub fn drop_hash_unique_index(&self) -> Result<()> {
        self.conn
            .execute_batch(&format!("DROP INDEX {HASH_UNIQUE_INDEX}"))?;
        Ok(())
    }

    /// Build (or rebuild) the named unique index on `repo_hashes(hash)`.
    ///
    /// Called after the deferred current-pass append completes (spec §4.2) and
    /// during the resume self-heal (§4.5). Logs the duration at `info!` (target
    /// `"db"`). Fail-loud on duplicates: if the append path ever inserted a
    /// duplicate hash, `CREATE UNIQUE INDEX` raises a uniqueness violation and
    /// the seed aborts — the correctness backstop (I8).
    ///
    /// # Errors
    /// Returns an error if the index creation fails, including on duplicate-hash
    /// violations.
    pub fn build_hash_unique_index(&self) -> Result<()> {
        let t0 = Instant::now();
        self.conn.execute_batch(&format!(
            "CREATE UNIQUE INDEX {HASH_UNIQUE_INDEX} ON repo_hashes(hash)"
        ))?;
        tracing::info!(
            target: "db",
            elapsed_ms = t0.elapsed().as_millis(),
            "build_hash_unique_index: index created"
        );
        Ok(())
    }

    /// True iff the named unique index on `repo_hashes(hash)` is present.
    ///
    /// Used by the seed's fresh-detection (§4.1) and self-heal (§4.5) logic.
    /// A single `sqlite_master` lookup — cheap, lock-free.
    ///
    /// This is a thin store-level wrapper around the connection-level free
    /// function [`hash_unique_index_present`].
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub(crate) fn has_hash_unique_index(&self) -> Result<bool> {
        hash_unique_index_present(&self.conn)
    }

    /// Run `f` inside a single deferred read transaction.
    ///
    /// # Errors
    /// Propagates whatever `f` returns; the transaction is rolled back on `Err`.
    pub fn read_snapshot<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        // Take an RAII guard rather than issuing raw BEGIN/COMMIT. Cleanup on
        // the normal return path is not enough: if `f` panics, the unwind skips
        // straight past any manual COMMIT/ROLLBACK and strands an open
        // transaction on the shared connection. Recovering the poisoned mutex
        // would then hand the next handler a connection whose own BEGIN fails
        // with "cannot start a transaction within a transaction" — the same
        // permanent outage #137 removes, wearing a different error. The guard's
        // Drop runs during unwind, so the connection is always left clean.
        //
        // `unchecked_transaction` (rather than `transaction`) because this takes
        // &self; it defaults to BEGIN DEFERRED and to rollback on drop, matching
        // the previous behaviour on the non-panicking paths.
        let tx = self.conn.unchecked_transaction()?;
        let out = f(self);
        if out.is_ok() {
            // Errors ignored to preserve the prior contract: `f`'s result is
            // what callers get. Committing a read transaction only releases it.
            let _ = tx.commit();
        }
        // On Err, and on unwind, the guard drops and rolls back.
        out
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // synchronous=NORMAL: same reasoning as the client library DB — WAL
        // crash recovery makes FULL redundant, and removing the per-commit
        // fsync significantly reduces write latency for bulk submissions.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // cache_size: negative values are KiB. 8 MiB is appropriate for the
        // repo store, which is smaller and primarily read-heavy (submissions
        // are infrequent relative to reads in a typical deployment).
        conn.pragma_update(None, "cache_size", -8192i64)?;
        // mmap_size: map 64 MiB of the file into the process address space
        // to eliminate one memcpy per read page on 64-bit hosts.
        conn.pragma_update(None, "mmap_size", 67_108_864i64)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        // Conscious design decision: disable FK enforcement repo-store-wide.
        //
        // Reasons (§2.1 of the deferred-hash-index spec, #187):
        //   (1) Writers always intern parent rows before child rows —
        //       `repo_hashes` rows are inserted before any `repo_mappings` row
        //       that references them, so FK checks would never catch a real bug.
        //   (2) Migration 0004 rebuilds `repo_hashes` via DROP + CREATE TABLE
        //       while `repo_mappings.hash_id` references it; FK enforcement
        //       would reject the DROP with "FOREIGN KEY constraint failed".
        //   (3) Removing the per-INSERT parent-lookup overhead benefits the
        //       bulk-ingest hot path at no correctness cost (see reason 1).
        //
        // The bundled SQLite is compiled with SQLITE_DEFAULT_FOREIGN_KEYS=1
        // (FKs enforced by default), so we must explicitly turn this OFF here —
        // before any transaction begins — or migration 0004 would fail.
        // PRAGMA foreign_keys is a no-op inside transactions, so this call
        // must precede to_latest.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        if let Err(e) = MIGRATIONS.to_latest(&mut conn) {
            // Distinguish a "binary is too old for this DB" error from all
            // other migration failures so the operator knows to upgrade rather
            // than wonder what went wrong.
            if matches!(
                e,
                MigrationError::MigrationDefinition(MigrationDefinitionError::DatabaseTooFarAhead)
            ) {
                return Err(anyhow::Error::new(e).context(
                    "repo database was created by a newer naiad-repo; upgrade this binary",
                ));
            }
            return Err(e.into());
        }
        Ok(Self { conn })
    }

    // ── Accounts ──────────────────────────────────────────────────────────────

    /// Ensure an account row exists for `pubkey`, creating it as a contributor
    /// if absent. Idempotent: re-calling on an existing pubkey is a no-op.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub fn ensure_account(&self, pubkey: &str, created_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO accounts(pubkey, created_at) VALUES(?1, ?2)",
            params![pubkey, created_at],
        )?;
        Ok(())
    }

    /// Look up one account by pubkey.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn account(&self, pubkey: &str) -> Result<Option<AccountRow>> {
        self.conn
            .query_row(
                "SELECT pubkey, role, banned, created_at, note FROM accounts WHERE pubkey = ?1",
                params![pubkey],
                |r| {
                    Ok(AccountRow {
                        pubkey: r.get(0)?,
                        role: r.get(1)?,
                        banned: r.get::<_, i64>(2)? != 0,
                        created_at: r.get(3)?,
                        note: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Set the role for `pubkey` (`"contributor"` or `"moderator"`).
    ///
    /// # Errors
    /// Returns an error if no such account exists or the statement fails.
    pub fn set_role(&self, pubkey: &str, role: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE accounts SET role = ?1 WHERE pubkey = ?2",
            params![role, pubkey],
        )?;
        if n == 0 {
            anyhow::bail!("set_role: account not found: {pubkey}");
        }
        Ok(())
    }

    /// Set the `banned` flag for `pubkey`. Banning leaves existing submissions
    /// and mappings untouched; it only blocks future submissions and reports.
    ///
    /// # Errors
    /// Returns an error if no such account exists or the statement fails.
    pub fn set_banned(&self, pubkey: &str, banned: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE accounts SET banned = ?1 WHERE pubkey = ?2",
            params![banned as i64, pubkey],
        )?;
        if n == 0 {
            anyhow::bail!("set_banned: account not found: {pubkey}");
        }
        Ok(())
    }

    /// `true` iff `pubkey` has role `"moderator"` and is not banned.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn is_moderator(&self, pubkey: &str) -> Result<bool> {
        let role: Option<String> = self
            .conn
            .query_row(
                "SELECT role FROM accounts WHERE pubkey = ?1 AND banned = 0",
                params![pubkey],
                |r| r.get(0),
            )
            .optional()?;
        Ok(role.as_deref() == Some("moderator"))
    }

    /// Every account row, ordered by `created_at`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn list_accounts(&self) -> Result<Vec<AccountRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT pubkey, role, banned, created_at, note FROM accounts ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AccountRow {
                    pubkey: r.get(0)?,
                    role: r.get(1)?,
                    banned: r.get::<_, i64>(2)? != 0,
                    created_at: r.get(3)?,
                    note: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── Submissions / mappings ─────────────────────────────────────────────────

    /// Apply one signed submission:
    /// - verifies the `naiad-sub` signature,
    /// - auto-creates the account on first submit (`INSERT OR IGNORE`),
    /// - rejects if the account is banned,
    /// - appends to the signed submissions log,
    /// - upserts `repo_mappings` with the next unified seq.
    ///
    /// The submissions-log INSERT and the repo_mappings UPSERT are wrapped in an
    /// IMMEDIATE transaction so a crash between the two cannot leave the audit
    /// log inconsistent with the current view.
    ///
    /// # Errors
    /// Returns an error if signature verification fails, the account is banned,
    /// or any statement fails.
    pub fn apply_submission(&self, sub: &Submission) -> Result<()> {
        // Verify signature first — a bad sig must not create a phantom account.
        verify(sub)?;
        let ts = now();
        // Auto-create the account if it doesn't exist yet.  This runs outside
        // the write transaction because INSERT OR IGNORE is idempotent and the
        // account must exist before we check the banned flag.
        self.conn.execute(
            "INSERT OR IGNORE INTO accounts(pubkey, created_at) VALUES(?1, ?2)",
            params![sub.author, ts],
        )?;
        // Reject banned submitters.
        let banned: i64 = self.conn.query_row(
            "SELECT banned FROM accounts WHERE pubkey = ?1",
            params![sub.author],
            |r| r.get(0),
        )?;
        if banned != 0 {
            anyhow::bail!("account {} is banned", sub.author);
        }
        let status_int: i64 = match sub.op {
            Op::Add => 0,
            Op::Remove => 1,
        };
        // Wrap the two-table write in a transaction so the audit log and the
        // current view are always consistent.
        let tx = self.conn.unchecked_transaction()?;
        // Compute the next seq inside the transaction to avoid a race.  All
        // changes to repo_mappings — user submissions and moderator actions —
        // advance this counter; clients use it as their cursor.
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM repo_mappings",
            [],
            |r| r.get(0),
        )?;
        // Append to the signed log (AUTOINCREMENT handles the log's own seq).
        // submissions table keeps hex hash / text tag — N4 (not interned).
        tx.execute(
            "INSERT INTO submissions(op, hash, tag, author, signature, created_at, origin)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                sub.op.as_str(),
                sub.hash,
                sub.tag,
                sub.author,
                sub.signature,
                ts,
                sub.origin
            ],
        )?;
        // Intern hash and tag, then upsert the current-view mapping.
        let hash_bytes = hex::decode(&sub.hash)
            .with_context(|| format!("submission hash is not valid hex: {}", sub.hash))?;
        anyhow::ensure!(
            hash_bytes.len() == 32,
            "submission hash is not 32 bytes: {}",
            sub.hash
        );
        // Read-first resolver: an upsert-RETURNING would perform a real row
        // UPDATE on every conflict (WAL write per lookup); a point SELECT on the
        // unique index is write-free and the INSERT only runs on a true miss.
        let hash_id: i64 = match tx
            .query_row(
                "SELECT id FROM repo_hashes WHERE hash = ?1",
                params![hash_bytes],
                |r| r.get(0),
            )
            .optional()?
        {
            Some(id) => id,
            None => tx.query_row(
                "INSERT INTO repo_hashes(hash) VALUES(?1) RETURNING id",
                params![hash_bytes],
                |r| r.get(0),
            )?,
        };
        let tag_id: i64 = match tx
            .query_row(
                "SELECT id FROM repo_tags WHERE tag = ?1",
                params![sub.tag],
                |r| r.get(0),
            )
            .optional()?
        {
            Some(id) => id,
            None => tx.query_row(
                "INSERT INTO repo_tags(tag) VALUES(?1) RETURNING id",
                params![sub.tag],
                |r| r.get(0),
            )?,
        };
        // Upsert the current-view mapping (id-based).
        //
        // Deliberately NOT remove-dominates (unlike apply_relation): a later
        // signed Add always resurrects a moderator-deleted mapping.  The spec's
        // moderator actions are delete/ban/dismiss with no undelete; the correct
        // recourse against repeat offenders is ban, not a sticky tombstone.
        //
        // origin follows the same "latest assertion wins" rule as status/seq:
        // a later signed Add carrying a new origin updates repo_mappings.origin.
        // A Remove usually carries origin NULL (the local mapping is already
        // gone when the client derives it) and clobbers origin here — inert,
        // since every pull-serving read filters status = 0.
        // §6.2 guard: skip echoes where neither status nor origin changed.
        // IS NOT (null-safe distinct) fires on NULL↔value transitions too, so a
        // re-assert that clobbers origin to NULL (tested by origin_persists…)
        // still fires the update. The submissions INSERT above is unconditional —
        // the signed event is always logged regardless (spec §6.2 last paragraph).
        // next_seq: computed fresh from MAX(seq) each call, so if this guard
        // fires 0 rows (echo), MAX(seq) is unchanged and the cursor stays put
        // automatically — no Rust-side reclaim needed here.
        //
        // #202: sample this hash's current-mapping count BEFORE the upsert so we
        // can detect a 0↔≥1 distinct-current-hash transition afterward.
        let before_current: i64 = tx.query_row(
            "SELECT COUNT(*) FROM repo_mappings WHERE hash_id = ?1 AND status = 0",
            params![hash_id],
            |r| r.get(0),
        )?;
        let changed = tx.execute(
            "INSERT INTO repo_mappings(hash_id, tag_id, status, seq, origin)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(hash_id, tag_id) DO UPDATE SET
                 status = excluded.status, seq = excluded.seq, origin = excluded.origin
                 WHERE repo_mappings.status IS NOT excluded.status
                    OR repo_mappings.origin IS NOT excluded.origin",
            params![hash_id, tag_id, status_int, next_seq, sub.origin],
        )?;
        // #202: maintain the persisted distinct-hash count on a genuine change
        // only, and only when a count row already exists (a pre-upgrade store
        // with no row is left for the serve-side one-shot fallback to populate).
        if changed > 0 {
            let after_current: i64 = tx.query_row(
                "SELECT COUNT(*) FROM repo_mappings WHERE hash_id = ?1 AND status = 0",
                params![hash_id],
                |r| r.get(0),
            )?;
            let delta: i64 = match (before_current, after_current) {
                (0, a) if a > 0 => 1,
                (b, 0) if b > 0 => -1,
                _ => 0,
            };
            if delta != 0 {
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT value FROM repo_meta WHERE key = 'distinct_hash_count'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(cur) = existing.and_then(|s| s.trim().parse::<u64>().ok()) {
                    let next = if delta > 0 {
                        cur + 1
                    } else {
                        cur.saturating_sub(1)
                    };
                    tx.execute(
                        "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('distinct_hash_count', ?1)",
                        params![next.to_string()],
                    )?;
                }
            }
        }
        tx.commit()?;
        tracing::debug!(target: "db", author = %sub.author, op = ?sub.op, seq = next_seq, "applied submission");
        Ok(())
    }

    /// Bulk-apply operator-signed Add submissions in one transaction.
    /// Idempotent: a (hash, tag) already `current` is skipped (counted, not
    /// re-signed). Duplicate pairs within one call are also deduplicated: the
    /// second occurrence finds the row inserted by the first and is skipped.
    ///
    /// # Errors
    /// Returns an error if the account is banned or any DB statement fails.
    pub fn seed_mappings<I>(&self, account: &Account, items: I) -> Result<SeedSummary>
    where
        I: IntoIterator<Item = (Hash, Tag)>,
    {
        let ts = now();
        let pubkey = account.public_hex();

        // Once, before the loop: ensure the operator account exists, then check
        // the banned flag. This mirrors apply_submission's account-creation
        // semantics but is hoisted out of the per-row loop.
        self.conn.execute(
            "INSERT OR IGNORE INTO accounts(pubkey, created_at) VALUES(?1, ?2)",
            params![pubkey, ts],
        )?;
        let banned: i64 = self.conn.query_row(
            "SELECT banned FROM accounts WHERE pubkey = ?1",
            params![pubkey],
            |r| r.get(0),
        )?;
        if banned != 0 {
            anyhow::bail!("account {} is banned", pubkey);
        }

        let tx = self.conn.unchecked_transaction()?;

        // Seed the seq counter once. Each insert pre-increments before use so
        // no seq value is ever reused, even across concurrent transactions.
        let mut next_seq: i64 =
            tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM repo_mappings", [], |r| {
                r.get(0)
            })?;

        // Prepare the statements once and reuse them in the loop to avoid
        // per-row statement compilation overhead. Interning is read-first: a
        // conflict-upsert would write a row per lookup; SELECT is write-free.
        let mut sel_hash_stmt = tx.prepare("SELECT id FROM repo_hashes WHERE hash = ?1")?;
        let mut ins_hash_stmt =
            tx.prepare("INSERT INTO repo_hashes(hash) VALUES(?1) RETURNING id")?;
        let mut sel_tag_stmt = tx.prepare("SELECT id FROM repo_tags WHERE tag = ?1")?;
        let mut ins_tag_stmt = tx.prepare("INSERT INTO repo_tags(tag) VALUES(?1) RETURNING id")?;
        let mut check_stmt =
            tx.prepare("SELECT status FROM repo_mappings WHERE hash_id = ?1 AND tag_id = ?2")?;
        let mut sub_stmt = tx.prepare(
            "INSERT INTO submissions(op, hash, tag, author, signature, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut map_stmt = tx.prepare(
            // §6.4 defence-in-depth: the pre-check above already prevents current→current
            // no-ops from reaching here, but the guard ensures the upsert is still a no-op
            // (0 rows) if somehow a duplicate slips through (e.g. deleted→deleted).
            "INSERT INTO repo_mappings(hash_id, tag_id, status, seq) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(hash_id, tag_id) DO UPDATE SET
                 status = excluded.status, seq = excluded.seq
                 WHERE repo_mappings.status IS NOT excluded.status",
        )?;

        let mut inserted: u64 = 0;
        let mut skipped: u64 = 0;

        for (hash, tag) in items {
            let hash_str = hash.to_hex();
            let tag_str = tag.to_string();

            // Intern the hash (32-byte blob) and the tag string, read-first.
            let hash_bytes = hash.as_bytes().to_vec();
            let hash_id: i64 = match sel_hash_stmt
                .query_row(params![hash_bytes], |r| r.get(0))
                .optional()?
            {
                Some(id) => id,
                None => ins_hash_stmt.query_row(params![hash_bytes], |r| r.get(0))?,
            };
            let tag_id: i64 = match sel_tag_stmt
                .query_row(params![tag_str], |r| r.get(0))
                .optional()?
            {
                Some(id) => id,
                None => ins_tag_stmt.query_row(params![tag_str], |r| r.get(0))?,
            };

            // Idempotency check: skip pairs already `current` (status = 0).
            // A pair inserted earlier in this same transaction also appears here
            // (SQLite sees within-transaction writes), so duplicate pairs within
            // one call count the second occurrence as skipped.
            let existing: Option<i64> = check_stmt
                .query_row(params![hash_id, tag_id], |r| r.get(0))
                .optional()?;
            if existing == Some(0) {
                skipped += 1;
                continue;
            }

            // Sign the submission in-process. We do NOT call verify() per row
            // because submissions are produced here by account.sign — the
            // signature is known-good. Signatures are still stored in full so
            // the audit log remains independently verifiable.
            // submissions table keeps hex hash / text tag — N4 (not interned).
            let sub = account.sign(Op::Add, &hash, &tag);

            next_seq += 1;
            sub_stmt.execute(params![
                sub.op.as_str(),
                hash_str,
                tag_str,
                sub.author,
                sub.signature,
                ts
            ])?;
            map_stmt.execute(params![hash_id, tag_id, 0i64, next_seq])?;
            inserted += 1;
        }

        // Drop prepared statements before commit (they borrow the transaction).
        drop(sel_hash_stmt);
        drop(ins_hash_stmt);
        drop(sel_tag_stmt);
        drop(ins_tag_stmt);
        drop(check_stmt);
        drop(sub_stmt);
        drop(map_stmt);

        tx.commit()?;
        tracing::debug!(
            target: "db",
            author = %pubkey,
            inserted,
            skipped,
            "seed_mappings complete"
        );
        Ok(SeedSummary {
            inserted,
            skipped,
            total: inserted + skipped,
        })
    }

    /// Shared per-row mapping loop, parametrised by hash-resolution strategy.
    ///
    /// `Indexed` — normal path: `repo_hashes_hash_unique` must be present;
    /// resolves each hash with cache → `SELECT` → `INSERT ON CONFLICT`.
    /// Clears `caches.hash` at entry (per-chunk bound).
    ///
    /// `DeferredAppend` — fresh-seed fast path: unique index is ABSENT; appends
    /// each genuinely-new hash with `INSERT … RETURNING id` (no `ON CONFLICT`,
    /// no `SELECT`); uses `caches.hash_carry` to skip re-inserts for same-hash
    /// runs across chunk boundaries. Does NOT clear `caches.hash` at entry.
    ///
    /// The tag branch, `repo_mappings` upsert, corrupt-hash skip (I5), and seq
    /// threading (I1/I2) are identical in both modes.
    fn apply_mappings_impl<I>(
        &self,
        items: I,
        caches: &mut InternCaches,
        next_seq: &mut i64,
        mode: HashResolve,
        ckpt: Option<&SeedCheckpoint>,
    ) -> Result<BulkApplyStats>
    where
        I: IntoIterator<Item = (String, String, bool)>,
    {
        // Indexed: clear per-chunk hash cache. DeferredAppend: keep carry intact.
        if matches!(mode, HashResolve::Indexed) {
            caches.hash.clear();
        }

        let tx = self.conn.unchecked_transaction()?;
        let mut stats = BulkApplyStats::default();
        {
            // Mode-specific hash statements — only prepare what the path uses.
            // This avoids preparing `ON CONFLICT(hash)` when the index is absent
            // (SQLite validates conflict-targets at prepare time; with the index
            // dropped, that prepare would fail).
            let mut sel_hash_stmt = if matches!(mode, HashResolve::Indexed) {
                Some(tx.prepare("SELECT id FROM repo_hashes WHERE hash = ?1")?)
            } else {
                None
            };
            let mut intern_hash_indexed_stmt = if matches!(mode, HashResolve::Indexed) {
                Some(tx.prepare(
                    "INSERT INTO repo_hashes(hash) VALUES(?1) \
                     ON CONFLICT(hash) DO UPDATE SET id = id RETURNING id",
                )?)
            } else {
                None
            };
            let mut intern_hash_deferred_stmt = if matches!(mode, HashResolve::DeferredAppend) {
                Some(tx.prepare("INSERT INTO repo_hashes(hash) VALUES(?1) RETURNING id")?)
            } else {
                None
            };
            let mut sel_tag_stmt = tx.prepare("SELECT id FROM repo_tags WHERE tag = ?1")?;
            let mut intern_tag_stmt = tx.prepare(
                "INSERT INTO repo_tags(tag) VALUES(?1) \
                 ON CONFLICT(tag) DO UPDATE SET id = id RETURNING id",
            )?;
            let mut map_stmt = tx.prepare(
                "INSERT INTO repo_mappings(hash_id, tag_id, status, seq) VALUES(?1, ?2, ?3, ?4) \
                 ON CONFLICT(hash_id, tag_id) DO UPDATE SET \
                     status = excluded.status, seq = excluded.seq \
                     WHERE repo_mappings.status IS NOT excluded.status",
            )?;

            for (hash_hex, tag, is_delete) in items {
                // I5: decode and validate — skip corrupt hashes with a warning.
                let hash_bytes: [u8; 32] =
                    match hex::decode(&hash_hex).ok().and_then(|v| v.try_into().ok()) {
                        Some(b) => b,
                        None => {
                            tracing::warn!(
                                target: "db",
                                hash = %hash_hex,
                                "skipping non-hex/non-32-byte hash in bulk apply"
                            );
                            continue;
                        }
                    };

                // Hash resolution — mode-specific.
                let hash_id = match mode {
                    HashResolve::Indexed => {
                        // Cache → read-only SELECT → INSERT ON CONFLICT.
                        if let Some(&id) = caches.hash.get(&hash_bytes) {
                            id
                        } else {
                            let sel = sel_hash_stmt.as_mut().unwrap();
                            let ins = intern_hash_indexed_stmt.as_mut().unwrap();
                            let id: i64 = match sel
                                .query_row(params![hash_bytes.as_slice()], |r| r.get(0))
                                .optional()?
                            {
                                Some(id) => id,
                                None => {
                                    ins.query_row(params![hash_bytes.as_slice()], |r| r.get(0))?
                                }
                            };
                            caches.hash.insert(hash_bytes, id);
                            id
                        }
                    }
                    HashResolve::DeferredAppend => {
                        // Carry check → append INSERT (no SELECT, no ON CONFLICT).
                        // The carry holds the last (hash, id) resolved; since the
                        // current pass is strictly hash-major, a run of same-hash
                        // rows arrives contiguously and the carry hits on every
                        // row after the first, avoiding repeated inserts.
                        let ins = intern_hash_deferred_stmt.as_mut().unwrap();
                        if let Some((carry_hash, carry_id)) = caches.hash_carry {
                            if carry_hash == hash_bytes {
                                carry_id
                            } else {
                                let id: i64 =
                                    ins.query_row(params![hash_bytes.as_slice()], |r| r.get(0))?;
                                caches.hash_carry = Some((hash_bytes, id));
                                id
                            }
                        } else {
                            let id: i64 =
                                ins.query_row(params![hash_bytes.as_slice()], |r| r.get(0))?;
                            caches.hash_carry = Some((hash_bytes, id));
                            id
                        }
                    }
                };

                // Tag resolution — identical for both modes.
                let tag_id = if let Some(&id) = caches.tag.get(&tag) {
                    id
                } else {
                    let id: i64 = match sel_tag_stmt
                        .query_row(params![tag], |r| r.get(0))
                        .optional()?
                    {
                        Some(id) => id,
                        None => intern_tag_stmt.query_row(params![tag], |r| r.get(0))?,
                    };
                    caches.tag.insert(tag.clone(), id);
                    id
                };

                // Upsert repo_mappings — I1/I2.
                *next_seq += 1;
                let status: i64 = is_delete as i64; // 0 = current, 1 = deleted
                let n = map_stmt.execute(params![hash_id, tag_id, status, *next_seq])?;
                if n == 0 {
                    *next_seq -= 1; // echo: DO UPDATE WHERE was false — reclaim seq
                } else if is_delete {
                    stats.deleted += 1;
                } else {
                    stats.applied += 1;
                }
            }
        }
        // I1: persist the checkpoint in the SAME transaction as the chunk it
        // describes, so a committed checkpoint always implies its rows are durable
        // (and vice-versa). Only the two Format-A deferred passes pass Some (I7).
        if let Some(ck) = ckpt {
            let json = serde_json::to_string(ck).context("serialising seed checkpoint")?;
            tx.execute(
                "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('seed_ckpt', ?1)",
                params![json],
            )?;
        }
        tx.commit()?;
        tracing::debug!(
            target: "db",
            applied = stats.applied,
            deleted = stats.deleted,
            "apply_mappings_bulk_cached complete"
        );
        Ok(stats)
    }

    /// Bulk-apply `(hash_hex, tag, is_delete)` mappings with caller-owned
    /// [`InternCaches`] and a threaded sequence counter.
    ///
    /// This is the hot inner path for the bridge seed. Callers supply:
    ///
    /// - `caches` — intern maps that persist across chunk calls (`caches.tag`
    ///   is seed-lifetime; `caches.hash` is cleared at entry to bound RAM).
    /// - `next_seq` — carries the highest assigned `seq` across chunks so each
    ///   chunk avoids a `MAX(seq)` probe. The seed reads `MAX(seq)` once at
    ///   phase start (via `mapping_cursor()`) and threads it across all chunks.
    ///
    /// The loop is verbatim from the original `apply_mappings_bulk`:
    /// read-first resolver (I5), corrupt-hash warn+skip, no-op guard + seq
    /// reclaim (I1/I2). The sync path keeps calling the wrapper below, which
    /// is byte-identical in observable behaviour (I6).
    ///
    /// **Trusted local ingest only** — same contract as `apply_mappings_bulk`.
    ///
    /// **On `Err`, discard `caches` and `next_seq`.** A failed chunk rolls the
    /// transaction back, but ids interned during it stay in the caches — reusing
    /// them after a rollback would write dangling ids into `repo_mappings`.
    /// The seed aborts wholesale on the first flush error, which satisfies this.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn apply_mappings_bulk_cached<I>(
        &self,
        items: I,
        caches: &mut InternCaches,
        next_seq: &mut i64,
        ckpt: Option<&SeedCheckpoint>,
    ) -> Result<BulkApplyStats>
    where
        I: IntoIterator<Item = (String, String, bool)>,
    {
        self.apply_mappings_impl(items, caches, next_seq, HashResolve::Indexed, ckpt)
    }

    /// Deferred-index variant of [`Self::apply_mappings_bulk_cached`] for the
    /// current pass of a fresh hash_id-ordered (per-hash clustered) seed.
    ///
    /// Differs from [`Self::apply_mappings_bulk_cached`] in the hash branch
    /// only: uses `caches.hash_carry` (cross-chunk carry) instead of the
    /// per-chunk `caches.hash` map; resolves each genuinely-new hash with a
    /// plain `INSERT INTO repo_hashes(hash) VALUES(?) RETURNING id` (no
    /// `ON CONFLICT`, no `SELECT`) — valid because the unique index on
    /// `repo_hashes(hash)` has been dropped before this call and the source
    /// guarantees each hash is first-seen in ascending order.
    ///
    /// The carry (`caches.hash_carry`) is NOT cleared at entry and survives
    /// across chunk calls — it is the minimal state needed to avoid redundant
    /// inserts when a hash's mapping-run straddles a chunk boundary.
    ///
    /// Tag branch, `repo_mappings` upsert, corrupt-hash skip (I5), and seq
    /// threading (I1/I2) are identical to the indexed path (I6).
    ///
    /// **Uniqueness guarantee:** the `build_hash_unique_index` call at the end
    /// of the current pass serves as the fail-loud correctness backstop (I8):
    /// if any hash was appended twice, `CREATE UNIQUE INDEX` fails loudly.
    ///
    /// **Trusted local ingest only** — must only be called during a fresh seed.
    ///
    /// # Errors
    /// Returns an error if any statement fails (including if a duplicate hash
    /// is inserted — detected later by `build_hash_unique_index`).
    /// **On `Err`, discard `caches` and `next_seq` (including `hash_carry`).**
    /// A failed chunk rolls the transaction back, but ids interned during it
    /// stay in the caches — reusing them on a retry would map to rolled-back
    /// rows. The seed must treat any error here as fatal and abort.
    pub fn apply_current_mappings_deferred<I>(
        &self,
        items: I,
        caches: &mut InternCaches,
        next_seq: &mut i64,
        ckpt: Option<&SeedCheckpoint>,
    ) -> Result<BulkApplyStats>
    where
        I: IntoIterator<Item = (String, String, bool)>,
    {
        self.apply_mappings_impl(items, caches, next_seq, HashResolve::DeferredAppend, ckpt)
    }

    /// Bulk-apply raw `(hash_hex, tag, is_delete)` mappings in ONE transaction,
    /// bypassing signed submissions. Each row upserts `repo_mappings` with a
    /// monotonically increasing `seq`, so a client's incremental cursor advances
    /// exactly as with signed submits — but NO `submissions` audit rows are
    /// written and NO signature is verified.
    ///
    /// **Trusted local ingest only** (the `naiad-bridge` seed/sync path). Never
    /// expose this on an HTTP handler: it is the deliberate escape hatch for an
    /// operator mirroring an external corpus (the Hydrus PTR) into a repo store
    /// they own. The caller is responsible for chunking (e.g. 50k rows/call) so
    /// each transaction stays bounded; this method itself does the whole batch
    /// in a single transaction.
    ///
    /// This is a thin delegating wrapper around [`Self::apply_mappings_bulk_cached`]
    /// that creates transient caches and reads `MAX(seq)` per call, preserving
    /// the original signature and observable behaviour byte-for-byte (I6).
    /// The bridge sync path keeps calling this wrapper; `seed.rs` calls the
    /// cached variant directly.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn apply_mappings_bulk<I>(&self, items: I) -> Result<BulkApplyStats>
    where
        I: IntoIterator<Item = (String, String, bool)>,
    {
        let mut caches = InternCaches::default();
        let mut seq: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM repo_mappings", [], |r| {
                    r.get(0)
                })?;
        self.apply_mappings_bulk_cached(items, &mut caches, &mut seq, None)
    }

    /// The whole store as `hash → [OriginTag]`, current mappings only.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn snapshot(&self) -> Result<BTreeMap<String, Vec<OriginTag>>> {
        let mut stmt = self.conn.prepare(
            "SELECT h.hash, t.tag, m.origin
             FROM   repo_mappings m
             JOIN   repo_hashes h ON h.id = m.hash_id
             JOIN   repo_tags   t ON t.id = m.tag_id
             WHERE  m.status = 0
             ORDER  BY h.hash, t.tag",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out: BTreeMap<String, Vec<OriginTag>> = BTreeMap::new();
        for row in rows {
            let (hash_blob, tag, origin) = row?;
            let hash = hex::encode(&hash_blob);
            out.entry(hash).or_default().push(OriginTag { tag, origin });
        }
        tracing::trace!(target: "db", hashes = out.len(), "snapshot read");
        Ok(out)
    }

    /// The `current` mappings whose hash is in `[lo, hi)`, as `hash → [OriginTag]`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn bucket(
        &self,
        lo: &str,
        hi: &str,
        budget: usize,
    ) -> Result<(BTreeMap<String, Vec<OriginTag>>, usize)> {
        let lo_blob = lo_bound(lo);
        let hi_blob = hi_bound(hi);
        let mut stmt = self.conn.prepare(
            "SELECT h.hash, t.tag, m.origin
             FROM   repo_hashes h
             JOIN   repo_mappings m ON m.hash_id = h.id
             JOIN   repo_tags     t ON t.id      = m.tag_id
             WHERE  m.status = 0 AND h.hash >= ?1 AND h.hash < ?2
             ORDER  BY h.hash, t.tag",
        )?;
        let mut rows = stmt.query(params![lo_blob, hi_blob])?;
        let mut out: BTreeMap<String, Vec<OriginTag>> = BTreeMap::new();
        // Start spend at the response envelope: `{"version":8,"cursor":N,"tags":{…}}`
        // is bounded by RESPONSE_ENVELOPE_OVERHEAD regardless of row count (#166).
        let mut spent: usize = RESPONSE_ENVELOPE_OVERHEAD;
        while let Some(r) = rows.next()? {
            let hash_blob: Vec<u8> = r.get(0)?;
            let hash = hex::encode(&hash_blob); // reconstruct 64-hex for the wire (§5.4)
            let tag: String = r.get(1)?;
            let origin: Option<String> = r.get(2)?;
            // Charge origin framing + JSON-escaped value: `,"origin":"<value>"`
            // = 12 framing bytes (`,` + `"origin"` + `:` + two quotes) plus the
            // JSON-escaped value length. None costs 0 (field omitted by serde
            // skip_serializing_if). Uses json_escaped_len so quotes/backslashes
            // in the origin are not undercounted (#166).
            let origin_cost = origin.as_deref().map_or(0, |o| json_escaped_len(o) + 12);
            // hash.len() == 64 (hex-encoded) — preserves budget accounting (§5.4).
            spent = spent.saturating_add(approx_row_cost(hash.len(), tag.len() + origin_cost));
            if spent > budget {
                return Err(BudgetExceeded { budget }.into());
            }
            out.entry(hash).or_default().push(OriginTag { tag, origin });
        }
        tracing::trace!(target: "db", hashes = out.len(), bytes = spent, "bucket read");
        Ok((out, spent))
    }

    /// Mapping rows in `[lo, hi)` with `seq > since`, as plain `DeltaMapping`
    /// tuples — the v8 client/server delta shape (includes origin).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn bucket_delta(
        &self,
        lo: &str,
        hi: &str,
        since: u64,
        budget: usize,
    ) -> Result<(Vec<DeltaMapping>, usize)> {
        let since_i = i64::try_from(since).unwrap_or(i64::MAX);
        let lo_blob = lo_bound(lo);
        let hi_blob = hi_bound(hi);
        let mut stmt = self.conn.prepare(
            "SELECT h.hash, t.tag, m.status, m.seq, m.origin
             FROM   repo_hashes h
             JOIN   repo_mappings m ON m.hash_id = h.id
             JOIN   repo_tags     t ON t.id      = m.tag_id
             WHERE  h.hash >= ?1 AND h.hash < ?2 AND m.seq > ?3
             ORDER  BY m.seq, h.hash, t.tag",
        )?;
        let mut rows = stmt.query(params![lo_blob, hi_blob, since_i])?;
        let mut out: Vec<DeltaMapping> = Vec::new();
        // Start spend at the response envelope: `{"version":8,"cursor":N,"changes":[…]}`
        // is bounded by RESPONSE_ENVELOPE_OVERHEAD regardless of row count (#166).
        let mut spent: usize = RESPONSE_ENVELOPE_OVERHEAD;
        while let Some(r) = rows.next()? {
            let hash_blob: Vec<u8> = r.get(0)?;
            let hash = hex::encode(&hash_blob); // reconstruct 64-hex for the wire (§5.4)
            let tag: String = r.get(1)?;
            let status_int: i64 = r.get(2)?;
            let seq_i: i64 = r.get(3)?;
            let origin: Option<String> = r.get(4)?;
            // The status/seq fields are folded into BUCKET_ROW_OVERHEAD (derived from
            // the delta framing using the maximum u64 seq width; see
            // naiad_core::BUCKET_ROW_OVERHEAD). Charge origin framing + JSON-escaped
            // value: same 12 framing bytes as bucket() (#166).
            let origin_cost = origin.as_deref().map_or(0, |o| json_escaped_len(o) + 12);
            // hash.len() == 64 (hex-encoded) — preserves budget accounting (§5.4).
            spent = spent.saturating_add(approx_row_cost(hash.len(), tag.len() + origin_cost));
            if spent > budget {
                return Err(BudgetExceeded { budget }.into());
            }
            out.push(DeltaMapping {
                hash,
                tag,
                status: if status_int == 1 {
                    MappingStatus::Deleted
                } else {
                    MappingStatus::Current
                },
                seq: seq_i as u64,
                origin,
            });
        }
        tracing::trace!(target: "db", rows = out.len(), since, bytes = spent, "bucket delta read");
        Ok((out, spent))
    }

    /// How many distinct hashes have at least one `current` mapping — the input
    /// to prefix auto-sizing.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn distinct_hash_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT hash_id) FROM repo_mappings WHERE status = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// How many distinct tags have at least one `current` mapping. Feeds the
    /// stats subsystem's `tags_stored` gauge (#235).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn distinct_tag_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT tag_id) FROM repo_mappings WHERE status = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// The count of `current` (non-deleted) rows in `repo_mappings`.
    ///
    /// More informative than the seq high-watermark for a status readout because
    /// it excludes tombstoned mappings: it answers "how many active tag→hash
    /// pairs does this repo hold right now?"
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn current_mapping_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM repo_mappings WHERE status = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Streaming `(count, blake3)` digest of current `(hash, tag)` pairs in a
    /// hash band, for the mirror parity audit (#184).  Groups rows by hash and
    /// sorts each hash's tags in Rust so the ordering is byte-identical to the
    /// Hydrus side regardless of SQLite collation.  `prefix_bits == 0` audits
    /// the full hash range.
    ///
    /// # Origin filter (#198)
    /// Only mirror-origin rows are counted/digested: the query carries
    /// `AND m.origin IS NULL`.  Convention (see 0002_submission_origin): a NULL
    /// origin means "seeded from / replayed as mirror PTR data", while a
    /// non-NULL origin marks a locally-authored signed submission.  On a hybrid
    /// node (#194: local submissions replayed on top of seeded PTR rows) the
    /// local rows have no counterpart in the Hydrus snapshot, so a naive
    /// whole-store digest would report them as false mismatches.  Excluding them
    /// here restores parity against a pure-PTR snapshot.  This method is only
    /// used by the mirror parity audit, so the filter is unconditional: on a
    /// pure mirror every current row has NULL origin, so behaviour is unchanged.
    ///
    /// Known limitation: origin is a single value per `(hash, tag)` row, so a
    /// local signed Add that *re-asserts* a pair also present in the snapshot
    /// stamps that row with a non-NULL origin and drops it from the audited set.
    /// The audit then FAILs with the store count one short.  This is inherent to
    /// the origin column being a single last-writer-wins value; it is documented
    /// rather than worked around (see the `audit_band_digest_reassert…` test).
    ///
    /// # Errors
    /// Returns an error if `lo_hex` cannot be parsed or if a query fails.
    pub fn audit_band_digest(&self, lo_hex: &str, prefix_bits: u32) -> Result<(u64, [u8; 32])> {
        use naiad_core::{PairDigest, bucket_key, bucket_upper};
        let lo: Hash = lo_hex
            .parse()
            .with_context(|| format!("audit_band_digest: bad lo_hex {lo_hex:?}"))?;
        let bits = prefix_bits.min(256);
        let lo_blob = hex::decode(bucket_key(&lo, bits))?;
        let hi_hex = bucket_upper(&lo, bits);
        const SENTINEL: &[u8] = &[0xff_u8; 33];
        let hi_blob: Vec<u8> = if hi_hex == "g" {
            SENTINEL.to_vec()
        } else {
            hex::decode(hi_hex)?
        };

        let mut stmt = self.conn.prepare(
            "SELECT h.hash, t.tag
             FROM   repo_hashes h
             JOIN   repo_mappings m ON m.hash_id = h.id
             JOIN   repo_tags     t ON t.id      = m.tag_id
             WHERE  m.status = 0 AND m.origin IS NULL
                    AND h.hash >= ?1 AND h.hash < ?2
             ORDER  BY h.hash",
        )?;
        let mut rows = stmt.query(params![lo_blob, hi_blob])?;

        let mut digest = PairDigest::new();
        let mut cur_hash: Option<[u8; 32]> = None;
        let mut cur_tags: Vec<String> = Vec::new();
        while let Some(r) = rows.next()? {
            let hash_blob: Vec<u8> = r.get(0)?;
            let tag: String = r.get(1)?;
            let hash: [u8; 32] = hash_blob
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("audit_band_digest: hash blob is not 32 bytes"))?;
            match cur_hash {
                Some(h) if h == hash => cur_tags.push(tag),
                _ => {
                    flush_hash(&mut digest, &cur_hash, &mut cur_tags);
                    cur_hash = Some(hash);
                    cur_tags.push(tag);
                }
            }
        }
        flush_hash(&mut digest, &cur_hash, &mut cur_tags);
        Ok(digest.finalize())
    }

    /// The repo's mapping high-watermark: `MAX(seq)` over all repo_mappings
    /// rows, or 0 for an empty repo. The cursor a client advances to.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn mapping_cursor(&self) -> Result<u64> {
        let c: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM repo_mappings", [], |r| {
                    r.get(0)
                })?;
        Ok(c as u64)
    }

    // ── Store generation (repo_meta) ─────────────────────────────────────────

    /// Read the current store-generation id from `repo_meta`.
    ///
    /// Returns `None` when the key is absent — a store that predates this
    /// feature or has never been seeded. The client falls back to the
    /// backwards-cursor guard in that case.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn store_generation(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM repo_meta WHERE key = 'store_generation'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Generate a new random store-generation id, persist it, and return it.
    ///
    /// Uses 16 random bytes from `getrandom`, hex-encoded to 32 lowercase
    /// chars. `INSERT OR REPLACE` so calling this on an existing store replaces
    /// the old value atomically — exactly what `bridge seed --rebuild` needs.
    ///
    /// # Errors
    /// Returns an error if the RNG or the DB statement fails.
    pub fn mint_store_generation(&self) -> Result<String> {
        let mut buf = [0u8; 16];
        getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("getrandom failed: {e}"))?;
        let generation = hex::encode(buf);
        self.conn.execute(
            "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('store_generation', ?1)",
            params![generation],
        )?;
        tracing::debug!(target: "db", generation = %generation, "mint_store_generation: stored");
        Ok(generation)
    }

    /// Read the persisted distinct-hash count from `repo_meta` (#202).
    ///
    /// `None` when the key is absent (a store that predates this feature or has
    /// never been seeded) OR when the stored text does not parse as a u64 (a
    /// corrupt value must not break `GET /repo/caps` — callers fall back). A
    /// non-numeric value is logged at `warn` and treated as absent.
    ///
    /// # Errors
    /// Returns an error only if the query itself fails.
    pub fn read_distinct_hash_count(&self) -> Result<Option<u64>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM repo_meta WHERE key = 'distinct_hash_count'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match raw {
            None => None,
            Some(s) => match s.trim().parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    tracing::warn!(
                        target: "db",
                        value = %s,
                        "repo_meta distinct_hash_count is not a u64; treating as absent"
                    );
                    None
                }
            },
        })
    }

    /// Persist the distinct-hash count into `repo_meta` (#202). `INSERT OR REPLACE`
    /// so calling on an existing store overwrites atomically.
    ///
    /// # Errors
    /// Returns an error if the DB statement fails.
    pub fn write_distinct_hash_count(&self, n: u64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('distinct_hash_count', ?1)",
            params![n.to_string()],
        )?;
        tracing::debug!(target: "db", count = n, "write_distinct_hash_count: stored");
        Ok(())
    }

    /// Read + parse the phase-1 seed checkpoint, if present.
    ///
    /// `Ok(None)` when the key is absent (fresh/legacy store). A row whose JSON
    /// fails to parse is a **hard error** — a corrupt checkpoint governs destructive
    /// control flow and must stop the seed, not cause a silent restart-from-scratch.
    /// (Contrast `read_distinct_hash_count`, which tolerates garbage because a bad
    /// count must not break a serve request.)
    ///
    /// # Errors
    /// Returns an error if the query fails or the stored JSON does not parse.
    pub fn read_seed_checkpoint(&self) -> Result<Option<SeedCheckpoint>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM repo_meta WHERE key = 'seed_ckpt'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(s) => {
                let ck: SeedCheckpoint = serde_json::from_str(&s)
                    .context("parsing seed_ckpt repo_meta row (corrupt checkpoint)")?;
                Ok(Some(ck))
            }
        }
    }

    /// Clear the phase-1 seed checkpoint. Idempotent; a no-op when absent, so the
    /// Format-B path and the completion path call it unconditionally.
    ///
    /// # Errors
    /// Returns an error if the DB statement fails.
    pub fn clear_seed_checkpoint(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM repo_meta WHERE key = 'seed_ckpt'", [])?;
        tracing::debug!(target: "db", "clear_seed_checkpoint");
        Ok(())
    }

    /// Write (upsert) the checkpoint OUTSIDE the chunk transaction. Used by tests
    /// and any explicit-marker path; the hot path writes it inside
    /// `apply_mappings_impl` instead. `INSERT OR REPLACE`.
    ///
    /// # Errors
    /// Returns an error if serialisation or the DB statement fails.
    pub fn write_seed_checkpoint(&self, ckpt: &SeedCheckpoint) -> Result<()> {
        let json = serde_json::to_string(ckpt).context("serialising seed checkpoint")?;
        self.conn.execute(
            "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('seed_ckpt', ?1)",
            params![json],
        )?;
        tracing::debug!(target: "db", "write_seed_checkpoint");
        Ok(())
    }

    /// Recompute and persist the distinct-hash count, but ONLY if a count row
    /// already exists (#202). A pre-upgrade store with no row is left untouched —
    /// the serve-side one-shot fallback populates it. Returns the new count, or
    /// `None` when no row was present.
    ///
    /// This runs a full `distinct_hash_count()` scan; call it at most once per
    /// bridge-sync pass (the writer process), never on a serve request path.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn refresh_distinct_hash_count(&self) -> Result<Option<u64>> {
        if self.read_distinct_hash_count()?.is_none() {
            return Ok(None);
        }
        let n = self.distinct_hash_count()?;
        self.write_distinct_hash_count(n)?;
        Ok(Some(n))
    }

    /// Set the `rebuild_in_progress` marker in `repo_meta`.
    ///
    /// Must be called BEFORE `clear_mirrored_mappings` so that a crash during
    /// the rebuild window is detectable on restart. A plain `bridge seed`
    /// (without `--rebuild`) checks this marker and bails with a clear error
    /// if it is set, preventing silent data corruption from a half-finished rebuild.
    ///
    /// # Errors
    /// Returns an error if the DB statement fails.
    pub fn set_rebuild_in_progress(&self) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('rebuild_in_progress', '1')",
            [],
        )?;
        tracing::debug!(target: "db", "set_rebuild_in_progress");
        Ok(())
    }

    /// Clear the `rebuild_in_progress` marker.
    ///
    /// Must be called as the LAST step of `bridge seed --rebuild` (after
    /// `mint_store_generation`). If a crash happens between set and clear the
    /// marker stays set and the operator must re-run `--rebuild`.
    ///
    /// # Errors
    /// Returns an error if the DB statement fails.
    pub fn clear_rebuild_in_progress(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM repo_meta WHERE key = 'rebuild_in_progress'",
            [],
        )?;
        tracing::debug!(target: "db", "clear_rebuild_in_progress");
        Ok(())
    }

    /// Refresh SQLite query-planner statistics (`ANALYZE`) so the bucket join picks
    /// a good plan on a freshly (re-)seeded store (#203). Run once at seed end,
    /// before the generation/count meta writes. Offline/admin only.
    ///
    /// # Errors
    /// Returns an error if the statement fails.
    pub(crate) fn analyze(&self) -> Result<()> {
        let t0 = std::time::Instant::now();
        self.conn.execute_batch("ANALYZE")?;
        tracing::info!(target: "db", elapsed_ms = t0.elapsed().as_millis() as u64, "ANALYZE complete");
        Ok(())
    }

    /// Returns `true` if the `rebuild_in_progress` marker is set in `repo_meta`.
    ///
    /// # Errors
    /// Returns an error if the DB query fails.
    pub fn rebuild_in_progress(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM repo_meta WHERE key = 'rebuild_in_progress'",
            [],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Clear the mirrored mapping tables for a rebuild-in-place re-seed.
    ///
    /// Deletes all `repo_mappings` and `repo_hashes` rows in a single
    /// transaction. The `submissions` table is intentionally NOT touched — it is
    /// the append-only log that is replayed on top after the rebuild.
    ///
    /// # Errors
    /// Returns an error if the transaction fails.
    pub fn clear_mirrored_mappings(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch("DELETE FROM repo_mappings; DELETE FROM repo_hashes;")?;
        tx.commit()?;
        tracing::info!(target: "db", "clear_mirrored_mappings: cleared repo_mappings and repo_hashes");
        Ok(())
    }

    /// Replay the append-only `submissions` log on top of `repo_mappings`.
    ///
    /// Used after `bridge seed --rebuild` to restore local signed submissions
    /// after the PTR-mirror data has been re-seeded. Reads `submissions ORDER
    /// BY rowid` (assertion order) and, for each row, upserts `repo_mappings`
    /// preserving `origin` — reusing the same UPSERT path as `apply_submission`
    /// (§6.2). A later `Remove` correctly clobbers an earlier `Add`.
    ///
    /// Does NOT re-insert into `submissions` (rows already exist there).
    /// Returns the count of submissions processed.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn replay_submissions(&self) -> Result<u64> {
        let tx = self.conn.unchecked_transaction()?;

        // Seed the seq counter once from the current MAX(seq).
        let mut next_seq: i64 =
            tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM repo_mappings", [], |r| {
                r.get(0)
            })?;

        // Collect all submission rows first so we can release the Rows
        // iterator borrow on `tx` before the inter-table write statements.
        let rows: Vec<(String, String, String, Option<String>)> = {
            let mut stmt =
                tx.prepare("SELECT op, hash, tag, origin FROM submissions ORDER BY rowid")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?
        }; // stmt dropped here, releasing its borrow on tx

        let mut sel_hash = tx.prepare("SELECT id FROM repo_hashes WHERE hash = ?1")?;
        let mut ins_hash = tx.prepare("INSERT INTO repo_hashes(hash) VALUES(?1) RETURNING id")?;
        let mut sel_tag = tx.prepare("SELECT id FROM repo_tags WHERE tag = ?1")?;
        let mut ins_tag = tx.prepare("INSERT INTO repo_tags(tag) VALUES(?1) RETURNING id")?;
        // Same UPSERT as the signed-apply path (§apply_submission, ~L681),
        // preserving `origin` so the rebuilder can later filter by origin.
        let mut map_stmt = tx.prepare(
            "INSERT INTO repo_mappings(hash_id, tag_id, status, seq, origin)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(hash_id, tag_id) DO UPDATE SET
                 status = excluded.status, seq = excluded.seq, origin = excluded.origin
                 WHERE repo_mappings.status IS NOT excluded.status
                    OR repo_mappings.origin IS NOT excluded.origin",
        )?;

        let mut count: u64 = 0;
        for (op, hash_hex, tag, origin) in &rows {
            // Decode and validate the hash — skip corrupt entries with a warning.
            let hash_bytes: Vec<u8> = match hex::decode(hash_hex) {
                Ok(b) if b.len() == 32 => b,
                _ => {
                    tracing::warn!(
                        target: "db",
                        hash = %hash_hex,
                        "replay_submissions: skipping invalid hash"
                    );
                    continue;
                }
            };

            let status_int: i64 = match op.as_str() {
                "add" => 0,
                "remove" => 1,
                _ => {
                    tracing::warn!(
                        target: "db",
                        %op,
                        "replay_submissions: skipping unknown op"
                    );
                    continue;
                }
            };

            // Intern hash (read-first resolver — identical to apply_submission).
            let hash_id: i64 = match sel_hash
                .query_row(params![hash_bytes], |r| r.get(0))
                .optional()?
            {
                Some(id) => id,
                None => ins_hash.query_row(params![hash_bytes], |r| r.get(0))?,
            };

            // Intern tag (read-first resolver).
            let tag_id: i64 = match sel_tag.query_row(params![tag], |r| r.get(0)).optional()? {
                Some(id) => id,
                None => ins_tag.query_row(params![tag], |r| r.get(0))?,
            };

            // Upsert with a fresh seq. Reclaim seq on echo (same as apply_mappings_impl).
            next_seq += 1;
            let n = map_stmt.execute(params![hash_id, tag_id, status_int, next_seq, origin])?;
            if n == 0 {
                next_seq -= 1; // echo: DO UPDATE WHERE was false — reclaim seq
            }
            count += 1;
        }

        // Drop prepared statements before commit (they borrow the transaction).
        drop(sel_hash);
        drop(ins_hash);
        drop(sel_tag);
        drop(ins_tag);
        drop(map_stmt);

        tx.commit()?;
        tracing::info!(target: "db", replayed = count, "replay_submissions: complete");
        Ok(count)
    }

    // ── Reports ───────────────────────────────────────────────────────────────

    /// File a report against `(hash, tag)`. Rejects banned reporters; unknown
    /// reporters (not yet in `accounts`) are allowed.
    ///
    /// # Errors
    /// Returns an error if the reporter is banned or a statement fails.
    pub fn insert_report(
        &self,
        hash: &str,
        tag: &str,
        reporter: &str,
        note: Option<&str>,
        created_at: i64,
    ) -> Result<()> {
        // Banned accounts cannot file reports.
        let banned: Option<i64> = self
            .conn
            .query_row(
                "SELECT banned FROM accounts WHERE pubkey = ?1",
                params![reporter],
                |r| r.get(0),
            )
            .optional()?;
        if banned == Some(1) {
            anyhow::bail!("banned account cannot file reports");
        }
        self.conn.execute(
            "INSERT INTO reports(hash, tag, reporter_pubkey, note, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![hash, tag, reporter, note, created_at],
        )?;
        tracing::debug!(target: "db", reporter = %reporter, "inserted report");
        Ok(())
    }

    /// Every open report, oldest first.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn open_reports(&self) -> Result<Vec<ReportRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, hash, tag, reporter_pubkey, note, created_at, status
             FROM reports WHERE status = 'open' ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let id: i64 = r.get(0)?;
                Ok(ReportRow {
                    id: id as u64,
                    hash: r.get(1)?,
                    tag: r.get(2)?,
                    reporter: r.get(3)?,
                    note: r.get(4)?,
                    created_at: r.get(5)?,
                    status: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Close the report with `id` (sets `status = 'closed'`).
    ///
    /// # Errors
    /// Returns an error if no such report exists or the statement fails.
    pub fn close_report(&self, id: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE reports SET status = 'closed' WHERE id = ?1",
            params![id],
        )?;
        if n == 0 {
            anyhow::bail!("close_report: report id {id} not found");
        }
        Ok(())
    }

    /// Hard-delete a `(hash, tag)` mapping and auto-close all open reports for
    /// that pair (spec §3: "Moderator closes by deleting the mapping, banning,
    /// or dismissing"). Both writes are wrapped in a transaction so they land
    /// atomically: the mapping is flipped to `deleted` with a bumped seq and
    /// all open reports for that pair are closed. Zero rows on the reports side
    /// is fine (deleting an unreported mapping); the not-found bail is keyed
    /// only on the mapping row.
    ///
    /// # Errors
    /// Returns an error if the mapping does not exist or any statement fails.
    pub fn moderator_delete_mapping(&self, hash: &str, tag: &str) -> Result<()> {
        let hash_bytes = hex::decode(hash)
            .with_context(|| format!("moderator_delete_mapping: invalid hash hex: {hash}"))?;
        anyhow::ensure!(
            hash_bytes.len() == 32,
            "moderator_delete_mapping: hash is not 32 bytes: {hash}"
        );
        let tx = self.conn.unchecked_transaction()?;
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM repo_mappings",
            [],
            |r| r.get(0),
        )?;
        let n = tx.execute(
            "UPDATE repo_mappings SET status = 1, seq = ?1
             WHERE hash_id = (SELECT id FROM repo_hashes WHERE hash = ?2)
               AND tag_id  = (SELECT id FROM repo_tags   WHERE tag  = ?3)",
            params![next_seq, hash_bytes, tag],
        )?;
        if n == 0 {
            anyhow::bail!("moderator_delete_mapping: ({hash}, {tag}) not found");
        }
        // Close all open reports for the deleted mapping (spec §3).
        // reports table keeps hex hash / text tag — N4 (not interned).
        tx.execute(
            "UPDATE reports SET status = 'closed'
             WHERE hash = ?1 AND tag = ?2 AND status = 'open'",
            params![hash, tag],
        )?;
        tx.commit()?;
        tracing::debug!(target: "db", seq = next_seq, "moderator deleted mapping");
        Ok(())
    }

    // ── Relations ─────────────────────────────────────────────────────────────

    /// Apply one verified relation submission. Remove-dominates per
    /// `(kind, from_tag, to_tag, author)`.
    ///
    /// Rejects banned submitters (spec §3). Auto-creates the account on first
    /// submission, consistent with `apply_submission`.
    ///
    /// # Errors
    /// Returns an error if the account is banned, the signature hex is
    /// malformed, or a statement fails.
    pub fn apply_relation(&self, sub: &RelationSubmission) -> Result<()> {
        let ts = now();
        // Auto-create the account if it doesn't exist yet (consistent with
        // apply_submission). This runs outside the upsert so the account exists
        // before the banned check.
        self.conn.execute(
            "INSERT OR IGNORE INTO accounts(pubkey, created_at) VALUES(?1, ?2)",
            params![sub.author, ts],
        )?;
        // Reject banned submitters.
        let banned: i64 = self.conn.query_row(
            "SELECT banned FROM accounts WHERE pubkey = ?1",
            params![sub.author],
            |r| r.get(0),
        )?;
        if banned != 0 {
            anyhow::bail!("account {} is banned", sub.author);
        }
        let status = match sub.op {
            Op::Add => "current",
            Op::Remove => "deleted",
        };
        let signature = hex::decode(&sub.signature)?;
        self.conn.execute(
            "INSERT INTO relations (kind, from_tag, to_tag, author, status, signature, created_at, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                     (SELECT COALESCE(MAX(seq), 0) + 1 FROM relations))
             ON CONFLICT(kind, from_tag, to_tag, author) DO UPDATE SET
                 status = CASE
                     WHEN relations.status = 'deleted' OR excluded.status = 'deleted'
                     THEN 'deleted' ELSE 'current' END,
                 signature = CASE
                     WHEN excluded.status = 'deleted'
                     THEN excluded.signature ELSE relations.signature END,
                 created_at = excluded.created_at,
                 seq = excluded.seq",
            params![
                sub.kind.as_str(),
                sub.from,
                sub.to,
                sub.author,
                status,
                signature,
                now()
            ],
        )?;
        tracing::debug!(target: "db", author = %sub.author, kind = ?sub.kind, op = ?sub.op, "applied relation");
        Ok(())
    }

    /// Authoritatively apply bridge-authored relation rows (#225).
    ///
    /// Unlike [`Self::apply_relation`], this is **last-writer-wins**, not
    /// remove-dominates: the PTR is authoritative ground truth for the bridged
    /// relation graph, so a delete followed by a later re-add of the same edge
    /// must end `current` (remove-dominates would wedge it `deleted` forever,
    /// since every bridged edge shares the single bridge author). This mirrors
    /// how the mapping path already treats the bridge (`apply_mappings_bulk`
    /// flips status both ways).
    ///
    /// All `rows` must share one author (the bridge key) and are applied in wire
    /// order inside a single transaction, keyed on `(kind, from_tag, to_tag,
    /// author)`. The `WHERE relations.status <> excluded.status` guard on the
    /// conflict clause is the idempotency lever: a wire row that restates the
    /// stored status writes nothing and **does not bump `seq`**, so replaying an
    /// already-applied update index does not churn downstream `?since=` cursors
    /// (ADR 0005). Within one index a `(from,to)` seen as add-then-delete nets to
    /// `deleted`, each status-changing step advancing `seq`.
    ///
    /// Returns the number of rows whose stored status actually changed.
    ///
    /// # Errors
    /// Returns an error if a signature hex is malformed or a statement fails. The
    /// account is auto-created (consistent with [`Self::apply_relation`]); a
    /// banned bridge author is not expected but is not special-cased here.
    pub fn apply_bridge_relations(&self, rows: &[RelationSubmission]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let ts = now();
        let tx = self.conn.unchecked_transaction()?;
        // Auto-create the (single) bridge author once, outside the row loop.
        tx.execute(
            "INSERT OR IGNORE INTO accounts(pubkey, created_at) VALUES(?1, ?2)",
            params![rows[0].author, ts],
        )?;
        let mut changed: u64 = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO relations (kind, from_tag, to_tag, author, status, signature, created_at, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                         (SELECT COALESCE(MAX(seq), 0) + 1 FROM relations))
                 ON CONFLICT(kind, from_tag, to_tag, author) DO UPDATE SET
                     status     = excluded.status,
                     signature  = excluded.signature,
                     created_at = excluded.created_at,
                     seq        = (SELECT COALESCE(MAX(seq), 0) + 1 FROM relations)
                 WHERE relations.status <> excluded.status",
            )?;
            for sub in rows {
                let status = match sub.op {
                    Op::Add => "current",
                    Op::Remove => "deleted",
                };
                let signature = hex::decode(&sub.signature)?;
                let n = stmt.execute(params![
                    sub.kind.as_str(),
                    sub.from,
                    sub.to,
                    sub.author,
                    status,
                    signature,
                    ts,
                ])?;
                // Both the fresh INSERT and a status-flipping UPDATE report one
                // affected row; a no-op UPDATE (guard fails) reports zero.
                changed += n as u64;
            }
        }
        tx.commit()?;
        let unchanged = rows.len() as u64 - changed;
        tracing::debug!(
            target: "db",
            author = %rows[0].author,
            applied = changed,
            unchanged,
            "applied bridge relations"
        );
        Ok(changed)
    }

    /// The whole relation graph: current sibling and parent edges, one
    /// deterministic author per edge.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn relations(&self) -> Result<RelationGraph> {
        Ok(RelationGraph {
            version: PROTOCOL_VERSION,
            cursor: self.relation_cursor()?,
            siblings: self.edges_of("sibling")?,
            parents: self.edges_of("parent")?,
        })
    }

    /// The repo's relation high-watermark: `MAX(seq)` over all relations, or 0.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn relation_cursor(&self) -> Result<u64> {
        let c: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM relations", [], |r| {
                    r.get(0)
                })?;
        Ok(c as u64)
    }

    /// Every relation edge with `seq > since`, ordered by `seq` ascending,
    /// tombstones included.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn edges_since(&self, since: u64) -> Result<Vec<DeltaEdge>> {
        let since = i64::try_from(since).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT kind, from_tag, to_tag, author, status, seq FROM relations
             WHERE seq > ?1
             ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![since], |r| {
                let kind: String = r.get(0)?;
                let status: String = r.get(4)?;
                Ok(DeltaEdge {
                    kind: if kind == "parent" {
                        RelKind::Parent
                    } else {
                        RelKind::Sibling
                    },
                    from: r.get(1)?,
                    to: r.get(2)?,
                    author: r.get(3)?,
                    status: if status == "deleted" {
                        EdgeStatus::Deleted
                    } else {
                        EdgeStatus::Current
                    },
                    seq: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tracing::trace!(target: "db", rows = rows.len(), "edges_since read");
        Ok(rows)
    }

    fn edges_of(&self, kind: &str) -> Result<Vec<AuthoredEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_tag, to_tag, MIN(author) FROM relations
             WHERE status = 'current' AND kind = ?1
             GROUP BY from_tag, to_tag
             ORDER BY from_tag, to_tag",
        )?;
        let rows = stmt
            .query_map(params![kind], |r| {
                Ok(AuthoredEdge {
                    from: r.get::<_, String>(0)?,
                    to: r.get::<_, String>(1)?,
                    author: r.get::<_, String>(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Checkpoint the WAL into the main database file.
    ///
    /// Runs `PRAGMA wal_checkpoint(TRUNCATE)` which moves all WAL frames into
    /// the database and, when no readers hold a snapshot, truncates the WAL
    /// file to zero bytes.  Call this on clean shutdown so volume backups start
    /// in a known-good state.
    ///
    /// Returns `true` when the `busy` column is non-zero — meaning at least
    /// one reader held a snapshot and the WAL could not be fully truncated.
    ///
    /// # Errors
    /// Returns an error if the PRAGMA fails.
    pub fn checkpoint_wal(&self) -> Result<bool> {
        // PRAGMA wal_checkpoint(TRUNCATE) returns a single row:
        //   (busy, log, checkpointed)
        // busy == 1  → some reader blocked the checkpoint; WAL not truncated.
        // busy == 0  → all frames copied; WAL truncated to zero bytes.
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        Ok(busy != 0)
    }
}

/// The pull mode to advertise for a repo holding `count` distinct hashes with a
/// crowd floor of `k`: `WholeRepo` below `k`, else `Bucketed`.
#[must_use]
pub fn advise(count: u64, k: u64) -> PullMode {
    if k == 0 || count < k {
        return PullMode::WholeRepo;
    }
    let bits = (count / k).ilog2();
    if bits == 0 {
        PullMode::WholeRepo
    } else {
        PullMode::Bucketed { prefix_bits: bits }
    }
}

/// Flush one hash group into `digest`: sort tags in Rust then feed each pair.
/// Called by [`RepoStore::audit_band_digest`] after every hash transition.
fn flush_hash(
    digest: &mut naiad_core::PairDigest,
    hash: &Option<[u8; 32]>,
    tags: &mut Vec<String>,
) {
    if let Some(h) = hash {
        tags.sort_unstable();
        // Defensive dedup: the mirror's repo_mappings PRIMARY KEY (hash_id, tag_id)
        // already guarantees each normalized tag appears at most once per hash, so
        // this is a no-op in practice. It is kept for symmetry with the Hydrus side
        // so both halves always digest over the same normalized set.
        tags.dedup();
        for t in tags.iter() {
            digest.update(h, t);
        }
    }
    tags.clear();
}

/// Lower bound for a BLOB hash range scan.
///
/// `bucket_key` always emits full 64-char lowercase hex → decodes to exactly 32
/// bytes. Any non-hex / wrong-length value (never produced in practice) degrades
/// to the empty blob, which memcmp-sorts before every hash → open lower bound.
fn lo_bound(s: &str) -> Vec<u8> {
    hex::decode(s)
        .ok()
        .filter(|b| b.len() == 32)
        .unwrap_or_default()
}

/// Upper bound for a BLOB hash range scan.
///
/// A valid 64-hex value → its 32 bytes (exclusive via `< hi`).
/// The `"g"` / `"gg"` sentinel (and any non-32-byte value) → a 33-byte all-0xFF
/// blob, which memcmp-sorts strictly after every 32-byte hash → unbounded upper.
fn hi_bound(s: &str) -> Vec<u8> {
    hex::decode(s)
        .ok()
        .filter(|b| b.len() == 32)
        .unwrap_or_else(|| vec![0xFFu8; 33])
}

/// Seconds since the Unix epoch.
pub(crate) fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_core::{Tag, hash_bytes};
    use naiad_netproto::{Account, MappingStatus, Op, RelKind};

    fn add(store: &RepoStore, acct: &Account, hash: &naiad_core::Hash, tag: &str) {
        let sub = acct.sign(Op::Add, hash, &Tag::parse(tag).unwrap());
        store.apply_submission(&sub).unwrap();
    }

    fn remove_sub(store: &RepoStore, acct: &Account, hash: &naiad_core::Hash, tag: &str) {
        let sub = acct.sign(Op::Remove, hash, &Tag::parse(tag).unwrap());
        store.apply_submission(&sub).unwrap();
    }

    fn add_rel(store: &RepoStore, acct: &Account, kind: RelKind, from: &str, to: &str) {
        let sub = acct.sign_relation(
            Op::Add,
            kind,
            &Tag::parse(from).unwrap(),
            &Tag::parse(to).unwrap(),
        );
        store.apply_relation(&sub).unwrap();
    }

    // ── AC 1: submit auto-creates account ─────────────────────────────────────

    #[test]
    fn submit_auto_creates_account() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        assert!(
            store.account(&acct.public_hex()).unwrap().is_none(),
            "account absent before first submit"
        );
        add(&store, &acct, &h, "character:samus");
        let row = store
            .account(&acct.public_hex())
            .unwrap()
            .expect("account created on first submit");
        assert_eq!(row.pubkey, acct.public_hex());
        assert_eq!(row.role, "contributor");
        assert!(!row.banned);
    }

    // ── AC 2: banned key rejected on submit ───────────────────────────────────

    #[test]
    fn banned_key_rejected_on_submit() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        // First submit creates the account.
        add(&store, &acct, &h, "character:samus");
        // Ban the account.
        store.set_banned(&acct.public_hex(), true).unwrap();
        // A subsequent submit must fail.
        let sub = acct.sign(Op::Add, &h, &Tag::parse("series:metroid").unwrap());
        assert!(
            store.apply_submission(&sub).is_err(),
            "banned key must be rejected on submit"
        );
    }

    // ── AC 2: banned key rejected on report ───────────────────────────────────

    #[test]
    fn banned_key_rejected_on_report() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        // Create and ban the account.
        add(&store, &acct, &h, "character:samus");
        store.set_banned(&acct.public_hex(), true).unwrap();
        // Filing a report must fail.
        assert!(
            store
                .insert_report(
                    &h.to_hex(),
                    "character:samus",
                    &acct.public_hex(),
                    None,
                    now()
                )
                .is_err(),
            "banned account cannot file reports"
        );
    }

    // ── AC 3: promote → is_moderator true ─────────────────────────────────────

    #[test]
    fn promote_to_moderator() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        add(&store, &acct, &h, "a:x");
        assert!(
            !store.is_moderator(&acct.public_hex()).unwrap(),
            "contributor is not a moderator"
        );
        store.set_role(&acct.public_hex(), "moderator").unwrap();
        assert!(
            store.is_moderator(&acct.public_hex()).unwrap(),
            "set_role promotes to moderator"
        );
        // is_moderator must return false after banning even if role is moderator.
        store.set_banned(&acct.public_hex(), true).unwrap();
        assert!(
            !store.is_moderator(&acct.public_hex()).unwrap(),
            "banned moderator is not active"
        );
    }

    // ── AC 4: report insert / list-open / close ───────────────────────────────

    #[test]
    fn report_lifecycle() {
        let store = RepoStore::open_in_memory().unwrap();
        let reporter = Account::generate();
        let h = hash_bytes(b"file");
        let ts = now();
        // Unknown reporter (not yet in accounts) is allowed.
        store
            .insert_report(
                &h.to_hex(),
                "character:samus",
                &reporter.public_hex(),
                Some("spam"),
                ts,
            )
            .unwrap();
        let open = store.open_reports().unwrap();
        assert_eq!(open.len(), 1, "one open report");
        assert_eq!(open[0].hash, h.to_hex());
        assert_eq!(open[0].tag, "character:samus");
        assert_eq!(open[0].reporter, reporter.public_hex());
        assert_eq!(open[0].note.as_deref(), Some("spam"));
        assert_eq!(open[0].status, "open");
        let id = open[0].id as i64;
        store.close_report(id).unwrap();
        assert!(
            store.open_reports().unwrap().is_empty(),
            "closed report no longer open"
        );
    }

    // ── AC 5: moderator delete flips status + bumps seq ───────────────────────

    #[test]
    fn moderator_delete_visible_in_bucket_delta() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = naiad_core::Hash::from_bytes([0x10; 32]);
        let lo = "10".to_string() + &"00".repeat(31);
        let hi = "20".to_string() + &"00".repeat(31);
        add(&store, &acct, &h, "character:samus");
        let cursor = store.mapping_cursor().unwrap();
        assert_eq!(cursor, 1);
        store
            .moderator_delete_mapping(&h.to_hex(), "character:samus")
            .unwrap();
        let delta = store.bucket_delta(&lo, &hi, cursor, usize::MAX).unwrap().0;
        assert_eq!(delta.len(), 1, "one changed mapping");
        assert_eq!(delta[0].hash, h.to_hex());
        assert_eq!(delta[0].tag, "character:samus");
        assert_eq!(delta[0].status, MappingStatus::Deleted);
        assert!(
            delta[0].seq > cursor,
            "moderator delete bumps seq above prior cursor"
        );
    }

    // ── Non-sticky moderator delete ────────────────────────────────────────────

    #[test]
    fn moderator_delete_is_non_sticky_add_resurrects_mapping() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = naiad_core::Hash::from_bytes([0x10; 32]);
        let lo = "10".to_string() + &"00".repeat(31);
        let hi = "20".to_string() + &"00".repeat(31);

        add(&store, &acct, &h, "character:samus");
        store
            .moderator_delete_mapping(&h.to_hex(), "character:samus")
            .unwrap();
        // Mapping is deleted.
        assert!(store.snapshot().unwrap().is_empty(), "mapping deleted");

        let cursor = store.mapping_cursor().unwrap();
        // A later signed Op::Add must resurrect the mapping (non-sticky).
        add(&store, &acct, &h, "character:samus");
        let row = store
            .bucket_delta(&lo, &hi, cursor, usize::MAX)
            .unwrap()
            .0
            .into_iter()
            .find(|d| d.tag == "character:samus")
            .expect("resurrected mapping in delta");
        assert_eq!(
            row.status,
            MappingStatus::Current,
            "Add after moderator delete resurrects the mapping"
        );
        assert!(row.seq > cursor, "seq bumped above prior cursor");
        assert_eq!(
            store
                .snapshot()
                .unwrap()
                .get(&h.to_hex())
                .unwrap()
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["character:samus"],
            "resurrected mapping appears in snapshot"
        );
    }

    // ── moderator_delete_mapping closes matching open reports (spec §3) ─────────

    #[test]
    fn moderator_delete_mapping_closes_matching_reports_leaves_unrelated_open() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = naiad_core::Hash::from_bytes([0x10; 32]);
        let h_hex = h.to_hex();
        let ts = now();

        // Insert the mapping.
        add(&store, &acct, &h, "character:samus");

        // Two open reports for (h, "character:samus").
        store
            .insert_report(&h_hex, "character:samus", "reporter1", Some("spam"), ts)
            .unwrap();
        store
            .insert_report(&h_hex, "character:samus", "reporter2", None, ts)
            .unwrap();

        // One unrelated open report for a different (hash, tag) pair.
        let h2 = naiad_core::Hash::from_bytes([0x20; 32]);
        add(&store, &acct, &h2, "series:metroid");
        store
            .insert_report(&h2.to_hex(), "series:metroid", "reporter3", None, ts)
            .unwrap();

        // All three are open before the delete.
        assert_eq!(store.open_reports().unwrap().len(), 3);

        // Delete the mapping for (h, "character:samus").
        store
            .moderator_delete_mapping(&h_hex, "character:samus")
            .unwrap();

        // The two matching reports are now closed; the unrelated one stays open.
        let open = store.open_reports().unwrap();
        assert_eq!(open.len(), 1, "only the unrelated report stays open");
        assert_eq!(open[0].hash, h2.to_hex());
        assert_eq!(open[0].tag, "series:metroid");
    }

    // ── Error-surface: not-found checks ───────────────────────────────────────

    #[test]
    fn set_role_errors_on_unknown_pubkey() {
        let store = RepoStore::open_in_memory().unwrap();
        assert!(
            store.set_role("unknown_pubkey", "moderator").is_err(),
            "set_role must error on unknown pubkey"
        );
    }

    #[test]
    fn set_banned_errors_on_unknown_pubkey() {
        let store = RepoStore::open_in_memory().unwrap();
        assert!(
            store.set_banned("unknown_pubkey", true).is_err(),
            "set_banned must error on unknown pubkey"
        );
    }

    #[test]
    fn close_report_errors_on_unknown_id() {
        let store = RepoStore::open_in_memory().unwrap();
        assert!(
            store.close_report(9999).is_err(),
            "close_report must error on unknown report id"
        );
    }

    #[test]
    fn moderator_delete_mapping_errors_on_unknown_pair() {
        let store = RepoStore::open_in_memory().unwrap();
        assert!(
            store
                .moderator_delete_mapping("no_such_hash", "no_such_tag")
                .is_err(),
            "moderator_delete_mapping must error when the pair is absent"
        );
    }

    // ── Snapshot / bucket / count sanity ─────────────────────────────────────

    #[test]
    fn snapshot_shows_current_tags_grouped_by_hash() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        add(&store, &acct, &h, "character:samus");
        add(&store, &acct, &h, "series:metroid");
        let snap = store.snapshot().unwrap();
        let tags = snap.get(&h.to_hex()).unwrap();
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().any(|t| t.tag == "character:samus"));
        assert!(tags.iter().any(|t| t.tag == "series:metroid"));
    }

    #[test]
    fn remove_tombstones_tag_from_snapshot() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        add(&store, &acct, &h, "character:samus");
        remove_sub(&store, &acct, &h, "character:samus");
        assert!(
            store.snapshot().unwrap().is_empty(),
            "removed tag must not appear in snapshot"
        );
    }

    #[test]
    fn distinct_hash_count_counts_current_hashes_only() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"one");
        let h2 = hash_bytes(b"two");
        add(&store, &acct, &h1, "a:x");
        add(&store, &acct, &h1, "a:y"); // same hash, two tags → still one hash
        add(&store, &acct, &h2, "a:z");
        assert_eq!(store.distinct_hash_count().unwrap(), 2);
        remove_sub(&store, &acct, &h2, "a:z");
        assert_eq!(store.distinct_hash_count().unwrap(), 1);
    }

    #[test]
    fn current_mapping_count_excludes_deleted_rows() {
        let store = RepoStore::open_in_memory().unwrap();
        // Two current mappings seeded via bulk ingest.
        store
            .apply_mappings_bulk(vec![
                ("ab".repeat(32), "character:samus".to_string(), false),
                ("ab".repeat(32), "series:metroid".to_string(), false),
                ("cd".repeat(32), "character:link".to_string(), false),
            ])
            .unwrap();
        assert_eq!(
            store.current_mapping_count().unwrap(),
            3,
            "all three current"
        );
        // Mark one as deleted (is_delete = true).
        store
            .apply_mappings_bulk(vec![("cd".repeat(32), "character:link".to_string(), true)])
            .unwrap();
        assert_eq!(
            store.current_mapping_count().unwrap(),
            2,
            "deleted row must not be counted"
        );
    }

    #[test]
    fn bucket_returns_only_in_range_hashes() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let lo_h = naiad_core::Hash::from_bytes([0x10; 32]);
        let hi_h = naiad_core::Hash::from_bytes([0x90; 32]);
        add(&store, &acct, &lo_h, "in:range");
        add(&store, &acct, &hi_h, "out:range");
        let lo = "10".to_string() + &"00".repeat(31);
        let hi = "20".to_string() + &"00".repeat(31);
        let got = store.bucket(&lo, &hi, usize::MAX).unwrap().0;
        assert_eq!(got.len(), 1);
        assert!(got.contains_key(&lo_h.to_hex()));
    }

    #[test]
    fn mapping_cursor_increments_on_submit_and_moderator_delete() {
        let store = RepoStore::open_in_memory().unwrap();
        assert_eq!(store.mapping_cursor().unwrap(), 0, "empty repo cursor is 0");
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        add(&store, &acct, &h, "a:x");
        let c1 = store.mapping_cursor().unwrap();
        assert_eq!(c1, 1);
        add(&store, &acct, &h, "a:y");
        let c2 = store.mapping_cursor().unwrap();
        assert!(c2 > c1, "second submit (new tag) advances cursor");
        store.moderator_delete_mapping(&h.to_hex(), "a:x").unwrap();
        let c3 = store.mapping_cursor().unwrap();
        assert!(c3 > c2, "moderator delete advances cursor");
    }

    #[test]
    fn bucket_delta_returns_changes_since_cursor() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = naiad_core::Hash::from_bytes([0x10; 32]);
        let lo = "10".to_string() + &"00".repeat(31);
        let hi = "20".to_string() + &"00".repeat(31);
        add(&store, &acct, &h, "a:x");
        let mid = store.mapping_cursor().unwrap();
        add(&store, &acct, &h, "a:y");
        let delta = store.bucket_delta(&lo, &hi, mid, usize::MAX).unwrap().0;
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].tag, "a:y");
        assert_eq!(delta[0].status, MappingStatus::Current);
        assert!(delta[0].seq > mid);
    }

    #[test]
    fn read_snapshot_commit_and_rollback() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        add(&store, &acct, &h, "a:x");
        let n = store.read_snapshot(|s| s.distinct_hash_count()).unwrap();
        assert_eq!(n, 1);
        // An Err-returning closure must not leave a dangling transaction.
        let err: Result<()> = store.read_snapshot(|_| Err(anyhow::anyhow!("boom")));
        assert!(err.is_err());
        let again = store.read_snapshot(|s| s.distinct_hash_count()).unwrap();
        assert_eq!(again, 1);
    }

    #[test]
    fn open_readonly_sees_writer_commits_and_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.db");
        let writer = RepoStore::open(&path).unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        add(&writer, &acct, &h, "character:samus");
        let reader = RepoStore::open_readonly(&path).unwrap();
        let snap = reader.snapshot().unwrap();
        assert_eq!(snap.get(&h.to_hex()).unwrap().len(), 1);
        assert!(
            reader
                .conn
                .execute("DELETE FROM repo_mappings", [])
                .is_err(),
            "read-only connection must reject writes"
        );
    }

    // ── Relations ─────────────────────────────────────────────────────────────

    #[test]
    fn banned_key_rejected_on_relation_submit() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        // First tag submit creates the account.
        add(&store, &acct, &h, "character:samus");
        // Ban the account.
        store.set_banned(&acct.public_hex(), true).unwrap();
        // A relation submit from the banned key must fail.
        let sub = acct.sign_relation(
            Op::Add,
            RelKind::Sibling,
            &Tag::parse("character:samus_aran").unwrap(),
            &Tag::parse("character:samus").unwrap(),
        );
        assert!(
            store.apply_relation(&sub).is_err(),
            "banned key must be rejected on relation submit"
        );
    }

    #[test]
    fn apply_relation_auto_creates_account() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        // No prior submit — account should not exist yet.
        assert!(store.account(&acct.public_hex()).unwrap().is_none());
        add_rel(
            &store,
            &acct,
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
        );
        // Account must have been auto-created.
        let row = store
            .account(&acct.public_hex())
            .unwrap()
            .expect("account auto-created on first relation submit");
        assert_eq!(row.role, "contributor");
        assert!(!row.banned);
    }

    #[test]
    fn applied_relations_appear_in_the_graph() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        add_rel(
            &store,
            &acct,
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
        );
        add_rel(
            &store,
            &acct,
            RelKind::Parent,
            "character:samus",
            "series:metroid",
        );
        let graph = store.relations().unwrap();
        assert_eq!(graph.siblings.len(), 1);
        assert_eq!(graph.siblings[0].from, "character:samus_aran");
        assert_eq!(graph.siblings[0].to, "character:samus");
        assert_eq!(graph.siblings[0].author, acct.public_hex());
        assert_eq!(graph.parents.len(), 1);
    }

    #[test]
    fn relation_remove_tombstones_edge() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        add_rel(
            &store,
            &acct,
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
        );
        let rm = acct.sign_relation(
            Op::Remove,
            RelKind::Sibling,
            &Tag::parse("character:samus_aran").unwrap(),
            &Tag::parse("character:samus").unwrap(),
        );
        store.apply_relation(&rm).unwrap();
        assert!(
            store.relations().unwrap().siblings.is_empty(),
            "removed relation must not appear in graph"
        );
    }

    #[test]
    fn a_removed_relation_is_sticky_against_a_later_add() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        add_rel(
            &store,
            &acct,
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
        );
        let rm = acct.sign_relation(
            Op::Remove,
            RelKind::Sibling,
            &Tag::parse("character:samus_aran").unwrap(),
            &Tag::parse("character:samus").unwrap(),
        );
        store.apply_relation(&rm).unwrap();
        // Replay Add — must not resurrect.
        add_rel(
            &store,
            &acct,
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
        );
        assert!(
            store.relations().unwrap().siblings.is_empty(),
            "a removed relation is sticky; a later Add must not resurrect it"
        );
    }

    #[test]
    fn edges_since_returns_incremental_edges_ordered_by_seq() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let (x, y) = (Tag::parse("a:x").unwrap(), Tag::parse("a:y").unwrap());
        let (c, p) = (Tag::parse("a:c").unwrap(), Tag::parse("a:p").unwrap());
        store
            .apply_relation(&acct.sign_relation(Op::Add, RelKind::Sibling, &x, &y))
            .unwrap();
        let mid = store.relation_cursor().unwrap();
        store
            .apply_relation(&acct.sign_relation(Op::Add, RelKind::Parent, &c, &p))
            .unwrap();
        store
            .apply_relation(&acct.sign_relation(Op::Remove, RelKind::Sibling, &x, &y))
            .unwrap();
        let delta = store.edges_since(mid).unwrap();
        assert!(delta.iter().all(|e| e.seq > mid));
        assert!(
            delta.windows(2).all(|w| w[0].seq < w[1].seq),
            "ordered by seq"
        );
        let tomb = delta
            .iter()
            .find(|e| e.from == "a:x" && e.to == "a:y")
            .expect("tombstone in delta");
        assert_eq!(tomb.status, EdgeStatus::Deleted);
    }

    #[test]
    fn relations_carries_the_cursor() {
        let store = RepoStore::open_in_memory().unwrap();
        assert_eq!(
            store.relations().unwrap().cursor,
            0,
            "empty repo cursor is 0"
        );
        let acct = Account::generate();
        let (x, y) = (Tag::parse("a:x").unwrap(), Tag::parse("a:y").unwrap());
        store
            .apply_relation(&acct.sign_relation(Op::Add, RelKind::Sibling, &x, &y))
            .unwrap();
        assert_eq!(
            store.relations().unwrap().cursor,
            store.relation_cursor().unwrap()
        );
    }

    // ── #225: apply_bridge_relations (last-writer-wins, no-op-guarded) ─────────

    /// Build a one-element bridge submission for `kind from → to` under `op`.
    fn bridge_rel(
        acct: &Account,
        op: Op,
        kind: RelKind,
        from: &str,
        to: &str,
    ) -> naiad_netproto::RelationSubmission {
        acct.sign_relation(
            op,
            kind,
            &Tag::parse(from).unwrap(),
            &Tag::parse(to).unwrap(),
        )
    }

    /// LWW: add → delete → add must net `current`, proving remove-dominates
    /// (which `apply_relation` enforces) is bypassed for the bridge author.
    #[test]
    fn bridge_relation_lww_readd_after_delete() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let add = bridge_rel(&acct, Op::Add, RelKind::Sibling, "a:bad", "a:good");
        let del = bridge_rel(&acct, Op::Remove, RelKind::Sibling, "a:bad", "a:good");
        assert_eq!(
            store
                .apply_bridge_relations(std::slice::from_ref(&add))
                .unwrap(),
            1
        );
        assert_eq!(store.apply_bridge_relations(&[del]).unwrap(), 1);
        assert_eq!(store.apply_bridge_relations(&[add]).unwrap(), 1);
        // The edge is live again — remove did NOT dominate.
        let g = store.relations().unwrap();
        assert_eq!(g.siblings.len(), 1, "re-added sibling is current");
        assert_eq!(g.siblings[0].from, "a:bad");
        assert_eq!(g.siblings[0].to, "a:good");
        assert_eq!(g.siblings[0].author, acct.public_hex());
    }

    /// Replaying an identical index is a relation-`seq` no-op: the cursor must
    /// not move and no row's `seq` may change.
    #[test]
    fn bridge_relation_replay_is_seq_noop() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let rows = vec![
            bridge_rel(&acct, Op::Add, RelKind::Sibling, "a:x", "a:y"),
            bridge_rel(&acct, Op::Add, RelKind::Parent, "a:c", "a:p"),
        ];
        assert_eq!(store.apply_bridge_relations(&rows).unwrap(), 2);
        let cursor = store.relation_cursor().unwrap();
        // Full per-row snapshot (kind, from, to, seq) so we catch churn of a
        // non-maximal row's seq, which a cursor-only check would miss.
        let snapshot = |s: &RepoStore| -> Vec<(String, String, String, i64)> {
            let mut stmt = s
                .conn
                .prepare("SELECT kind, from_tag, to_tag, seq FROM relations ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let before = snapshot(&store);
        // Re-apply the identical rows: guard makes every row a no-op.
        assert_eq!(
            store.apply_bridge_relations(&rows).unwrap(),
            0,
            "replay changes nothing"
        );
        assert_eq!(
            store.relation_cursor().unwrap(),
            cursor,
            "cursor must not advance on replay"
        );
        assert_eq!(
            snapshot(&store),
            before,
            "no row's seq (or key) moved on replay"
        );
        // And no new edges surfaced via the incremental path since the cursor.
        assert!(
            store.edges_since(cursor).unwrap().is_empty(),
            "no edge crossed the cursor on replay"
        );
    }

    /// A real add→delete advances `seq` and the tombstone is visible via
    /// `edges_since`.
    #[test]
    fn bridge_relation_status_change_bumps_seq() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        store
            .apply_bridge_relations(&[bridge_rel(&acct, Op::Add, RelKind::Sibling, "a:x", "a:y")])
            .unwrap();
        let after_add = store.relation_cursor().unwrap();
        let changed = store
            .apply_bridge_relations(&[bridge_rel(
                &acct,
                Op::Remove,
                RelKind::Sibling,
                "a:x",
                "a:y",
            )])
            .unwrap();
        assert_eq!(changed, 1, "the delete changed status");
        assert!(
            store.relation_cursor().unwrap() > after_add,
            "status change bumped seq"
        );
        let delta = store.edges_since(after_add).unwrap();
        assert_eq!(delta.len(), 1, "one edge crossed the cursor");
        assert_eq!(delta[0].status, naiad_netproto::EdgeStatus::Deleted);
        assert_eq!(delta[0].author, acct.public_hex());
    }

    // ── Preflight: foreign / wrong-file rejection ─────────────────────────────

    #[test]
    fn opening_client_library_db_names_the_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (id INTEGER PRIMARY KEY, blake3 TEXT NOT NULL UNIQUE);
                 PRAGMA user_version = 31;",
            )
            .unwrap();
        }
        let err = format!("{:#}", RepoStore::open(&path).unwrap_err());
        assert!(err.contains("client library"), "got: {err}");
        let err = format!("{:#}", RepoStore::open_readonly(&path).unwrap_err());
        assert!(err.contains("client library"), "got: {err}");
    }

    #[test]
    fn opening_unknown_db_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("random.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE zzz (id INTEGER); PRAGMA user_version = 5;")
                .unwrap();
        }
        let err = format!("{:#}", RepoStore::open(&path).unwrap_err());
        assert!(err.contains("not a naiad repo database"), "got: {err}");
        let err = format!("{:#}", RepoStore::open_readonly(&path).unwrap_err());
        assert!(err.contains("not a naiad repo database"), "got: {err}");
    }

    #[test]
    fn opening_newer_repo_db_suggests_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.db");
        drop(RepoStore::open(&path).unwrap());
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 999).unwrap();
        }
        let err = format!("{:#}", RepoStore::open(&path).unwrap_err());
        assert!(err.contains("newer naiad-repo"), "got: {err}");
    }

    #[test]
    fn advise_falls_back_below_k_and_sizes_above() {
        assert_eq!(advise(0, 1000), PullMode::WholeRepo);
        assert_eq!(advise(999, 1000), PullMode::WholeRepo);
        assert_eq!(advise(1500, 1000), PullMode::WholeRepo);
        assert_eq!(advise(2000, 1000), PullMode::Bucketed { prefix_bits: 1 });
        assert_eq!(
            advise(8 * 1000, 1000),
            PullMode::Bucketed { prefix_bits: 3 }
        );
        assert_eq!(advise(10, 0), PullMode::WholeRepo);
    }

    #[test]
    fn list_accounts_returns_all_created_accounts() {
        let store = RepoStore::open_in_memory().unwrap();
        let a = Account::generate();
        let b = Account::generate();
        let h = hash_bytes(b"file");
        add(&store, &a, &h, "a:x");
        add(&store, &b, &h, "a:y");
        let accounts = store.list_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().any(|r| r.pubkey == a.public_hex()));
        assert!(accounts.iter().any(|r| r.pubkey == b.public_hex()));
    }

    // ── seed_mappings ─────────────────────────────────────────────────────────

    #[test]
    fn seed_mappings_inserts_all_new() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"file1");
        let h2 = hash_bytes(b"file2");
        let h3 = hash_bytes(b"file3");
        let items = vec![
            (h1, Tag::parse("character:samus").unwrap()),
            (h2, Tag::parse("series:metroid").unwrap()),
            (h3, Tag::parse("creator:nintendo").unwrap()),
        ];
        let summary = store.seed_mappings(&acct, items).unwrap();
        assert_eq!(summary.inserted, 3);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.total, 3);

        // All three should appear in the snapshot.
        let snap = store.snapshot().unwrap();
        assert_eq!(
            snap[&h1.to_hex()]
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["character:samus"]
        );
        assert_eq!(
            snap[&h2.to_hex()]
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["series:metroid"]
        );
        assert_eq!(
            snap[&h3.to_hex()]
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["creator:nintendo"]
        );
    }

    #[test]
    fn seed_mappings_is_idempotent_on_rerun() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"file1");
        let h2 = hash_bytes(b"file2");
        let items = || {
            vec![
                (h1, Tag::parse("character:samus").unwrap()),
                (h2, Tag::parse("series:metroid").unwrap()),
            ]
        };

        let first = store.seed_mappings(&acct, items()).unwrap();
        assert_eq!(first.inserted, 2);
        assert_eq!(first.skipped, 0);

        let cursor_after_first = store.mapping_cursor().unwrap();

        let second = store.seed_mappings(&acct, items()).unwrap();
        assert_eq!(second.inserted, 0);
        assert_eq!(second.skipped, 2);
        assert_eq!(second.total, 2);

        // Seq high-watermark must not advance on re-run (no seq burned).
        assert_eq!(store.mapping_cursor().unwrap(), cursor_after_first);
    }

    #[test]
    fn seed_mappings_dedups_within_one_call() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");
        let tag = Tag::parse("character:samus").unwrap();
        // Same pair twice in a single call.
        let items = vec![(h, tag.clone()), (h, tag)];
        let summary = store.seed_mappings(&acct, items).unwrap();
        assert_eq!(summary.inserted, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.total, 2);
    }

    #[test]
    fn seed_mappings_resurrects_deleted() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let mod_acct = Account::generate();
        let h = hash_bytes(b"file");
        let tag_str = "character:samus";

        // Seed a mapping, then moderator-delete it.
        add(&store, &acct, &h, tag_str);
        store
            .moderator_delete_mapping(&h.to_hex(), tag_str)
            .unwrap();

        // Snapshot should be empty now.
        assert!(store.snapshot().unwrap().is_empty());

        // seed_mappings should resurrect it.
        let summary = store
            .seed_mappings(&mod_acct, vec![(h, Tag::parse(tag_str).unwrap())])
            .unwrap();
        assert_eq!(summary.inserted, 1);
        assert_eq!(summary.skipped, 0);

        let snap = store.snapshot().unwrap();
        assert_eq!(
            snap[&h.to_hex()]
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec![tag_str]
        );
    }

    #[test]
    fn seed_mappings_rejects_banned_operator() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"file");

        // First add creates the account, then ban it.
        add(&store, &acct, &h, "character:samus");
        store.set_banned(&acct.public_hex(), true).unwrap();

        let err = store
            .seed_mappings(&acct, vec![(h, Tag::parse("series:metroid").unwrap())])
            .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("banned"), "got: {msg}");
    }

    // ── apply_mappings_bulk ───────────────────────────────────────────────────

    #[test]
    fn apply_mappings_bulk_inserts_updates_and_deletes() {
        let store = RepoStore::open_in_memory().unwrap();
        let h = "aa".repeat(32); // 64-hex sha256-shaped key
        let stats = store
            .apply_mappings_bulk(vec![
                (h.clone(), "character:samus".to_string(), false),
                (h.clone(), "series:metroid".to_string(), false),
            ])
            .unwrap();
        assert_eq!((stats.applied, stats.deleted), (2, 0));
        let lo = "00".repeat(32);
        let hi = "gg"; // sentinel upper bound, sorts after any hex
        let bucket = store.bucket(&lo, hi, usize::MAX).unwrap().0;
        assert_eq!(bucket.get(&h).map(Vec::len), Some(2));
        assert_eq!(store.mapping_cursor().unwrap(), 2);

        let stats = store
            .apply_mappings_bulk(vec![(h.clone(), "series:metroid".to_string(), true)])
            .unwrap();
        assert_eq!((stats.applied, stats.deleted), (0, 1));
        let bucket = store.bucket(&lo, hi, usize::MAX).unwrap().0;
        assert_eq!(
            bucket.get(&h).map(Vec::len),
            Some(1),
            "only samus remains current"
        );
        let delta = store.bucket_delta(&lo, hi, 2, usize::MAX).unwrap().0;
        assert!(
            delta
                .iter()
                .any(|d| d.tag == "series:metroid"
                    && d.status == naiad_netproto::MappingStatus::Deleted),
            "delete visible past the prior cursor: {delta:?}"
        );
        assert_eq!(
            store.mapping_cursor().unwrap(),
            3,
            "cursor advanced past the delete"
        );
    }

    #[test]
    fn apply_mappings_bulk_writes_no_submissions() {
        let store = RepoStore::open_in_memory().unwrap();
        store
            .apply_mappings_bulk(vec![("bb".repeat(32), "a:b".to_string(), false)])
            .unwrap();
        let n: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM submissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "bulk ingest is unsigned: no submissions log rows");
    }

    // ── No-op seq guard + reclaim (#180 G4, Phase 3) ─────────────────────────

    /// (a) Re-applying an identical bulk batch leaves `mapping_cursor()` unchanged
    /// and produces no new `bucket_delta` rows (pure echoes are free).
    #[test]
    fn apply_mappings_bulk_echo_does_not_advance_cursor() {
        let store = RepoStore::open_in_memory().unwrap();
        let h = "aa".repeat(32);
        let lo = "00".repeat(32);
        let hi = "gg";

        let batch = vec![
            (h.clone(), "character:samus".to_string(), false),
            (h.clone(), "series:metroid".to_string(), false),
        ];
        // First apply: real inserts, cursor advances to 2.
        store.apply_mappings_bulk(batch.clone()).unwrap();
        let cursor_after_first = store.mapping_cursor().unwrap();
        assert_eq!(cursor_after_first, 2, "cursor after first apply");

        // Second apply: identical batch → echoes, cursor must not move.
        let stats = store.apply_mappings_bulk(batch).unwrap();
        assert_eq!(
            store.mapping_cursor().unwrap(),
            cursor_after_first,
            "cursor must not advance on pure echo"
        );
        // Stats: no real writes.
        assert_eq!(
            (stats.applied, stats.deleted),
            (0, 0),
            "echo batch must report zero applied/deleted"
        );
        // bucket_delta since=cursor_after_first: no new rows.
        let (delta, _) = store
            .bucket_delta(&lo, hi, cursor_after_first, usize::MAX)
            .unwrap();
        assert!(
            delta.is_empty(),
            "echo must produce no new bucket_delta rows: {delta:?}"
        );
    }

    /// (b) A status flip (add→delete) DOES advance seq.
    #[test]
    fn apply_mappings_bulk_status_flip_advances_cursor() {
        let store = RepoStore::open_in_memory().unwrap();
        let h = "bb".repeat(32);

        store
            .apply_mappings_bulk(vec![(h.clone(), "character:link".to_string(), false)])
            .unwrap();
        let cursor_after_add = store.mapping_cursor().unwrap();

        // Flip to deleted — this is a real status change.
        let stats = store
            .apply_mappings_bulk(vec![(h.clone(), "character:link".to_string(), true)])
            .unwrap();
        assert!(
            store.mapping_cursor().unwrap() > cursor_after_add,
            "status flip must advance the cursor"
        );
        assert_eq!(
            (stats.applied, stats.deleted),
            (0, 1),
            "status flip must count as 1 deleted"
        );
    }

    /// (c) `apply_submission` re-assert with a new origin advances seq;
    /// a truly identical re-assert (same status + same origin) does not.
    #[test]
    fn apply_submission_new_origin_advances_seq_identical_does_not() {
        use naiad_netproto::Op;
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"seq-guard-test");
        let tag = Tag::parse("character:samus").unwrap();

        // First submission: Add with origin "tool-a".
        let sub1 = acct.sign_with_origin(Op::Add, &h, &tag, Some("tool-a"));
        store.apply_submission(&sub1).unwrap();
        let cursor1 = store.mapping_cursor().unwrap();

        // Second submission: same (hash, tag, status=Add) but new origin "tool-b" → real change.
        let sub2 = acct.sign_with_origin(Op::Add, &h, &tag, Some("tool-b"));
        store.apply_submission(&sub2).unwrap();
        let cursor2 = store.mapping_cursor().unwrap();
        assert!(
            cursor2 > cursor1,
            "new origin must advance seq: {cursor1} → {cursor2}"
        );

        // Third submission: identical (hash, tag, status=Add, origin=tool-b) → echo, no advance.
        let sub3 = acct.sign_with_origin(Op::Add, &h, &tag, Some("tool-b"));
        store.apply_submission(&sub3).unwrap();
        let cursor3 = store.mapping_cursor().unwrap();
        assert_eq!(
            cursor3, cursor2,
            "truly identical re-assert must not advance seq"
        );

        // Submissions log must have all three entries (append-only).
        let n: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE hash = ?1 AND tag = ?2",
                rusqlite::params![h.to_hex(), tag.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 3,
            "submissions log must record all three events regardless"
        );
    }

    /// (d) A mixed echo/real batch keeps seq monotone and dense:
    /// cursor advances by exactly the number of real changes.
    #[test]
    fn apply_mappings_bulk_mixed_echo_real_cursor_dense() {
        let store = RepoStore::open_in_memory().unwrap();
        let h1 = "cc".repeat(32); // will be re-asserted (echo)
        let h2 = "dd".repeat(32); // new hash → real insert
        let h3 = "ee".repeat(32); // will be re-asserted (echo)
        let h4 = "ff".repeat(32); // new hash → real insert

        // Seed h1 and h3 first.
        store
            .apply_mappings_bulk(vec![
                (h1.clone(), "a:b".to_string(), false),
                (h3.clone(), "c:d".to_string(), false),
            ])
            .unwrap();
        let cursor_before = store.mapping_cursor().unwrap();
        assert_eq!(cursor_before, 2);

        // Mixed batch: h1 echo, h2 new, h3 echo, h4 new.
        let stats = store
            .apply_mappings_bulk(vec![
                (h1.clone(), "a:b".to_string(), false), // echo
                (h2.clone(), "a:b".to_string(), false), // new
                (h3.clone(), "c:d".to_string(), false), // echo
                (h4.clone(), "c:d".to_string(), false), // new
            ])
            .unwrap();

        let cursor_after = store.mapping_cursor().unwrap();
        // Only 2 real inserts → cursor advances by exactly 2.
        assert_eq!(
            cursor_after,
            cursor_before + 2,
            "cursor must advance by real-change count only (dense, no gaps)"
        );
        assert_eq!(
            (stats.applied, stats.deleted),
            (2, 0),
            "only real changes counted in stats"
        );
    }

    /// §10.10 (I7) — the sync-path wrapper `apply_mappings_bulk` writes no checkpoint
    /// and its stats are unaffected by the new ckpt parameter.
    #[test]
    fn sync_wrapper_writes_no_checkpoint() {
        let store = RepoStore::open_in_memory().unwrap();
        let stats = store
            .apply_mappings_bulk(vec![("ab".repeat(32), "a:x".to_string(), false)])
            .unwrap();
        assert_eq!(stats.applied, 1, "one current mapping applied");
        let n: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM repo_meta WHERE key = 'seed_ckpt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "sync path must never write a seed_ckpt row (I7)");
    }

    /// `open_bulk_ingest`: opens and migrates a fresh db; data written through it
    /// is readable via a normal `open` connection; PRAGMA values are as specified.
    #[test]
    fn open_bulk_ingest_migrates_and_data_readable_via_open() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("repo.db");

        // Write data through the bulk-ingest connection.
        {
            let store = RepoStore::open_bulk_ingest(&db_path, false).unwrap();
            store
                .apply_mappings_bulk(vec![(
                    "aa".repeat(32),
                    "character:samus".to_string(),
                    false,
                )])
                .unwrap();

            // Verify PRAGMA values on the bulk-ingest connection.
            let cache_size: i64 = store
                .conn
                .pragma_query_value(None, "cache_size", |r| r.get(0))
                .unwrap();
            // SQLite may return the page-count form; the negative KiB form may be
            // normalised. Accept either the raw -262144 or any equivalent large value.
            assert!(
                cache_size <= -262_144 || cache_size >= 32_768,
                "cache_size should be large (got {cache_size})"
            );

            let sync: i64 = store
                .conn
                .pragma_query_value(None, "synchronous", |r| r.get(0))
                .unwrap();
            // NORMAL = 1
            assert_eq!(
                sync, 1,
                "synchronous should be NORMAL (1) for unsafe_fast=false"
            );

            let temp: i64 = store
                .conn
                .pragma_query_value(None, "temp_store", |r| r.get(0))
                .unwrap();
            // MEMORY = 2
            assert_eq!(temp, 2, "temp_store should be MEMORY (2)");
        } // connection closes here

        // Data must be visible through a normal open connection.
        let store2 = RepoStore::open(&db_path).unwrap();
        let snap = store2.snapshot().unwrap();
        assert!(
            snap.values()
                .any(|tags| tags.iter().any(|t| t.tag.contains("samus"))),
            "data written via open_bulk_ingest must be readable via open"
        );
    }

    // ── Origin columns (#162) ─────────────────────────────────────────────────

    #[test]
    fn origin_columns_are_unindexed() {
        // Origin is display/filter metadata; it must NEVER key a query.
        // Verify that no index on either origin column was created by the
        // migration (the seq/hash indexes are the only access paths).
        let store = RepoStore::open_in_memory().unwrap();
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND sql LIKE '%origin%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "origin columns must have no index");
    }

    /// Read `repo_mappings.origin` for a given hex hash + tag string.
    ///
    /// Test-only helper: drives the join needed after the 0003 schema migration
    /// (repo_mappings no longer stores hash/tag TEXT directly). Returns `None`
    /// both when the row doesn't exist and when origin is NULL.
    #[cfg(test)]
    fn origin_of(store: &RepoStore, hash: &naiad_core::Hash, tag: &str) -> Option<String> {
        let hash_blob: Vec<u8> = hash.as_bytes().to_vec();
        // Use Option<String> in the closure so NULL columns return Ok(None)
        // rather than Err(InvalidColumnType).
        store
            .conn
            .query_row(
                "SELECT m.origin
                 FROM   repo_mappings m
                 JOIN   repo_hashes h ON h.id = m.hash_id
                 JOIN   repo_tags   t ON t.id = m.tag_id
                 WHERE  h.hash = ?1 AND t.tag = ?2",
                rusqlite::params![hash_blob, tag],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or(None)
    }

    #[test]
    fn origin_persists_to_both_tables_and_conflict_updates_repo_mappings() {
        use naiad_netproto::Op;
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"origin-test");
        let tag = Tag::parse("character:samus").unwrap();

        // First Add with origin "wd14-tagger" — should land in both tables.
        let sub1 = acct.sign_with_origin(Op::Add, &h, &tag, Some("wd14-tagger"));
        store.apply_submission(&sub1).unwrap();

        let origin_sub: Option<String> = store
            .conn
            .query_row(
                "SELECT origin FROM submissions WHERE hash = ?1 AND tag = ?2",
                rusqlite::params![h.to_hex(), tag.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            origin_sub.as_deref(),
            Some("wd14-tagger"),
            "submissions.origin must be stored"
        );

        let origin_map = origin_of(&store, &h, &tag.to_string());
        assert_eq!(
            origin_map.as_deref(),
            Some("wd14-tagger"),
            "repo_mappings.origin must be stored"
        );

        // Second Add for the same (hash, tag) with a different origin — conflict path.
        // repo_mappings.origin must update to the new value (latest assertion wins).
        let sub2 = acct.sign_with_origin(Op::Add, &h, &tag, Some("gelbooru"));
        store.apply_submission(&sub2).unwrap();

        let origin_map2 = origin_of(&store, &h, &tag.to_string());
        assert_eq!(
            origin_map2.as_deref(),
            Some("gelbooru"),
            "conflict path must update repo_mappings.origin to the latest origin"
        );

        // MINOR 1: Third Add for the SAME (hash, tag) with no origin (plain sign).
        // repo_mappings.origin must be clobbered to NULL (Some→None, latest wins).
        let sub3 = acct.sign(Op::Add, &h, &tag);
        store.apply_submission(&sub3).unwrap();

        let origin_map3 = origin_of(&store, &h, &tag.to_string());
        assert!(
            origin_map3.is_none(),
            "a later manual Add (origin=None) must clobber repo_mappings.origin to NULL"
        );

        // MINOR 2: The submissions log must retain ALL three assertions in insertion
        // order (append-only log; the conflict-path UPSERT only touches repo_mappings).
        let mut sub_stmt = store
            .conn
            .prepare("SELECT origin FROM submissions WHERE hash = ?1 AND tag = ?2 ORDER BY rowid")
            .unwrap();
        let sub_origins: Vec<Option<String>> = sub_stmt
            .query_map(rusqlite::params![h.to_hex(), tag.to_string()], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            sub_origins.len(),
            3,
            "submissions log must have three entries for (hash, tag)"
        );
        assert_eq!(
            sub_origins[0].as_deref(),
            Some("wd14-tagger"),
            "first submission origin"
        );
        assert_eq!(
            sub_origins[1].as_deref(),
            Some("gelbooru"),
            "second submission origin"
        );
        assert!(
            sub_origins[2].is_none(),
            "third (manual) submission origin must be NULL"
        );
    }

    // ── Budget accounting with origins (#166) ─────────────────────────────────

    /// Verify that the charged amount returned by bucket() and bucket_delta()
    /// is always >= the actual serialised full response body (Snapshot and
    /// MappingDelta structs respectively), including the JSON envelope and
    /// per-row origin escaping.
    ///
    /// The test primes the seq counter to a 7-digit value so large-seq undercount
    /// bugs are caught: before #166's delta-overhead fix, this test would fail at
    /// seq ≥ 10 because BUCKET_ROW_OVERHEAD only budgeted 1 seq digit.
    /// 200 rows are used so the per-row budget errors accumulate visibly.
    #[test]
    fn budget_charged_gte_actual_full_response_with_large_seq_and_special_chars() {
        use naiad_netproto::{MappingDelta, Op, PROTOCOL_VERSION, Snapshot};

        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();

        // Origin with quotes and backslashes — JSON-escaped it is longer than
        // the raw byte length, exercising json_escaped_len correctness.
        let nasty_origin = r#"tool "name" with \slashes\"#;

        // Query range: hashes from 10...00 (inclusive) to 20...00 (exclusive).
        let lo_h = naiad_core::Hash::from_bytes([0x10; 32]);
        let lo = "10".to_string() + &"00".repeat(31);
        let hi = "20".to_string() + &"00".repeat(31);

        // Prime the seq counter to ~10,000,000 by inserting a single row with a
        // large seq directly. This row uses a hash OUTSIDE the query range so it
        // never appears in the bucket/delta results. After this insert, normal
        // apply_submission() calls receive seq ≥ 10,000,001 (8 digits), which
        // would undercount under the old 1-digit assumption.
        let prime_hash = "ff".repeat(32); // > hi, outside the query range
        // Port to id-based insert for 0003 schema: intern hash + tag first.
        let prime_hash_bytes = hex::decode(&prime_hash).unwrap();
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO repo_hashes(hash) VALUES(?1)",
                rusqlite::params![prime_hash_bytes],
            )
            .unwrap();
        let prime_hash_id: i64 = store
            .conn
            .query_row(
                "SELECT id FROM repo_hashes WHERE hash = ?1",
                rusqlite::params![prime_hash_bytes],
                |r| r.get(0),
            )
            .unwrap();
        store
            .conn
            .execute("INSERT OR IGNORE INTO repo_tags(tag) VALUES('z:prime')", [])
            .unwrap();
        let prime_tag_id: i64 = store
            .conn
            .query_row("SELECT id FROM repo_tags WHERE tag = 'z:prime'", [], |r| {
                r.get(0)
            })
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO repo_mappings(hash_id, tag_id, status, seq) \
                 VALUES(?1, ?2, 1, 9999990)",
                rusqlite::params![prime_hash_id, prime_tag_id],
            )
            .unwrap();

        // Insert 200 rows — enough to accumulate seq-digit undercount visibly.
        for i in 0..200_u32 {
            let tag_str = format!("tag:{i:04}");
            let tag = Tag::parse(&tag_str).unwrap();
            let sub = acct.sign_with_origin(Op::Add, &lo_h, &tag, Some(nasty_origin));
            store.apply_submission(&sub).unwrap();
        }

        let cursor = store.mapping_cursor().unwrap(); // ≥ 10,000,191 (8 digits)

        // ── bucket() (snapshot shape) ──────────────────────────────────────
        let (snap_map, charged_snap) = store.bucket(&lo, &hi, usize::MAX).unwrap();
        // Construct the full Snapshot struct the HTTP handler would serialize.
        let snap_full = Snapshot {
            version: PROTOCOL_VERSION,
            cursor,
            tags: snap_map,
        };
        let snap_actual = serde_json::to_vec(&snap_full).unwrap().len();
        assert!(
            charged_snap >= snap_actual,
            "bucket: charged ({charged_snap}) must be >= full response body ({snap_actual}); \
             budget must be a true upper bound on the actual Snapshot JSON"
        );

        // ── bucket_delta() (delta shape) ───────────────────────────────────
        let (delta_vec, charged_delta) = store.bucket_delta(&lo, &hi, 0, usize::MAX).unwrap();
        // Construct the full MappingDelta struct the HTTP handler would serialize.
        let delta_full = MappingDelta {
            version: PROTOCOL_VERSION,
            cursor,
            changes: delta_vec,
        };
        let delta_actual = serde_json::to_vec(&delta_full).unwrap().len();
        assert!(
            charged_delta >= delta_actual,
            "bucket_delta: charged ({charged_delta}) must be >= full response body ({delta_actual}); \
             budget must be a true upper bound on the actual MappingDelta JSON"
        );
    }

    // ── Migration 0003: old schema → interned schema (#180) ──────────────────

    /// Verify that a store seeded under the 0002 schema (hex TEXT hashes,
    /// TEXT status) migrates to 0003 without error and that snapshot() returns
    /// the same data afterwards.
    ///
    /// Approach: open a temp-file DB, apply 0001+0002 SQL manually, insert a
    /// few old-format rows, then reopen via RepoStore::open (which runs 0003)
    /// and assert snapshot() output matches.
    #[test]
    fn migration_0003_migrates_old_schema_and_preserves_snapshot() {
        use rusqlite::Connection;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");

        let h1 = naiad_core::Hash::from_bytes([0x10; 32]);
        let h2 = naiad_core::Hash::from_bytes([0xab; 32]);
        let h1_hex = h1.to_hex();
        let h2_hex = h2.to_hex();

        // Build a store under 0002 schema via raw SQL.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(include_str!("../migrations/0001_baseline.sql"))
                .unwrap();
            conn.execute_batch(include_str!("../migrations/0002_submission_origin.sql"))
                .unwrap();
            // Tell rusqlite_migration that migrations 1+2 are already applied
            // so that RepoStore::open only runs 0003.
            conn.pragma_update(None, "user_version", 2i64).unwrap();
            // Insert old-format rows: hex TEXT hashes, TEXT status.
            conn.execute_batch(&format!(
                "INSERT INTO repo_mappings(hash, tag, status, seq, origin)
                 VALUES('{h1_hex}', 'character:samus', 'current', 1, 'wd14-tagger');
                 INSERT INTO repo_mappings(hash, tag, status, seq, origin)
                 VALUES('{h1_hex}', 'series:metroid', 'current', 2, NULL);
                 INSERT INTO repo_mappings(hash, tag, status, seq, origin)
                 VALUES('{h2_hex}', 'character:link', 'current', 3, NULL);
                 INSERT INTO repo_mappings(hash, tag, status, seq, origin)
                 VALUES('{h2_hex}', 'character:zelda', 'deleted', 4, NULL);"
            ))
            .unwrap();
        }

        // Reopen via RepoStore::open — this triggers 0003 migration.
        let store =
            RepoStore::open(&path).expect("0003 migration must succeed on a 0002-schema store");

        // snapshot() must return only current rows, with correct hex keys.
        let snap = store.snapshot().unwrap();
        assert_eq!(snap.len(), 2, "two hashes with current mappings");

        let tags_h1 = snap.get(&h1_hex).expect("h1 must appear in snapshot");
        let tag_strs_h1: Vec<&str> = tags_h1.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(
            tag_strs_h1,
            vec!["character:samus", "series:metroid"],
            "h1 tags must be in lexicographic order"
        );
        // Origin preserved through migration.
        let samus_origin = tags_h1
            .iter()
            .find(|t| t.tag == "character:samus")
            .unwrap()
            .origin
            .as_deref();
        assert_eq!(samus_origin, Some("wd14-tagger"), "origin survives 0003");

        let tags_h2 = snap.get(&h2_hex).expect("h2 must appear in snapshot");
        let tag_strs_h2: Vec<&str> = tags_h2.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(tag_strs_h2, vec!["character:link"], "deleted row excluded");

        // The new schema must have repo_hashes and repo_tags tables.
        let has_hashes: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='repo_hashes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            has_hashes, 1,
            "repo_hashes table must exist after migration"
        );

        let has_tags: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='repo_tags'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_tags, 1, "repo_tags table must exist after migration");

        // The old hash index must be gone; only idx_repo_mappings_seq remains.
        let idx_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'
                 AND name LIKE 'idx_repo_mappings%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            idx_count, 1,
            "only idx_repo_mappings_seq must exist (hash index dropped)"
        );

        // Cursor must reflect the migrated seq values.
        assert_eq!(
            store.mapping_cursor().unwrap(),
            4,
            "cursor equals max seq from migrated rows"
        );
    }

    // ── R1: bucket boundary exactness (inclusive lo, exclusive hi, sentinel) ──

    #[test]
    fn bucket_boundary_inclusive_lo_exclusive_hi_and_sentinel() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();

        // Four hashes bracketing the mid-range bucket [20..0, 30..0):
        //   just_below: 1fff…f — one position before lo  → must be EXCLUDED
        //   at_lo:      2000…0 — exactly the lower bound → must be INCLUDED
        //   at_mid:     2fff…f — strictly inside range   → must be INCLUDED
        //   at_hi:      3000…0 — exactly the upper bound → must be EXCLUDED (hi exclusive)
        let just_below = {
            let mut b = [0xffu8; 32];
            b[0] = 0x1f;
            naiad_core::Hash::from_bytes(b)
        };
        let at_lo = {
            let mut b = [0x00u8; 32];
            b[0] = 0x20;
            naiad_core::Hash::from_bytes(b)
        };
        let at_mid = {
            let mut b = [0xffu8; 32];
            b[0] = 0x2f;
            naiad_core::Hash::from_bytes(b)
        };
        let at_hi = {
            let mut b = [0x00u8; 32];
            b[0] = 0x30;
            naiad_core::Hash::from_bytes(b)
        };

        add(&store, &acct, &just_below, "r1:just_below");
        add(&store, &acct, &at_lo, "r1:at_lo");
        add(&store, &acct, &at_mid, "r1:at_mid");
        add(&store, &acct, &at_hi, "r1:at_hi");

        let lo = "20".to_string() + &"00".repeat(31);
        let hi = "30".to_string() + &"00".repeat(31);

        // bucket(): only the two in-range hashes
        let (got, _) = store.bucket(&lo, &hi, usize::MAX).unwrap();
        assert_eq!(got.len(), 2, "bucket: exactly two in-range hashes");
        assert!(
            got.contains_key(&at_lo.to_hex()),
            "bucket: lower bound is inclusive"
        );
        assert!(
            got.contains_key(&at_mid.to_hex()),
            "bucket: midpoint is included"
        );
        assert!(
            !got.contains_key(&just_below.to_hex()),
            "bucket: just-below lo excluded"
        );
        assert!(
            !got.contains_key(&at_hi.to_hex()),
            "bucket: upper bound is exclusive"
        );

        // bucket_delta(): same boundary semantics on the delta path
        let (delta, _) = store.bucket_delta(&lo, &hi, 0, usize::MAX).unwrap();
        let delta_hashes: std::collections::HashSet<String> =
            delta.iter().map(|d| d.hash.clone()).collect();
        assert_eq!(
            delta_hashes.len(),
            2,
            "bucket_delta: exactly two in-range hashes"
        );
        assert!(
            delta_hashes.contains(&at_lo.to_hex()),
            "bucket_delta: lower bound inclusive"
        );
        assert!(
            delta_hashes.contains(&at_mid.to_hex()),
            "bucket_delta: midpoint included"
        );
        assert!(
            !delta_hashes.contains(&just_below.to_hex()),
            "bucket_delta: just-below lo excluded"
        );
        assert!(
            !delta_hashes.contains(&at_hi.to_hex()),
            "bucket_delta: upper bound exclusive"
        );

        // Sentinel: the final bucket [e0..0, "g") must capture ffff…f.
        // hi_bound("g") → 33-byte 0xFF sentinel, which sorts after every 32-byte hash.
        let all_ff = naiad_core::Hash::from_bytes([0xffu8; 32]);
        add(&store, &acct, &all_ff, "r1:all_ff");

        let lo_e0 = "e0".to_string() + &"00".repeat(31);
        let (got_e0, _) = store.bucket(&lo_e0, "g", usize::MAX).unwrap();
        assert!(
            got_e0.contains_key(&all_ff.to_hex()),
            "sentinel 'g': ffff…f must be included in final bucket"
        );

        // "gg" sentinel: full-range bucket ["00..0", "gg") also captures ffff…f.
        let lo_00 = "00".repeat(32);
        let (got_gg, _) = store.bucket(&lo_00, "gg", usize::MAX).unwrap();
        assert!(
            got_gg.contains_key(&all_ff.to_hex()),
            "sentinel 'gg': ffff…f must be included in full-range bucket"
        );
    }

    // ── Migration 0004: explicit droppable unique index (#187) ───────────────

    /// I9: Migration 0004 rebuilds repo_hashes without the inline UNIQUE and
    /// creates the named index. id values — including a gap (1, 2, 5) — must
    /// survive verbatim, and repo_mappings joins must still resolve.
    ///
    /// Approach: apply 0001..0003 via raw rusqlite (set user_version=3), insert
    /// rows with explicit ids (including a gap), then call to_latest so only
    /// 0004 runs, and assert ids/hashes/joins are unchanged.
    #[test]
    fn migration_0004_preserves_hash_ids_and_mappings_joins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo_0004.db");

        let h1 = [0x11u8; 32];
        let h2 = [0x22u8; 32];
        let h5 = [0x55u8; 32]; // id 5, deliberate gap after 2

        // Apply 0001..0003 via raw SQL, bypassing RepoStore::open so that we
        // control exactly which migrations have run.
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            // FK is declared but never enforced (§2.1, #187). The bundled SQLite
            // has SQLITE_DEFAULT_FOREIGN_KEYS=1, so we disable it explicitly here
            // (matching what init() does) — otherwise 0004's DROP TABLE repo_hashes
            // (the FK parent) fails with "FOREIGN KEY constraint failed".
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            conn.execute_batch(include_str!("../migrations/0001_baseline.sql"))
                .unwrap();
            conn.execute_batch(include_str!("../migrations/0002_submission_origin.sql"))
                .unwrap();
            conn.execute_batch(include_str!("../migrations/0003_intern_mappings.sql"))
                .unwrap();
            // Tell rusqlite_migration that three migrations are already applied.
            conn.pragma_update(None, "user_version", 3i64).unwrap();

            // Insert hashes with explicit ids: 1, 2, 5 (gap at 3 and 4).
            conn.execute(
                "INSERT INTO repo_hashes(id, hash) VALUES(1, ?1)",
                rusqlite::params![h1.as_slice()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repo_hashes(id, hash) VALUES(2, ?1)",
                rusqlite::params![h2.as_slice()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repo_hashes(id, hash) VALUES(5, ?1)",
                rusqlite::params![h5.as_slice()],
            )
            .unwrap();

            // Insert a tag and a mapping that references hash ids 1 and 5.
            conn.execute(
                "INSERT INTO repo_tags(id, tag) VALUES(1, 'character:samus')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repo_mappings(hash_id, tag_id, status, seq)
                 VALUES(1, 1, 0, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repo_mappings(hash_id, tag_id, status, seq)
                 VALUES(5, 1, 0, 2)",
                [],
            )
            .unwrap();

            // Run migration 0004 by advancing to_latest.
            MIGRATIONS.to_latest(&mut conn).unwrap();
        }

        // Reopen via the public API — 0004 already applied, assertion must pass.
        let store = RepoStore::open(&path).expect("open must succeed after 0004");

        // Row count: still 3 (0 added by 0004 rebuild on a populated store).
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM repo_hashes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 3,
            "repo_hashes row count must be unchanged after 0004"
        );

        // id/hash pairs verbatim — including the gap.
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut stmt = store
                .conn
                .prepare("SELECT id, hash FROM repo_hashes ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], (1, h1.to_vec()), "id 1 preserved");
        assert_eq!(rows[1], (2, h2.to_vec()), "id 2 preserved");
        assert_eq!(rows[2], (5, h5.to_vec()), "id 5 preserved (gap intact)");

        // The named index must exist.
        let idx_present: bool = hash_unique_index_present(&store.conn).unwrap();
        assert!(idx_present, "repo_hashes_hash_unique must exist after 0004");

        // repo_mappings joins still resolve to the correct hashes (I9).
        let resolved: Vec<(Vec<u8>, String, i64)> = {
            let mut stmt = store
                .conn
                .prepare(
                    "SELECT h.hash, t.tag, m.hash_id
                     FROM   repo_mappings m
                     JOIN   repo_hashes h ON h.id = m.hash_id
                     JOIN   repo_tags   t ON t.id = m.tag_id
                     ORDER  BY m.seq",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(resolved.len(), 2, "both mappings survive 0004");
        assert_eq!(resolved[0].0, h1.to_vec(), "seq=1 mapping → hash id 1");
        assert_eq!(resolved[0].1, "character:samus");
        assert_eq!(resolved[0].2, 1i64, "hash_id=1 unchanged");
        assert_eq!(resolved[1].0, h5.to_vec(), "seq=2 mapping → hash id 5");
        assert_eq!(resolved[1].2, 5i64, "hash_id=5 unchanged (gap preserved)");
    }

    /// I7: `open` and `open_readonly` refuse a store whose
    /// `repo_hashes_hash_unique` was manually dropped after a successful
    /// migration. The error message must mention the seed self-heal path.
    #[test]
    fn open_and_open_readonly_refuse_store_with_dropped_unique_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo_nodrop.db");

        // Create and fully migrate the store.
        drop(RepoStore::open(&path).expect("initial open must succeed"));

        // Drop the index via a raw connection to simulate an aborted deferred seed.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("DROP INDEX {HASH_UNIQUE_INDEX}"))
                .unwrap();
        }

        // open must fail with an actionable message.
        let err = RepoStore::open(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("incomplete bridge seed"),
            "open error must mention incomplete bridge seed; got: {msg}"
        );
        assert!(
            msg.contains("seed") || msg.contains("re-seed"),
            "open error must mention the seed/re-seed path; got: {msg}"
        );

        // open_readonly must fail with the same guard.
        let err_ro = RepoStore::open_readonly(&path).unwrap_err();
        let msg_ro = format!("{err_ro:#}");
        assert!(
            msg_ro.contains("incomplete bridge seed"),
            "open_readonly error must mention incomplete bridge seed; got: {msg_ro}"
        );
    }

    /// I7 complement: `open_bulk_ingest` on the same index-dropped store must
    /// succeed — it is the one opener that legitimately operates while the index
    /// is absent (the seed manages the index lifecycle itself).
    #[test]
    fn open_bulk_ingest_tolerates_absent_unique_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo_bitest.db");

        // Create and fully migrate the store via open_bulk_ingest itself.
        drop(
            RepoStore::open_bulk_ingest(&path, false)
                .expect("initial open_bulk_ingest must succeed"),
        );

        // Drop the index.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("DROP INDEX {HASH_UNIQUE_INDEX}"))
                .unwrap();
        }

        // open_bulk_ingest must succeed even without the index.
        RepoStore::open_bulk_ingest(&path, false)
            .expect("open_bulk_ingest must tolerate absent repo_hashes_hash_unique");
    }

    // ── audit_band_digest ─────────────────────────────────────────────────────

    #[test]
    fn audit_band_digest_nonzero_count_over_seeded_rows() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"file1");
        let h2 = hash_bytes(b"file2");
        // Seed three current (status=0) mappings across two hashes.
        add(&store, &acct, &h1, "character:samus");
        add(&store, &acct, &h1, "series:metroid");
        add(&store, &acct, &h2, "maid");
        // Full-range scan (prefix_bits == 0) must return all three.
        let lo = "00".repeat(32);
        let (count, _digest) = store.audit_band_digest(&lo, 0).unwrap();
        assert_eq!(
            count, 3,
            "three current mappings must be counted; got {count}"
        );
    }

    /// #198: a local signed submission adding a NEW (hash, tag) pair carries a
    /// non-NULL origin, so it must NOT change the mirror parity audit
    /// count/digest. The audit only compares mirror-origin (origin IS NULL)
    /// rows against the Hydrus snapshot; the local row has no counterpart there.
    #[test]
    fn audit_band_digest_excludes_local_origin_new_pair() {
        use naiad_netproto::Op;
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"file1");
        let h2 = hash_bytes(b"file2");
        // Seed three mirror-origin (origin NULL) mappings across two hashes.
        add(&store, &acct, &h1, "character:samus");
        add(&store, &acct, &h1, "series:metroid");
        add(&store, &acct, &h2, "maid");

        let lo = "00".repeat(32);
        let (count_before, digest_before) = store.audit_band_digest(&lo, 0).unwrap();
        assert_eq!(count_before, 3, "baseline: three mirror rows");

        // A local signed Add of a brand-new pair, stamped with a non-NULL origin.
        let h3 = hash_bytes(b"file3");
        let tag = Tag::parse("local:submission").unwrap();
        let sub = acct.sign_with_origin(Op::Add, &h3, &tag, Some("wd14-tagger"));
        store.apply_submission(&sub).unwrap();

        // The local row exists as a current mapping…
        assert_eq!(
            store.current_mapping_count().unwrap(),
            4,
            "the local row is a current mapping"
        );
        // …but the audit set is unchanged: count and digest identical to before.
        let (count_after, digest_after) = store.audit_band_digest(&lo, 0).unwrap();
        assert_eq!(
            count_after, count_before,
            "local-origin row must be excluded from the audit count"
        );
        assert_eq!(
            digest_after, digest_before,
            "local-origin row must not perturb the audit digest"
        );
    }

    /// #198 known limitation: origin is a single last-writer-wins value per
    /// row, so a local signed Add that *re-asserts* a pair also present in the
    /// snapshot stamps that seeded row with a non-NULL origin and drops it from
    /// the audited set — the audit count then falls one short. Documented, not
    /// fixed. This test pins the current (short-by-one) behaviour.
    #[test]
    fn audit_band_digest_reassert_drops_seeded_pair() {
        use naiad_netproto::Op;
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"file1");
        let h2 = hash_bytes(b"file2");
        // Seed three mirror-origin (origin NULL) mappings.
        add(&store, &acct, &h1, "character:samus");
        add(&store, &acct, &h1, "series:metroid");
        add(&store, &acct, &h2, "maid");

        let lo = "00".repeat(32);
        let (count_before, _) = store.audit_band_digest(&lo, 0).unwrap();
        assert_eq!(count_before, 3, "baseline: three mirror rows");

        // A local signed Add RE-ASSERTING an existing seeded pair, but with a
        // non-NULL origin. Latest-writer-wins stamps the row's origin non-NULL.
        let tag = Tag::parse("character:samus").unwrap();
        let sub = acct.sign_with_origin(Op::Add, &h1, &tag, Some("wd14-tagger"));
        store.apply_submission(&sub).unwrap();

        // The row's origin is now non-NULL, confirming the mechanism.
        assert_eq!(
            origin_of(&store, &h1, "character:samus").as_deref(),
            Some("wd14-tagger"),
            "re-assert stamped the seeded row with a non-NULL origin"
        );
        // The mapping is still current — the total store is unchanged…
        assert_eq!(
            store.current_mapping_count().unwrap(),
            3,
            "re-assert did not add a new mapping"
        );
        // …but the audit count is now one short: the re-asserted pair fell out.
        let (count_after, _) = store.audit_band_digest(&lo, 0).unwrap();
        assert_eq!(
            count_after,
            count_before - 1,
            "re-asserted pair drops out of the audit (known #198 limitation)"
        );
    }

    // ── store_generation / mint_store_generation (#194) ───────────────────────

    #[test]
    fn store_generation_absent_on_fresh_store() {
        let store = RepoStore::open_in_memory().unwrap();
        assert!(
            store.store_generation().unwrap().is_none(),
            "a fresh store must have no generation"
        );
    }

    #[test]
    fn mint_store_generation_returns_32_hex_chars() {
        let store = RepoStore::open_in_memory().unwrap();
        let minted = store.mint_store_generation().unwrap();
        assert_eq!(
            minted.len(),
            32,
            "minted generation must be 32 hex chars (16 bytes): {minted}"
        );
        assert!(
            minted.chars().all(|c| c.is_ascii_hexdigit()),
            "minted generation must be all hex: {minted}"
        );
    }

    #[test]
    fn mint_store_generation_stores_and_is_readable() {
        let store = RepoStore::open_in_memory().unwrap();
        let minted = store.mint_store_generation().unwrap();
        let read = store.store_generation().unwrap();
        assert_eq!(
            read.as_deref(),
            Some(minted.as_str()),
            "store_generation must return the minted value"
        );
    }

    #[test]
    fn mint_store_generation_changes_value_on_second_call() {
        let store = RepoStore::open_in_memory().unwrap();
        let g1 = store.mint_store_generation().unwrap();
        let g2 = store.mint_store_generation().unwrap();
        // Two successive mints are overwhelmingly likely to produce different
        // values (probability of collision is ~1/2^128).
        assert_ne!(g1, g2, "two successive mints should produce different ids");
    }

    // ── distinct_tag_count (#235) ─────────────────────────────────────────────

    #[test]
    fn distinct_tag_count_returns_distinct_tags() {
        let store = RepoStore::open_in_memory().unwrap();
        // 2 distinct tags across 3 current mappings.
        store
            .apply_mappings_bulk(vec![
                ("ab".repeat(32), "character:samus".to_string(), false),
                ("cd".repeat(32), "character:samus".to_string(), false),
                ("ef".repeat(32), "series:metroid".to_string(), false),
            ])
            .unwrap();
        assert_eq!(
            store.distinct_tag_count().unwrap(),
            2,
            "3 mappings but only 2 distinct tags"
        );
    }

    // ── distinct_hash_count (#202) ────────────────────────────────────────────

    #[test]
    fn distinct_hash_count_meta_absent_then_written() {
        let store = RepoStore::open_in_memory().unwrap();
        assert_eq!(store.read_distinct_hash_count().unwrap(), None);
        store.write_distinct_hash_count(42).unwrap();
        assert_eq!(store.read_distinct_hash_count().unwrap(), Some(42));
        store.write_distinct_hash_count(7).unwrap();
        assert_eq!(store.read_distinct_hash_count().unwrap(), Some(7));
    }

    #[test]
    fn distinct_hash_count_meta_corrupt_value_is_none() {
        let store = RepoStore::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('distinct_hash_count', 'not-a-number')",
                [],
            )
            .unwrap();
        assert_eq!(store.read_distinct_hash_count().unwrap(), None);
    }

    // ── seed_ckpt (#182) ──────────────────────────────────────────────────────

    #[test]
    fn seed_ckpt_absent_then_written_then_cleared() {
        let store = RepoStore::open_in_memory().unwrap();
        assert_eq!(store.read_seed_checkpoint().unwrap(), None);

        let ck = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Current,
            high_water: 1_837_465,
            service_id: 9,
            fp: "v1:svc=9:maxhash=54217883:mapsize=207618899968".to_string(),
        };
        store.write_seed_checkpoint(&ck).unwrap();
        assert_eq!(store.read_seed_checkpoint().unwrap(), Some(ck.clone()));

        // Overwrite with a later pass/high_water.
        let ck2 = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Deleted,
            high_water: 2_000_000,
            service_id: 9,
            fp: ck.fp.clone(),
        };
        store.write_seed_checkpoint(&ck2).unwrap();
        assert_eq!(store.read_seed_checkpoint().unwrap(), Some(ck2));

        // Clear is effective and idempotent.
        store.clear_seed_checkpoint().unwrap();
        assert_eq!(store.read_seed_checkpoint().unwrap(), None);
        store.clear_seed_checkpoint().unwrap(); // no-op, no error
        assert_eq!(store.read_seed_checkpoint().unwrap(), None);
    }

    #[test]
    fn seed_ckpt_corrupt_json_is_hard_error() {
        let store = RepoStore::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT OR REPLACE INTO repo_meta(key, value) VALUES('seed_ckpt', 'not-json{')",
                [],
            )
            .unwrap();
        // Unlike distinct_hash_count (tolerant), a corrupt checkpoint must STOP the seed.
        assert!(
            store.read_seed_checkpoint().is_err(),
            "a corrupt seed_ckpt row must be a hard error, not a silent restart-from-scratch"
        );
    }

    /// Positive case for I1: a committed deferred flush persists the checkpoint.
    /// The rollback half of I1 is guaranteed structurally — the `seed_ckpt`
    /// INSERT runs on `tx` immediately before `tx.commit()`, so an uncommitted
    /// transaction cannot leave a stale checkpoint behind. No rollback test needed.
    #[test]
    fn seed_ckpt_persisted_by_deferred_flush() {
        let store = RepoStore::open_in_memory().unwrap();
        store.drop_hash_unique_index().unwrap(); // deferred append path requires no unique index
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        let ck = SeedCheckpoint {
            v: 1,
            pass: SeedPass::Current,
            high_water: 1,
            service_id: 9,
            fp: "v1:svc=9:maxhash=1:mapsize=1".to_string(),
        };
        store
            .apply_current_mappings_deferred(
                vec![("ab".repeat(32), "a:x".to_string(), false)],
                &mut caches,
                &mut seq,
                Some(&ck),
            )
            .unwrap();
        assert_eq!(
            store.read_seed_checkpoint().unwrap(),
            Some(ck),
            "a Some(ckpt) flush must persist the checkpoint in the chunk's transaction"
        );
    }

    #[test]
    fn seed_ckpt_none_writes_no_row() {
        let store = RepoStore::open_in_memory().unwrap();
        let mut caches = InternCaches::default();
        let mut seq: i64 = 0;
        store
            .apply_mappings_bulk_cached(
                vec![("cd".repeat(32), "a:y".to_string(), false)],
                &mut caches,
                &mut seq,
                None,
            )
            .unwrap();
        assert_eq!(
            store.read_seed_checkpoint().unwrap(),
            None,
            "a None flush must never create a seed_ckpt row"
        );
    }

    #[test]
    fn refresh_distinct_absent_row_is_noop() {
        let store = RepoStore::open_in_memory().unwrap();
        assert_eq!(store.refresh_distinct_hash_count().unwrap(), None);
        assert_eq!(store.read_distinct_hash_count().unwrap(), None);
    }

    #[test]
    fn refresh_distinct_overwrites_existing_row() {
        let store = RepoStore::open_in_memory().unwrap();
        store.write_distinct_hash_count(999).unwrap(); // stale
        // Apply one current mapping via the bulk path so the real count is 1.
        store
            .apply_mappings_bulk(vec![("ab".repeat(32), "a:x".to_string(), false)])
            .unwrap();
        assert_eq!(store.refresh_distinct_hash_count().unwrap(), Some(1));
        assert_eq!(store.read_distinct_hash_count().unwrap(), Some(1));
    }

    // ── replay_submissions (#194) ─────────────────────────────────────────────

    #[test]
    fn replay_submissions_empty_store_returns_zero() {
        let store = RepoStore::open_in_memory().unwrap();
        let count = store.replay_submissions().unwrap();
        assert_eq!(count, 0, "no submissions → replayed count must be 0");
    }

    #[test]
    fn replay_submissions_restores_add_with_origin() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"replay_test");
        let tag = "character:samus";

        // Apply one submission with a local origin (sign_with_origin so the
        // origin is covered by the Ed25519 signature; mutating sub.origin after
        // sign() would fail verify()).
        let sub = {
            use naiad_netproto::Op;
            let hash = naiad_core::Hash::from_bytes(*h.as_bytes());
            let tag_val = naiad_core::Tag::parse(tag).unwrap();
            acct.sign_with_origin(Op::Add, &hash, &tag_val, Some("local"))
        };
        store.apply_submission(&sub).unwrap();

        // Verify the submission went in.
        let snap_before = store.snapshot().unwrap();
        assert!(
            snap_before
                .values()
                .any(|ts| ts.iter().any(|t| t.tag == tag)),
            "mapping must be present before rebuild"
        );

        // Simulate a rebuild: clear mappings, then replay.
        store.clear_mirrored_mappings().unwrap();
        let snap_cleared = store.snapshot().unwrap();
        assert!(
            snap_cleared.is_empty(),
            "mappings must be empty after clear"
        );

        let replayed = store.replay_submissions().unwrap();
        assert_eq!(replayed, 1, "one submission must be replayed");

        // Mapping is back with correct origin.
        let snap_after = store.snapshot().unwrap();
        assert!(
            snap_after.values().any(|ts| {
                ts.iter()
                    .any(|t| t.tag == tag && t.origin.as_deref() == Some("local"))
            }),
            "replayed mapping must have origin=local"
        );
    }

    #[test]
    fn replay_submissions_delete_after_add_yields_deleted() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"del_test");

        let make_sub = |op: naiad_netproto::Op| {
            let hash = naiad_core::Hash::from_bytes(*h.as_bytes());
            let tag = naiad_core::Tag::parse("maid").unwrap();
            acct.sign_with_origin(op, &hash, &tag, Some("local"))
        };

        // Add then remove.
        store
            .apply_submission(&make_sub(naiad_netproto::Op::Add))
            .unwrap();
        store
            .apply_submission(&make_sub(naiad_netproto::Op::Remove))
            .unwrap();

        // Clear and replay.
        store.clear_mirrored_mappings().unwrap();
        let replayed = store.replay_submissions().unwrap();
        assert_eq!(replayed, 2, "two submissions (add+remove) must be replayed");

        // The final state must be deleted (not current).
        let snap = store.snapshot().unwrap();
        assert!(
            !snap.values().any(|ts| ts.iter().any(|t| t.tag == "maid")),
            "after replay a removed tag must not appear in snapshot"
        );
    }

    // ── #202: submission-path distinct_hash_count maintenance ─────────────────

    /// With a count row seeded at 0:
    ///   Add(h1,t1) fresh hash          → count becomes 1
    ///   Add(h1,t2) second tag, same h  → still 1 (no transition)
    ///   Add(h2,t1) second hash         → count becomes 2
    ///   Remove(h2,t1) last current tag → count becomes 1
    ///   re-Add(h1,t1) echo (no change) → still 1
    #[test]
    fn submission_count_tracks_distinct_current_hash_transitions() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h1 = hash_bytes(b"sub-count-h1");
        let h2 = hash_bytes(b"sub-count-h2");

        // Seed the count row at 0 so maintenance is active.
        store.write_distinct_hash_count(0).unwrap();
        assert_eq!(store.read_distinct_hash_count().unwrap(), Some(0));

        // Add(h1, t1): h1 transitions 0→1 current mapping → +1
        add(&store, &acct, &h1, "character:samus");
        assert_eq!(
            store.read_distinct_hash_count().unwrap(),
            Some(1),
            "Add(h1,t1): fresh hash must increment count"
        );

        // Add(h1, t2): h1 already has 1 current mapping → no transition → unchanged
        add(&store, &acct, &h1, "series:metroid");
        assert_eq!(
            store.read_distinct_hash_count().unwrap(),
            Some(1),
            "Add(h1,t2): second tag on same hash must not increment count"
        );

        // Add(h2, t1): h2 transitions 0→1 → +1
        add(&store, &acct, &h2, "character:samus");
        assert_eq!(
            store.read_distinct_hash_count().unwrap(),
            Some(2),
            "Add(h2,t1): second distinct hash must increment count"
        );

        // Remove(h2, t1): h2 transitions 1→0 current → -1
        remove_sub(&store, &acct, &h2, "character:samus");
        assert_eq!(
            store.read_distinct_hash_count().unwrap(),
            Some(1),
            "Remove(h2,t1): removing last current mapping must decrement count"
        );

        // echo re-Add(h1,t1): status unchanged, origin unchanged → no DB row touched → unchanged
        add(&store, &acct, &h1, "character:samus");
        assert_eq!(
            store.read_distinct_hash_count().unwrap(),
            Some(1),
            "echo re-Add(h1,t1): no real change must leave count unchanged"
        );
    }

    /// With NO count row, apply_submission must leave the row absent even after
    /// a successful Add (the serve-side fallback is responsible for populating it).
    #[test]
    fn submission_count_absent_row_stays_absent() {
        let store = RepoStore::open_in_memory().unwrap();
        let acct = Account::generate();
        let h = hash_bytes(b"sub-count-absent");

        // Confirm no row exists.
        assert_eq!(store.read_distinct_hash_count().unwrap(), None);

        add(&store, &acct, &h, "character:samus");

        assert_eq!(
            store.read_distinct_hash_count().unwrap(),
            None,
            "absent count row must stay absent after apply_submission"
        );
    }

    /// After `apply_read_only_serve_pragmas`, any DDL or DML on the connection
    /// must fail with a SQLite error, but reads must still succeed (#202, Task 7).
    #[test]
    fn query_only_pragma_rejects_writes() {
        let store = RepoStore::open_in_memory().unwrap();
        store.apply_read_only_serve_pragmas().unwrap();
        // Writes must be rejected.
        assert!(
            store.conn.execute("CREATE TABLE t(x)", []).is_err(),
            "query_only must reject DDL writes"
        );
        // Reads must still work.
        let one: i64 = store
            .conn
            .query_row("SELECT 1", [], |r| r.get(0))
            .expect("SELECT 1 must succeed even after query_only");
        assert_eq!(one, 1, "read result must be correct after query_only");
    }
}
