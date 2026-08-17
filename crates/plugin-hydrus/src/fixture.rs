//! Build a synthetic Hydrus snapshot on disk.
//!
//! Test support that lives in the library rather than a `#[cfg(test)]` module
//! because it is needed across crate boundaries (the server's snapshot-backend
//! tests and the daemon's end-to-end tests) and copying the DDL would let the
//! copies drift from [`crate::HydrusDb`]'s expectations.
//!
//! Gated behind the off-by-default `fixture` feature: this is the only code in
//! a read-only crate that writes a Hydrus-shaped database, so it must not be
//! reachable from a normal build. Consumers opt in from `[dev-dependencies]`:
//!
//! ```toml
//! [dev-dependencies]
//! naiad-plugin-hydrus = { workspace = true, features = ["fixture"] }
//! ```
//!
//! Cargo's v2+ feature resolver does not unify dev-dependency features into
//! non-test builds, so a crate can depend on this one normally and still get
//! `fixture` only when compiling its tests.
//!
//! The schema is the subset [`crate::HydrusDb`] reads: `master.hashes`,
//! `master.namespaces`, `master.subtags`, `master.tags` and
//! `mappings.current_mappings_<svc>`.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, params};

use naiad_plugin::{PluginError, Result};

fn err(e: impl std::fmt::Display) -> PluginError {
    PluginError(format!("hydrus fixture: {e}"))
}

/// Hydrus `services.service_type` for a remote tag repository (e.g. the PTR).
///
/// Mirrors [`crate::schema::SERVICE_TYPE_TAG_REPOSITORY`]; kept here so fixtures
/// read as self-describing Hydrus rows.
pub const SERVICE_TYPE_TAG_REPOSITORY: i64 = crate::schema::SERVICE_TYPE_TAG_REPOSITORY;
/// Hydrus `services.service_type` for a local tag service (e.g. "my tags").
pub const SERVICE_TYPE_LOCAL_TAG: i64 = crate::schema::SERVICE_TYPE_LOCAL_TAG;

/// One tag service to synthesise into a snapshot via [`write_snapshot_with_services`].
///
/// Produces one row in the Hydrus `services` table (in `client.db`) and one
/// `current_mappings_<service_id>` table (in `client.mappings.db`) holding
/// `mappings`. Namespaces, subtags, tags and hashes are interned across all
/// services in the same snapshot, exactly as Hydrus shares its master tables.
pub struct SnapshotService<'a> {
    /// Hydrus `service_id` (the `services` table's primary key).
    pub service_id: i64,
    /// Hydrus `service_type` (e.g. [`SERVICE_TYPE_TAG_REPOSITORY`] or
    /// [`SERVICE_TYPE_LOCAL_TAG`]).
    pub service_type: i64,
    /// Human-readable service name (e.g. `"public tag repository"`).
    pub name: &'a str,
    /// `(sha256_hex, tag)` current mappings for this service; may be empty (an
    /// empty local tag service is exactly the #167 trap).
    pub mappings: &'a [(&'a str, &'a str)],
}

/// Interns master-table ids (hashes, namespaces, subtags, tags) so repeated
/// values collapse to one row, matching Hydrus' shared `client.master.db`.
#[derive(Default)]
struct Interner {
    next_hash_id: i64,
    next_ns_id: i64,
    next_sub_id: i64,
    next_tag_id: i64,
    hash_ids: HashMap<String, i64>,
    ns_ids: HashMap<String, i64>,
    sub_ids: HashMap<String, i64>,
    tag_ids: HashMap<(i64, i64), i64>,
}

impl Interner {
    fn new() -> Self {
        Self {
            next_hash_id: 1,
            next_ns_id: 1,
            next_sub_id: 1,
            next_tag_id: 1,
            ..Self::default()
        }
    }

