//! Bridge-private SQLite state store (`bridge-state.db`).
//!
//! Deliberately separate from `bridge-repo.db` (the naiad-server `RepoStore`)
//! so that bridge-private tables never touch the server crate's rusqlite_migration
//! `user_version` space.

use std::path::Path;

use std::collections::HashMap;

use anyhow::Context as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

/// Maximum number of bind variables per `IN (...)` query.
///
/// Safe under every SQLite `SQLITE_MAX_VARIABLE_NUMBER` build default,
/// including the legacy 999-variable limit in older builds.
const DEFS_LOOKUP_CHUNK: usize = 900;

/// A handle to the bridge's private state database.
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Open (creating if absent) the state database at `path`.
    ///
    /// Creates all required tables idempotently and enables WAL mode.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be opened or schema setup fails.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)
            .with_context(|| format!("opening state db {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .context("setting busy timeout")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL mode")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS defs_hashes (
                service_hash_id INTEGER PRIMARY KEY,
                sha256 TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS defs_tags (
                service_tag_id INTEGER PRIMARY KEY,
                tag TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sync_state (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            -- Designed for future push; unused in v1 pull-only.
            CREATE TABLE IF NOT EXISTS pending_relay (
                id INTEGER PRIMARY KEY,
                op TEXT NOT NULL,
                hash TEXT NOT NULL,
                tag TEXT NOT NULL,
                author TEXT,
                created_at INTEGER NOT NULL,
                relayed INTEGER NOT NULL DEFAULT 0
            );
            ",
        )
        .context("creating state schema")?;
        Ok(Self { conn })
    }

    /// Open the state database **read-only** at `path`.
    ///
    /// Sets the same 10-second busy timeout as [`Self::open`] but does NOT run
    /// WAL pragma or `CREATE TABLE` DDL — both are writes that can hit
    /// `SQLITE_BUSY` against a bridge mid-transaction.
    ///
    /// # Errors
    /// Returns an error if `path` does not exist (read-only cannot create the
    /// file) or if the connection cannot be opened.
    pub fn open_readonly(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening state db read-only {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .context("setting busy timeout on read-only state db")?;
        Ok(Self { conn })
    }

    /// Returns the cursor for the next PTR update file index to fetch.
    ///
    /// Returns `0` when the key has never been set.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn next_update_index(&self) -> anyhow::Result<u64> {
        let val: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'next_update_index'",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("querying next_update_index")?;
        match val {
            None => Ok(0),
            Some(s) => s
                .parse::<u64>()
                .with_context(|| format!("parsing next_update_index value {:?}", s)),
        }
    }

    /// Persist the cursor for the next PTR update file index to fetch.
    ///
    /// # Errors
    /// Returns an error if the upsert fails.
    pub fn set_next_update_index(&self, index: u64) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO sync_state (key, value) VALUES ('next_update_index', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![index.to_string()],
            )
            .context("setting next_update_index")?;
        Ok(())
    }

    /// Read an arbitrary flag from `sync_state`.
    ///
    /// Returns `None` if the key has never been set.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn get_flag(&self, key: &str) -> anyhow::Result<Option<String>> {
        let val: Option<String> = self
            .conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .with_context(|| format!("querying flag {key:?}"))?;
        Ok(val)
    }

    /// Write (upsert) an arbitrary flag into `sync_state`.
    ///
    /// # Errors
    /// Returns an error if the upsert fails.
    pub fn set_flag(&self, key: &str, val: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, val],
            )
            .with_context(|| format!("setting flag {key:?}"))?;
        Ok(())
    }

    /// Delete the `seed_phase_mappings` key from `sync_state` so the mappings
    /// phase of `bridge seed` runs again from scratch.
    ///
    /// Called by `bridge seed --rebuild` before clearing and re-seeding
    /// `repo_mappings`. The mapping-seq cursor (MAX(seq)) resets automatically
    /// when `repo_mappings` is cleared, so only the phase flag needs resetting.
    ///
    /// # Errors
    /// Returns an error if the delete fails.
    pub fn reset_seed_phase_mappings(&self) -> anyhow::Result<()> {
        self.conn
            .execute(
                "DELETE FROM sync_state WHERE key = 'seed_phase_mappings'",
                [],
            )
            .context("reset_seed_phase_mappings")?;
        Ok(())
    }

    /// Bulk-upsert `(service_hash_id, sha256)` rows into `defs_hashes` in a
    /// single transaction. Uses `INSERT OR REPLACE` so re-seeding is safe.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn insert_defs_hashes(&self, rows: &[(u64, String)]) -> anyhow::Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("starting defs_hashes transaction")?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO defs_hashes (service_hash_id, sha256)
                     VALUES (?1, ?2)",
                )
                .context("preparing insert_defs_hashes")?;
            for (id, sha) in rows {
                stmt.execute(rusqlite::params![*id as i64, sha])
                    .context("inserting defs_hash row")?;
            }
        }
        tx.commit().context("committing defs_hashes")?;
        Ok(())
    }

    /// Bulk-upsert `(service_tag_id, tag)` rows into `defs_tags` in a single
    /// transaction. Uses `INSERT OR REPLACE` so re-seeding is safe.
    ///
    /// # Errors
    /// Returns an error if any statement fails.
    pub fn insert_defs_tags(&self, rows: &[(u64, String)]) -> anyhow::Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("starting defs_tags transaction")?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO defs_tags (service_tag_id, tag)
                     VALUES (?1, ?2)",
                )
                .context("preparing insert_defs_tags")?;
            for (id, tag) in rows {
                stmt.execute(rusqlite::params![*id as i64, tag])
                    .context("inserting defs_tag row")?;
            }
        }
        tx.commit().context("committing defs_tags")?;
        Ok(())
    }

    /// Look up the sha256 hex for a `service_hash_id`.
    ///
    /// Returns `Ok(None)` when the id is not yet in `defs_hashes`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn defs_hash(&self, id: u64) -> anyhow::Result<Option<String>> {
        let val: Option<String> = self
            .conn
            .query_row(
                "SELECT sha256 FROM defs_hashes WHERE service_hash_id = ?1",
                rusqlite::params![id as i64],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("querying defs_hashes for id {id}"))?;
        Ok(val)
    }

    /// Look up the tag string for a `service_tag_id`.
    ///
    /// Returns `Ok(None)` when the id is not yet in `defs_tags`.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn defs_tag(&self, id: u64) -> anyhow::Result<Option<String>> {
        let val: Option<String> = self
            .conn
            .query_row(
                "SELECT tag FROM defs_tags WHERE service_tag_id = ?1",
                rusqlite::params![id as i64],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("querying defs_tags for id {id}"))?;
        Ok(val)
    }

    /// Batch-resolve `service_hash_id → sha256` for a set of ids.
    ///
    /// Ids absent from `defs_hashes` are simply absent from the returned map.
    /// An empty `ids` slice returns an empty map without querying the database.
    /// Large slices are split into chunks of [`DEFS_LOOKUP_CHUNK`] to stay
    /// under SQLite's bind-variable limit.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn defs_hashes_for(&self, ids: &[u64]) -> anyhow::Result<HashMap<u64, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut map = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(DEFS_LOOKUP_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT service_hash_id, sha256 FROM defs_hashes WHERE service_hash_id IN ({placeholders})"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .context("preparing defs_hashes_for")?;
            let params = rusqlite::params_from_iter(chunk.iter().map(|&id| id as i64));
            let rows = stmt
                .query_map(params, |r| {
                    Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
                })
                .context("querying defs_hashes_for")?;
            for row in rows {
                let (id, sha) = row.context("reading defs_hashes_for row")?;
                map.insert(id, sha);
            }
        }
        Ok(map)
    }

    /// Batch-resolve `service_tag_id → tag` for a set of ids.
    ///
    /// Ids absent from `defs_tags` are simply absent from the returned map.
    /// An empty `ids` slice returns an empty map without querying the database.
    /// Large slices are split into chunks of [`DEFS_LOOKUP_CHUNK`] to stay
    /// under SQLite's bind-variable limit.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn defs_tags_for(&self, ids: &[u64]) -> anyhow::Result<HashMap<u64, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut map = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(DEFS_LOOKUP_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT service_tag_id, tag FROM defs_tags WHERE service_tag_id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql).context("preparing defs_tags_for")?;
            let params = rusqlite::params_from_iter(chunk.iter().map(|&id| id as i64));
            let rows = stmt
                .query_map(params, |r| {
                    Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
                })
                .context("querying defs_tags_for")?;
            for row in rows {
                let (id, tag) = row.context("reading defs_tags_for row")?;
                map.insert(id, tag);
            }
        }
        Ok(map)
    }

    /// Expose a reference to the underlying connection (crate-internal use).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_cursor_defaults_zero_then_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge-state.db");
        {
            let st = StateDb::open(&path).unwrap();
            assert_eq!(st.next_update_index().unwrap(), 0, "unset cursor reads 0");
            st.set_next_update_index(42).unwrap();
        }
        let st = StateDb::open(&path).unwrap();
        assert_eq!(st.next_update_index().unwrap(), 42);
    }

    #[test]
    fn open_readonly_reads_persisted_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge-state.db");
        // Create and write via the normal open.
        let st = StateDb::open(&path).unwrap();
        st.set_next_update_index(99).unwrap();
        drop(st);
        // Re-open read-only: must see the persisted value.
        let ro = StateDb::open_readonly(&path).unwrap();
        assert_eq!(ro.next_update_index().unwrap(), 99);
    }

    #[test]
    fn open_readonly_on_nonexistent_path_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.db");
        let result = StateDb::open_readonly(&path);
        assert!(result.is_err(), "read-only open on missing file must error");
    }

    #[test]
    fn state_schema_tables_exist() {
        let dir = tempfile::tempdir().unwrap();
        let st = StateDb::open(dir.path().join("s.db")).unwrap();
        for t in ["defs_hashes", "defs_tags", "sync_state", "pending_relay"] {
            let n: i64 = st
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {t} must exist");
        }
    }

    /// Insert 2500 rows (> 2 full chunks of 900) and verify all are resolved.
    #[test]
    fn defs_lookup_large_batch_multi_chunk() {
        const N: u64 = 2500;
        let dir = tempfile::tempdir().unwrap();
        let st = StateDb::open(dir.path().join("s.db")).unwrap();

        let hash_rows: Vec<(u64, String)> = (0..N).map(|i| (i, format!("{i:064x}"))).collect();
        let tag_rows: Vec<(u64, String)> = (0..N).map(|i| (i, format!("tag:{i}"))).collect();
        st.insert_defs_hashes(&hash_rows).unwrap();
        st.insert_defs_tags(&tag_rows).unwrap();

        let ids: Vec<u64> = (0..N).collect();
        let hashes = st.defs_hashes_for(&ids).unwrap();
        let tags = st.defs_tags_for(&ids).unwrap();

        assert_eq!(hashes.len(), N as usize, "all hash ids resolved");
        assert_eq!(tags.len(), N as usize, "all tag ids resolved");
        for i in 0..N {
            assert_eq!(hashes[&i], format!("{i:064x}"), "hash value for id {i}");
            assert_eq!(tags[&i], format!("tag:{i}"), "tag value for id {i}");
        }
    }

    /// Verify chunk boundaries: exactly 900 ids (one full chunk) and 901 ids
    /// (one full chunk + one overflow) both resolve without error.
    #[test]
    fn defs_lookup_chunk_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let st = StateDb::open(dir.path().join("s.db")).unwrap();

        // Insert 901 rows for both tables.
        let hash_rows: Vec<(u64, String)> = (0..901).map(|i| (i, format!("{i:064x}"))).collect();
        let tag_rows: Vec<(u64, String)> = (0..901).map(|i| (i, format!("tag:{i}"))).collect();
        st.insert_defs_hashes(&hash_rows).unwrap();
        st.insert_defs_tags(&tag_rows).unwrap();

        // Exactly 900 ids.
        let ids_900: Vec<u64> = (0..900).collect();
        assert_eq!(st.defs_hashes_for(&ids_900).unwrap().len(), 900);
        assert_eq!(st.defs_tags_for(&ids_900).unwrap().len(), 900);

        // 901 ids — one element spills into the next chunk.
        let ids_901: Vec<u64> = (0..901).collect();
        assert_eq!(st.defs_hashes_for(&ids_901).unwrap().len(), 901);
        assert_eq!(st.defs_tags_for(&ids_901).unwrap().len(), 901);
    }
}
