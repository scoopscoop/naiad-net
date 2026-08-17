//! CLI-level tests for the bridge single-writer lock (#193).
//!
//! Spawns the real `naiad-repo` binary: a second `bridge sync` must exit 4
//! while the lock is held, and `bridge seed` must be entirely un-gated.

use std::path::Path;
use std::process::Command;

use naiad_server::bridge::lock::{BridgeLock, lock_path};

// SHA_A + build_seed_fixture: copied from mirror_mode_e2e.rs — see
// that file for the schema commentary.
// (Connection requalified to rusqlite::Connection; body otherwise identical)

/// A file with this SHA-256 gets `character:samus` (current) and `meta:badtag`
/// (deleted) from the fixture; the follow-loop later adds `series:metroid`.
const SHA_A: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";

/// Build a three-file Hydrus snapshot (service id 9) sufficient for all three
/// seed phases: current + deleted mappings, the service-id->master-id maps used
/// by the follow-loop defs, and an update watermark of 0 (index 0 fully
/// processed, index 1 partial) -> seed cursor 1. Mirrors the proven
/// `build_seed_fixture` in seed.rs / schema.rs.
fn build_seed_fixture(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    let master = rusqlite::Connection::open(dir.join("client.master.db")).unwrap();
    master
        .execute_batch(
            "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
             CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
             CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
             CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
             CREATE TABLE repository_hash_id_map_9 (service_hash_id INTEGER PRIMARY KEY, hash_id INTEGER);
             CREATE TABLE repository_tag_id_map_9 (service_tag_id INTEGER PRIMARY KEY, tag_id INTEGER);",
        )
        .unwrap();
    master
        .execute(
            "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1)",
            [hex::decode(SHA_A).unwrap()],
        )
        .unwrap();
    master
        .execute_batch(
            "INSERT INTO namespaces VALUES (1, ''), (2, 'character'), (3, 'meta');
             INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'badtag');
             INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3);
             INSERT INTO repository_hash_id_map_9 VALUES (500, 1);
             INSERT INTO repository_tag_id_map_9 VALUES (800, 2);",
        )
        .unwrap();

    let client = rusqlite::Connection::open(dir.join("client.db")).unwrap();
    client
        .execute_batch(
            "CREATE TABLE repository_updates_9 (update_index INTEGER, hash_id INTEGER);
             INSERT INTO repository_updates_9 VALUES (0, 100), (1, 101);
             CREATE TABLE repository_updates_processed_9
                 (hash_id INTEGER, content_type INTEGER, processed INTEGER);
             INSERT INTO repository_updates_processed_9
                 VALUES (100, 1, 1), (101, 1, 1), (101, 2, 0);",
        )
        .unwrap();

    let mappings = rusqlite::Connection::open(dir.join("client.mappings.db")).unwrap();
    mappings
        .execute_batch(
            "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO current_mappings_9 VALUES (2, 1);
             CREATE TABLE deleted_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO deleted_mappings_9 VALUES (3, 1);",
        )
        .unwrap();
}

fn repo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_naiad-repo"))
}

#[test]
fn sync_exits_4_when_lock_held() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("repo.db");
    // Default [bridge].state_db resolves to "bridge-state.db" beside --db,
    // so the lock is dir/bridge.lock.
    let _guard = BridgeLock::acquire(&lock_path(&dir.path().join("bridge-state.db"))).unwrap();

    let out = repo_bin()
        .args(["--db", db.to_str().unwrap(), "bridge", "sync"])
        // Satisfy the ptr_key config check that precedes the lock; the
        // process must die on the lock before any network use of the key.
        .env("NAIAD_REPO_BRIDGE_PTR_KEY", "dummy-key-for-test")
        .env(
            "NAIAD_REPO_BRIDGE_STATE_DB",
            dir.path().join("bridge-state.db"),
        )
        .env("NAIAD_REPO_BRIDGE_PTR_URL", "http://127.0.0.1:1")
        .output()
        .expect("spawning naiad-repo");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(4),
        "expected exit 4, got {:?}; stderr: {stderr}",
        out.status.code()
    );
    assert!(
        stderr.contains("another bridge process appears to be running"),
        "missing contention message; stderr: {stderr}"
    );
}

#[test]
fn seed_is_never_gated_by_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("repo.db");
    let snapshot = dir.path().join("snapshot");
    build_seed_fixture(&snapshot);

    let _guard = BridgeLock::acquire(&lock_path(&dir.path().join("bridge-state.db"))).unwrap();

    let out = repo_bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "bridge",
            "seed",
            snapshot.to_str().unwrap(),
            "--service-id",
            "9",
        ])
        .env(
            "NAIAD_REPO_BRIDGE_STATE_DB",
            dir.path().join("bridge-state.db"),
        )
        .env("NAIAD_REPO_BRIDGE_PTR_URL", "http://127.0.0.1:1")
        .output()
        .expect("spawning naiad-repo");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "seed must succeed while lock is held (hard constraint); exit {:?}; stderr: {stderr}",
        out.status.code()
    );
}