    /// Intern `sha_hex` into `master.hashes`, returning its `hash_id`.
    fn hash_id(&mut self, master: &Connection, sha_hex: &str) -> Result<i64> {
        let key = sha_hex.to_lowercase();
        if let Some(id) = self.hash_ids.get(&key) {
            return Ok(*id);
        }
        let bytes = hex::decode(&key).map_err(err)?;
        let id = self.next_hash_id;
        self.next_hash_id += 1;
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (?1, ?2)",
                params![id, bytes],
            )
            .map_err(err)?;
        self.hash_ids.insert(key, id);
        Ok(id)
    }

    /// Intern `tag` (split on the first `':'`) into `master.namespaces`,
    /// `master.subtags` and `master.tags`, returning its `tag_id`.
    fn tag_id(&mut self, master: &Connection, tag: &str) -> Result<i64> {
        let (ns, sub) = match tag.split_once(':') {
            Some((n, s)) => (n.to_string(), s.to_string()),
            None => (String::new(), tag.to_string()),
        };
        let ns_id = match self.ns_ids.get(&ns) {
            Some(id) => *id,
            None => {
                let id = self.next_ns_id;
                self.next_ns_id += 1;
                master
                    .execute(
                        "INSERT INTO namespaces (namespace_id, namespace) VALUES (?1, ?2)",
                        params![id, ns],
                    )
                    .map_err(err)?;
                self.ns_ids.insert(ns.clone(), id);
                id
            }
        };
        let sub_id = match self.sub_ids.get(&sub) {
            Some(id) => *id,
            None => {
                let id = self.next_sub_id;
                self.next_sub_id += 1;
                master
                    .execute(
                        "INSERT INTO subtags (subtag_id, subtag) VALUES (?1, ?2)",
                        params![id, sub],
                    )
                    .map_err(err)?;
                self.sub_ids.insert(sub.clone(), id);
                id
            }
        };
        match self.tag_ids.get(&(ns_id, sub_id)) {
            Some(id) => Ok(*id),
            None => {
                let id = self.next_tag_id;
                self.next_tag_id += 1;
                master
                    .execute(
                        "INSERT INTO tags (tag_id, namespace_id, subtag_id) VALUES (?1, ?2, ?3)",
                        params![id, ns_id, sub_id],
                    )
                    .map_err(err)?;
                self.tag_ids.insert((ns_id, sub_id), id);
                Ok(id)
            }
        }
    }
}

fn create_master(master: &Connection) -> Result<()> {
    master
        .execute_batch(
            "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
             CREATE UNIQUE INDEX hashes_hash ON hashes (hash);
             CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
             CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
             CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);",
        )
        .map_err(err)
}

/// Write a Hydrus snapshot into `dir` that carries a real `services` table plus
/// one `current_mappings_<service_id>` table per entry in `services`.
///
/// This is the multi-service counterpart to [`write_snapshot`]: it models a full
/// Hydrus **client** database, where low service ids are local tag services and
/// the actual tag repository (the PTR) sits at a higher id. It is the fixture
/// for #167 — auto-discovery must pick the tag *repository*, not the lowest id.
///
/// The `services` table matches the subset [`crate::schema::HydrusDb`] reads:
/// `service_id` and `service_type` (plus filler columns Hydrus really has).
///
/// # Errors
/// Returns an error if any file cannot be created or any statement fails.
pub fn write_snapshot_with_services(dir: &Path, services: &[SnapshotService]) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(err)?;

    // client.db carries the Hydrus `services` table (the source of truth for a
    // service's type). Column shape mirrors a real Hydrus client.db.
    let client = Connection::open(dir.join("client.db")).map_err(err)?;
    client
        .execute_batch(
            "CREATE TABLE services (
                 service_id INTEGER PRIMARY KEY,
                 service_key BLOB,
                 service_type INTEGER,
                 name TEXT,
                 dictionary_string TEXT
             );",
        )
        .map_err(err)?;
    for s in services {
        client
            .execute(
                "INSERT INTO services (service_id, service_key, service_type, name, dictionary_string)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![s.service_id, vec![0u8; 32], s.service_type, s.name, ""],
            )
            .map_err(err)?;
    }

    let master = Connection::open(dir.join("client.master.db")).map_err(err)?;
    create_master(&master)?;

    let mappings_db = Connection::open(dir.join("client.mappings.db")).map_err(err)?;
    let mut interner = Interner::new();
    for s in services {
        mappings_db
            .execute_batch(&format!(
                "CREATE TABLE current_mappings_{} (tag_id INTEGER, hash_id INTEGER);",
                s.service_id
            ))
            .map_err(err)?;
        for (sha_hex, tag) in s.mappings {
            let hash_id = interner.hash_id(&master, sha_hex)?;
            let tag_id = interner.tag_id(&master, tag)?;
            mappings_db
                .execute(
                    &format!(
                        "INSERT INTO current_mappings_{} (tag_id, hash_id) VALUES (?1, ?2)",
                        s.service_id
                    ),
                    params![tag_id, hash_id],
                )
                .map_err(err)?;
        }
    }
    Ok(())
}

/// Write a COMPLETE PTR-shaped snapshot for exercising the sidecar seed: every
/// current mapping's internal tag/hash id has a `repository_*_id_map` row, and a
/// fully-processed `repository_updates` watermark is present. Service `svc`.
///
/// Hashes (by prefix): 0x11.. (h1), 0x33.. (h2), 0xaa.. (h3), 0xbb.. (h4).
/// Mappings:
/// - h1 → {maid(800), character:samus(801)}  (two parseable tags)
/// - h2 → {maid(800)}                         (one parseable tag)
/// - h3 → none                                (no current_mappings row)
/// - h4 → {service_tag_id=802}               (sole tag is unparseable: empty subtag)
///
/// The unparseable tag (802) exercises the F10 pack-time filter: the translation map built
/// by `stream_ptr_tag_translation` omits it, so h4 ends up with no `bucket_map` row after seeding.
///
/// # Errors
/// Returns an error if any file cannot be created or written.
pub fn write_ptr_seed_fixture(dir: &Path, svc: i64) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(err)?;
    let h1 = format!("11{}", "00".repeat(31));
    let h2 = format!("33{}", "00".repeat(31));
    let h3 = format!("aa{}", "00".repeat(31));
    let h4 = format!("bb{}", "00".repeat(31));

    let master = Connection::open(dir.join("client.master.db")).map_err(err)?;
    master
        .execute_batch(&format!(
            "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
             CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
             CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
             CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
             CREATE TABLE repository_hash_id_map_{svc} (service_hash_id INTEGER PRIMARY KEY, hash_id INTEGER);
             CREATE TABLE repository_tag_id_map_{svc} (service_tag_id INTEGER PRIMARY KEY, tag_id INTEGER);
             INSERT INTO namespaces VALUES (1, ''), (2, 'character');
             INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, '');
             INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 1, 3);
             INSERT INTO repository_tag_id_map_{svc} VALUES (800, 1), (801, 2), (802, 3);
             INSERT INTO repository_hash_id_map_{svc} VALUES (500, 1), (501, 2), (502, 3), (503, 4);"
        ))
        .map_err(err)?;
    for (hid, hx) in [(1i64, &h1), (2, &h2), (3, &h3), (4, &h4)] {
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (?1, ?2)",
                rusqlite::params![hid, hex::decode(hx).map_err(err)?],
            )
            .map_err(err)?;
    }

    let client = Connection::open(dir.join("client.db")).map_err(err)?;
    client
        .execute_batch(&format!(
            "CREATE TABLE repository_updates_{svc} (update_index INTEGER, hash_id INTEGER);
             INSERT INTO repository_updates_{svc} VALUES (0, 100);
             CREATE TABLE repository_updates_processed_{svc}
                 (hash_id INTEGER, content_type INTEGER, processed INTEGER);
             INSERT INTO repository_updates_processed_{svc} VALUES (100, 1, 1);"
        ))
        .map_err(err)?;

    let mappings = Connection::open(dir.join("client.mappings.db")).map_err(err)?;
    // tag_id, hash_id: h1→{1,2}, h2→{1}, h3→none, h4→{3 (unparseable)}.
    mappings
        .execute_batch(&format!(
            "CREATE TABLE current_mappings_{svc} (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO current_mappings_{svc} VALUES (1, 1), (2, 1), (1, 2), (3, 4);"
        ))
        .map_err(err)?;
    Ok(())
}

/// Write a minimal snapshot exercising the sibling/parent relation reader
/// (`import_relations_only`, #225 seed backfill). Master tags: 1=`maid`,
/// 2=`character:samus`, 3=`character:samus_aran`. For service `svc`:
///
/// - `current_tag_siblings_<svc>`: (3 → 2)  — samus_aran aliases samus (current)
/// - `current_tag_parents_<svc>`:  (2 → 1)  — samus implies maid (current)
/// - `deleted_tag_siblings_<svc>`: (1 → 2)  — a tombstoned sibling (deleted)
///
/// `client.mappings.db` is created empty (required for `HydrusDb::open`); no
/// mapping rows are written. `deleted_tag_parents_<svc>` is intentionally absent
/// so the reader's missing-table tolerance is exercised.
///
/// # Errors
/// Returns an error if any file cannot be created or written.
pub fn write_relations_fixture(dir: &Path, svc: i64) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(err)?;
    let master = Connection::open(dir.join("client.master.db")).map_err(err)?;
    master
        .execute_batch(
            "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
             CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
             CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
             CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
             INSERT INTO namespaces VALUES (1, ''), (2, 'character');
             INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'samus_aran');
             INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3);",
        )
        .map_err(err)?;
    let client = Connection::open(dir.join("client.db")).map_err(err)?;
    client
        .execute_batch(&format!(
            "CREATE TABLE current_tag_siblings_{svc} (bad_tag_id INTEGER, good_tag_id INTEGER);
             INSERT INTO current_tag_siblings_{svc} VALUES (3, 2);
             CREATE TABLE current_tag_parents_{svc} (child_tag_id INTEGER, parent_tag_id INTEGER);
             INSERT INTO current_tag_parents_{svc} VALUES (2, 1);
             CREATE TABLE deleted_tag_siblings_{svc} (bad_tag_id INTEGER, good_tag_id INTEGER);
             INSERT INTO deleted_tag_siblings_{svc} VALUES (1, 2);"
        ))
        .map_err(err)?;
    // Must exist for HydrusDb::open; content irrelevant for a relations-only read.
    Connection::open(dir.join("client.mappings.db")).map_err(err)?;
    Ok(())
}

/// Write a Hydrus snapshot into `dir` holding `mappings`, each a
/// `(sha256_hex, tag)` pair. `tag` is split on the first `':'` into
/// namespace and subtag; a tag with no colon becomes a bare subtag with an
/// empty namespace (the Hydrus convention).
///
/// Creates `client.db` (empty — `HydrusDb::open` requires the file to exist),
/// `client.master.db` and `client.mappings.db`. Repeated hashes and repeated
/// tags are interned, so passing the same hash twice yields one `hashes` row
/// with two mappings. `mappings` may be empty; the
/// `current_mappings_<service_id>` table is still created.
/// A repeated `(hash, tag)` pair emits a second identical `current_mappings`
/// row — callers that need deduplicated output (e.g. `SnapshotBackend::bucket`)
/// must sort and dedup the results themselves.
///
/// # Errors
/// Returns an error if any file cannot be created or any statement fails.
pub fn write_snapshot(dir: &Path, service_id: i64, mappings: &[(&str, &str)]) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(err)?;

    // client.db must exist for HydrusDb::open; no tables are needed.
    Connection::open(dir.join("client.db")).map_err(err)?;

    let master = Connection::open(dir.join("client.master.db")).map_err(err)?;
    create_master(&master)?;

    let mappings_db = Connection::open(dir.join("client.mappings.db")).map_err(err)?;
    mappings_db
        .execute_batch(&format!(
            "CREATE TABLE current_mappings_{service_id} (tag_id INTEGER, hash_id INTEGER);"
        ))
        .map_err(err)?;

    let mut interner = Interner::new();
    for (sha_hex, tag) in mappings {
        let hash_id = interner.hash_id(&master, sha_hex)?;
        let tag_id = interner.tag_id(&master, tag)?;
        mappings_db
            .execute(
                &format!(
                    "INSERT INTO current_mappings_{service_id} (tag_id, hash_id) VALUES (?1, ?2)"
                ),
                params![tag_id, hash_id],
            )
            .map_err(err)?;
    }

    Ok(())
}
