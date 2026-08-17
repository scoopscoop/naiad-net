//! End-to-end acceptance (issues #124, #128, #209): a naiad client pulls tags
//! from a served sha256-domain repo and lands them on BLAKE3 file identities.
//! Unlike sha256_domain_pull.rs (in-memory store), this drives an on-disk repo
//! over a real HTTP bind and probes /repo/caps via RepoClient — the wire path
//! the absorbed bridge serve setup produces.
//!
//! Also contains the sidecar-mode E2E test (#209): seeds a compact SQLite
//! sidecar index from `write_ptr_seed_fixture`, applies a PTR sync update,
//! boots a dual-domain server (BLAKE3 native + SHA-256 sidecar), and asserts
//! that a naiad client pull lands all three tags on the BLAKE3 file identity.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use naiad_core::{FileRecord, hash_reader_dual};
use naiad_daemon::{CapsCache, pull_repo};
use naiad_db::Db;
use naiad_netproto::{HashDomain, RepoClient};
use naiad_server::RepoStore;

struct ServedRepo {
    addr: SocketAddr,
    _handle: JoinHandle<()>,
}

/// Serve an on-disk sha256-domain `RepoStore` over a background thread, exactly
/// as `naiad-repo serve` does for a bridge node (`HashDomain::Sha256`, k=1).
fn spawn_sha256_repo(store: RepoStore, k: u64) -> ServedRepo {
    let store = Arc::new(Mutex::new(store));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            axum::serve(
                listener,
                naiad_server::app_split(store, None, k, None, None, HashDomain::Sha256)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve repo");
        });
    });
    ServedRepo {
        addr: rx.recv().expect("repo failed to bind"),
        _handle: handle,
    }
}

#[test]
fn client_pulls_from_bridge_serve_path() {
    let content = b"bridge-e2e-file";
    let (blake3_hash, sha256_hex) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual content");

    // On-disk repo db, seeded with sha256-keyed mappings (as `bridge seed`
    // would). Two hashes in the store. k=3 > count(2) → advise() returns
    // WholeRepo so the client uses /repo/snapshot, not /repo/buckets.
    // #195: mirror mode now enforces a floor on bucketed sha256 queries;
    // this test uses WholeRepo mode to avoid that path (the floor is for
    // production mirrors with many millions of hashes, not tiny test fixtures).
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("bridge-repo.db");
    let store = RepoStore::open(&db_path).expect("open bridge-repo.db");
    store
        .apply_mappings_bulk(vec![(
            sha256_hex.clone(),
            "character:samus".to_string(),
            false,
        )])
        .expect("apply_mappings_bulk primary");
    let (_, sha256_filler) =
        hash_reader_dual(&b"filler-bridge"[..]).expect("hash_reader_dual filler");
    store
        .apply_mappings_bulk(vec![(sha256_filler, "filler:tag".to_string(), false)])
        .expect("apply_mappings_bulk filler");

    // k=3: count(2) < k(3) → advise() returns WholeRepo, no bucketed floor.
    let server = spawn_sha256_repo(store, 3);
    let url = format!("http://{}", server.addr);

    // The served path must advertise the SHA-256 domain in caps.
    let probe = RepoClient::new(&url);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let caps = loop {
        match probe.fetch_caps() {
            Ok(c) => break c,
            Err(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "repo did not start within 5 s"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    assert_eq!(
        caps.hash_domain,
        HashDomain::Sha256,
        "bridge serve path must advertise HashDomain::Sha256"
    );

    // ---------- Client DB ----------
    let db = Db::open_in_memory().expect("client Db");
    db.insert_file(
        &FileRecord::new(
            blake3_hash,
            "/lib/bridge.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_hex.clone()),
        1,
    )
    .expect("insert file A");
    let (null_sha_blake3, _) =
        hash_reader_dual(&b"no-sha256-bridge"[..]).expect("hash_reader_dual null-sha");
    db.insert_file(
        &FileRecord::new(null_sha_blake3, "/lib/null-bridge.png".into(), 16, Some(1)),
        2,
    )
    .expect("insert null-sha file");

    db.add_shared_service("bridge-e2e", &url, None)
        .expect("add_shared_service");
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "bridge-e2e", 256, None)
        .expect("pull_repo must succeed against bridge serve path");
    assert!(stats.matched_files >= 1, "at least one file must match");

    let db_lock = db.lock().unwrap();

    let fid = db_lock
        .file_id_by_hash(&blake3_hash)
        .expect("file_id_by_hash for file A")
        .expect("file A must be in the library");
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .expect("tags_of file A")
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags.contains(&"character:samus".to_string()),
        "tag must land on the blake3 identity via bridge serve path; got {tags:?}"
    );

    let null_fid = db_lock
        .file_id_by_hash(&null_sha_blake3)
        .expect("file_id_by_hash for null file")
        .expect("null-sha file must be in the library");
    let null_tags = db_lock.tags_of(null_fid).expect("tags_of null file");
    assert!(
        null_tags.is_empty(),
        "NULL-sha file must receive no tags; got {null_tags:?}"
    );
}

/// Serve an on-disk **dual-domain** repo: a native BLAKE3 `RepoStore` plus a
/// snapshot-mode SHA-256 backend reading a fixture Hydrus snapshot. This is
/// exactly what `naiad-repo serve` builds from
/// `[bridge] enabled = true, mode = "snapshot", snapshot_dir = ...`.
fn spawn_dual_domain_repo(store: RepoStore, snapshot_dir: &std::path::Path, k: u64) -> ServedRepo {
    let backend =
        naiad_server::SnapshotBackend::open(snapshot_dir, Some(9)).expect("open snapshot backend");
    let domains = naiad_server::DomainConfig {
        native: HashDomain::Blake3,
        added_sha256: Some(Arc::new(backend) as Arc<dyn naiad_server::Sha256Backend>),
        max_query_bits: 256,
        min_query_bits: 8,
    };
    let store = Arc::new(Mutex::new(store));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            axum::serve(
                listener,
                naiad_server::app_domains(store, None, k, None, None, domains)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve repo");
        });
    });
    ServedRepo {
        addr: rx.recv().expect("repo failed to bind"),
        _handle: handle,
    }
}

fn wait_for_caps(url: &str) -> naiad_netproto::Caps {
    let probe = RepoClient::new(url);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match probe.fetch_caps() {
            Ok(c) => return c,
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "repo did not start within 10 s: {e:#}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// THE PHASE 1 HEADLINE (spec §5): a repo backed by nothing but a downloaded
/// Hydrus snapshot serves PTR tags to a naiad daemon, and they land on BLAKE3
/// file identities. There is **no seed step anywhere in this test** — the only
/// SHA-256 data source is the three read-only `client*.db` files.
#[test]
fn snapshot_mode_serves_ptr_tags_with_no_seed() {
    let content = b"snapshot-mode-file";
    let (blake3_hash, sha256_hex) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual content");

    // The "downloaded PTR snapshot": three read-only Hydrus files, nothing else.
    let snapshot = tempfile::tempdir().expect("snapshot tempdir");
    naiad_plugin_hydrus::fixture::write_snapshot(
        snapshot.path(),
        9,
        &[
            (&sha256_hex, "character:samus"),
            (&sha256_hex, "series:metroid"),
        ],
    )
    .expect("write snapshot fixture");

    // The repo's own store is a brand-new, EMPTY native BLAKE3 store.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("dual-repo.db");
    let store = RepoStore::open(&db_path).expect("open dual-repo.db");

    let server = spawn_dual_domain_repo(store, snapshot.path(), 1);
    let url = format!("http://{}", server.addr);

    let caps = wait_for_caps(&url);
    assert_eq!(
        caps.hash_domain,
        HashDomain::Blake3,
        "the native domain is unchanged and still what old clients see"
    );
    assert_eq!(
        caps.hash_domains,
        vec![HashDomain::Blake3, HashDomain::Sha256],
        "the bridge ADDED a sha256 domain"
    );

    // ---------- Client ----------
    let db = Db::open_in_memory().expect("client Db");
    db.insert_file(
        &FileRecord::new(
            blake3_hash,
            "/lib/snapshot-mode.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_hex.clone()),
        1,
    )
    .expect("insert file");
    db.add_shared_service("snapshot-mode", &url, None)
        .expect("add_shared_service");
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "snapshot-mode", 256, None)
        .expect("pull_repo must succeed against a snapshot-mode repo");
    assert!(
        stats.matched_files >= 1,
        "the file must match a snapshot-backed bucket: {stats:?}"
    );

    let guard = db.lock().unwrap();
    let fid = guard
        .file_id_by_hash(&blake3_hash)
        .expect("file_id_by_hash")
        .expect("file must be in the library");
    let tags: Vec<String> = guard
        .tags_of(fid)
        .expect("tags_of")
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    assert!(
        tags.contains(&"character:samus".to_string())
            && tags.contains(&"series:metroid".to_string()),
        "PTR tags must land on the blake3 identity with no seed: {tags:?}"
    );
}

/// Serve a sha256-domain repo with a REAL read-only split store opened from a
/// SEPARATE db file (`read_db_path`), so a test can prove reads route through
/// `read_store` rather than the write `store` — the production read-split path
/// uncovered after `serve_end_to_end.rs` was removed in commit 90be861 (#128).
fn spawn_sha256_repo_with_read_store(
    store: RepoStore,
    read_db_path: &std::path::Path,
    k: u64,
) -> ServedRepo {
    let read_store = Some(Arc::new(Mutex::new(
        RepoStore::open_readonly(read_db_path).expect("open read-only split store"),
    )));
    let store = Arc::new(Mutex::new(store));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            axum::serve(
                listener,
                naiad_server::app_split(store, read_store, k, None, None, HashDomain::Sha256)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve repo");
        });
    });
    ServedRepo {
        addr: rx.recv().expect("repo failed to bind"),
        _handle: handle,
    }
}

/// Regression: proves the sha256-domain serve path routes READS through the
/// `Some(read_store)` split connection, not the write store — the production
/// read-split path that `serve_end_to_end.rs` (deleted in #128) covered.
///
/// The read store is a SEPARATE db holding the real tag; the write store holds
/// a decoy tag on the SAME sha256. A pull that returns the read-store tag and
/// NOT the decoy can only have gone through `read_store` — so, unlike a test
/// where both stores serve identical data, this one fails if reads fall back to
/// the write store.
#[test]
fn sha256_domain_reads_route_through_split_read_store() {
    let content = b"bridge-read-store-e2e-file";
    let (blake3_hash, sha256_hex) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual content");

    let dir = tempfile::tempdir().expect("tempdir");
    let write_db = dir.path().join("split-write.db");
    let read_db = dir.path().join("split-read.db");

    // Write store: a DECOY tag on the same sha256 — reads must NOT surface it.
    let write_store = RepoStore::open(&write_db).expect("open write store");
    write_store
        .apply_mappings_bulk(vec![(
            sha256_hex.clone(),
            "writer-only:decoy".to_string(),
            false,
        )])
        .expect("seed decoy into write store");

    // Read store source: the REAL tag. Drop the writer handle so the read-only
    // open below sees a checkpointed db (matches the state.rs open_readonly test).
    {
        let read_seed = RepoStore::open(&read_db).expect("open read store source");
        read_seed
            .apply_mappings_bulk(vec![(
                sha256_hex.clone(),
                "character:samus".to_string(),
                false,
            )])
            .expect("seed real tag into read store");
    }

    // Serve: store = writer (decoy), read_store = open_readonly(read_db) (real).
    let server = spawn_sha256_repo_with_read_store(write_store, &read_db, 1);
    let url = format!("http://{}", server.addr);

    // Verify caps advertise SHA-256 domain.
    let probe = RepoClient::new(&url);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let caps = loop {
        match probe.fetch_caps() {
            Ok(c) => break c,
            Err(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "repo did not start within 5 s"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    assert_eq!(
        caps.hash_domain,
        HashDomain::Sha256,
        "read-split serve path must advertise HashDomain::Sha256"
    );

    // Pull via naiad daemon client: the sha256-keyed tag must land on the blake3 identity.
    let db = Db::open_in_memory().expect("client Db");
    db.insert_file(
        &FileRecord::new(
            blake3_hash,
            "/lib/split-store.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_hex.clone()),
        1,
    )
    .expect("insert file");
    let (null_sha_blake3, _) =
        hash_reader_dual(&b"no-sha256-split"[..]).expect("hash_reader_dual null-sha");
    db.insert_file(
        &FileRecord::new(null_sha_blake3, "/lib/null-split.png".into(), 16, Some(1)),
        2,
    )
    .expect("insert null-sha file");

    db.add_shared_service("bridge-split-e2e", &url, None)
        .expect("add_shared_service");
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "bridge-split-e2e", 256, None)
        .expect("pull_repo must succeed with a real read-store");
    assert!(
        stats.matched_files >= 1,
        "at least one file must match via read-store"
    );

    let db_lock = db.lock().unwrap();
    let fid = db_lock
        .file_id_by_hash(&blake3_hash)
        .expect("file_id_by_hash")
        .expect("file must be in library");
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .expect("tags_of")
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags.contains(&"character:samus".to_string()),
        "read-store tag must land — reads must route through the split read store; got {tags:?}"
    );
    assert!(
        !tags.contains(&"writer-only:decoy".to_string()),
        "write-store decoy must NOT surface — reads must not fall back to the write store; got {tags:?}"
    );
}

/// Back-compat (spec §7): a pre-dual-domain client reads only `hash_domain`
/// and sends no `domain=`. Against a dual-domain repo it must see and pull
/// BLAKE3 only — no SHA-256-keyed rows, and no errors.
#[test]
fn old_client_sees_blake3_only_from_a_dual_domain_repo() {
    let content = b"back-compat-file";
    let (blake3_hash, sha256_hex) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual content");

    let snapshot = tempfile::tempdir().expect("snapshot tempdir");
    naiad_plugin_hydrus::fixture::write_snapshot(
        snapshot.path(),
        9,
        &[(&sha256_hex, "sha:only-in-the-snapshot")],
    )
    .expect("write snapshot fixture");

    // The native store carries one blake3-keyed tag for the same file.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = RepoStore::open(dir.path().join("dual-repo.db")).expect("open store");
    store
        .apply_mappings_bulk(vec![(
            blake3_hash.to_hex(),
            "native:blake3-tag".to_string(),
            false,
        )])
        .expect("seed the NATIVE domain (blake3-keyed, as any repo does)");

    let server = spawn_dual_domain_repo(store, snapshot.path(), 1);
    let url = format!("http://{}", server.addr);

    // An "old client": reads only `hash_domain`, calls the domain-less fetches.
    let caps = wait_for_caps(&url);
    assert_eq!(
        caps.hash_domain,
        HashDomain::Blake3,
        "an old client reads this field and must see a plain blake3 repo"
    );

    let client = RepoClient::new(&url);
    let snapshot_body = client
        .fetch_buckets(256, &[blake3_hash.to_hex()])
        .expect("a domain-less bucket fetch must not error");

    assert!(
        snapshot_body.tags.contains_key(&blake3_hash.to_hex()),
        "the native blake3 tags are served as always: {:?}",
        snapshot_body.tags
    );
    assert!(
        !snapshot_body.tags.contains_key(&sha256_hex),
        "no sha256-keyed rows may leak into a domain-less response: {:?}",
        snapshot_body.tags
    );
    let all_tags: Vec<String> = snapshot_body
        .tags
        .values()
        .flatten()
        .map(|ot| ot.tag.clone())
        .collect();
    assert!(
        !all_tags.iter().any(|t| t.starts_with("sha:")),
        "snapshot-only tags must not appear for an old client: {all_tags:?}"
    );
}

// ── Sidecar-mode E2E (#209) ───────────────────────────────────────────────────

/// sha256 that `write_ptr_seed_fixture` assigns to hash slot h1 (service_hash_id 500).
/// The fixture stores: h1 → {maid(800), character:samus(801)}.
const SIDECAR_H1: &str = "1100000000000000000000000000000000000000000000000000000000000000";

/// zlib-compress a JSON value, exactly as Hydrus does for update files.
fn zlib_json_sidecar(v: &serde_json::Value) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
    enc.write_all(serde_json::to_string(v).unwrap().as_bytes())
        .unwrap();
    enc.finish().unwrap()
}

/// Sidecar mode E2E: full seed → sync-one-index → client pull lands tags on
/// blake3 identities (#209 plan §Task 8 deferral, companion to the snapshot
/// variant above).
///
/// Steps:
/// 1. Seed the sidecar from `write_ptr_seed_fixture` (service 9). After seed
///    h1 carries {maid, character:samus} and the cursor sits at 1.
/// 2. Apply PTR sync index 1 — adds "series:metroid" to h1 — using a Fake
///    `UpdateSource` that serves a pre-built definitions + content update.
/// 3. Boot a dual-domain server: native BLAKE3 (empty) + sidecar SHA-256.
/// 4. Insert a client file with blake3 = hash_bytes(...) and sha256 = SIDECAR_H1.
/// 5. `pull_repo` → three tags must land on the blake3 identity.
#[test]
fn sidecar_mode_seed_sync_pull_lands_on_blake3() {
    use std::collections::HashMap;

    use naiad_core::hash_bytes;
    use naiad_netproto::HashDomain;
    use naiad_server::DomainConfig;
    use naiad_server::bridge::ptr_client::{Metadata, MetadataEntry};
    use naiad_server::bridge::sidecar::Sidecar;
    use naiad_server::bridge::sidecar_seed;
    use naiad_server::bridge::sidecar_sync;
    use naiad_server::bridge::sync::UpdateSource;

    // ── Step 1: seed ──────────────────────────────────────────────────────────
    let fixture_dir = tempfile::tempdir().expect("fixture tempdir");
    naiad_plugin_hydrus::fixture::write_ptr_seed_fixture(fixture_dir.path(), 9)
        .expect("write_ptr_seed_fixture");
    let sc_path = fixture_dir.path().join("sidecar.db");
    {
        let sc = Sidecar::create(&sc_path).expect("Sidecar::create");
        sidecar_seed::seed(fixture_dir.path(), Some(9), &sc, false).expect("seed");
        assert_eq!(
            sc.next_update_index().unwrap(),
            1,
            "seed must advance cursor to 1"
        );
    }

    // ── Step 2: sync index 1 ──────────────────────────────────────────────────
    // Definitions: service_hash_id 500 → SIDECAR_H1, service_tag_id 803 → "series:metroid".
    // Content:     action=ADD, tag 803, hash 500.
    {
        let sc = Sidecar::open(&sc_path).expect("Sidecar::open for sync");

        let def_bytes = zlib_json_sidecar(&serde_json::json!([
            36,
            1,
            [[0, [[500, SIDECAR_H1]]], [1, [[803, "series:metroid"]]]]
        ]));
        let content_bytes =
            zlib_json_sidecar(&serde_json::json!([34, 1, [[0, [[0, [[803, [500]]]]]]]]));

        let def_h = "aa".repeat(32);
        let content_h = "bb".repeat(32);
        let meta = Metadata {
            entries: vec![MetadataEntry {
                update_index: 1,
                update_hashes: vec![def_h.clone(), content_h.clone()],
                begin_ts: 0,
                end_ts: 0,
            }],
            next_update_due: 2,
        };
        let files = HashMap::from([(def_h, def_bytes), (content_h, content_bytes)]);

        struct FakeSrc {
            meta: Metadata,
            files: HashMap<String, Vec<u8>>,
        }
        impl UpdateSource for FakeSrc {
            fn metadata(&mut self, _since: u64) -> anyhow::Result<Metadata> {
                Ok(self.meta.clone())
            }
            fn fetch_update(&mut self, h: &str) -> anyhow::Result<Vec<u8>> {
                self.files
                    .get(h)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("no update {h}"))
            }
        }

        let mut src = FakeSrc { meta, files };
        let report = sidecar_sync::sync_once(&sc, &mut src, None).expect("sidecar sync_once");
        assert_eq!(report.indexes_applied, 1, "must apply exactly index 1");
        assert_eq!(
            sc.next_update_index().unwrap(),
            2,
            "cursor must advance to 2 after sync"
        );
    }

    // ── Step 3: spawn dual-domain server ──────────────────────────────────────
    // Build DomainConfig through from_settings so the sidecar branch
    // (resolve_beside_db → SidecarBackend::open → min/max clamping) is covered
    // by this test — not just by the oneshot unit tests in sidecar_serve_e2e.rs.
    let bridge_cfg = naiad_server::settings::BridgeConfig {
        enabled: true,
        mode: naiad_server::settings::BridgeMode::Sidecar,
        snapshot_dir: None,
        snapshot_service_id: None,
        max_query_bits: 256,
        min_query_bits: 8,
        ptr_url: String::new(),
        ptr_key: String::new(),
        // sc_path is absolute, so db_path below is irrelevant.
        state_db: sc_path.to_str().unwrap().to_string(),
    };
    let domains = DomainConfig::from_settings(
        HashDomain::Blake3,
        &bridge_cfg,
        std::path::Path::new("dummy.db"),
        1,
    )
    .expect("from_settings must succeed for a valid seeded sidecar");
    let repo_path = fixture_dir.path().join("repo.db");
    let store = Arc::new(Mutex::new(
        naiad_server::RepoStore::open(&repo_path).expect("open blake3 store"),
    ));
    let server = {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build repo runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let addr = listener.local_addr().expect("local_addr");
                tx.send(addr).expect("send addr");
                axum::serve(
                    listener,
                    naiad_server::app_domains(store, None, 1, None, None, domains)
                        .into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .expect("serve");
            });
        });
        ServedRepo {
            addr: rx.recv().expect("addr from server thread"),
            _handle: handle,
        }
    };

    let url = format!("http://{}", server.addr);
    let caps = wait_for_caps(&url);
    assert!(
        caps.serves(HashDomain::Sha256),
        "sidecar-backed server must serve sha256: {caps:?}"
    );
    // sha256 must NOT be incremental (no sequence numbers in sidecar).
    let incr = caps.incremental_domains.as_deref().unwrap_or(&[]);
    assert!(
        !incr.iter().any(|s| s == "sha256"),
        "sha256 must be excluded from incremental_domains: {incr:?}"
    );

    // ── Step 4: insert file in client DB ──────────────────────────────────────
    let blake3_hash = hash_bytes(b"sidecar-e2e-file");
    let db = naiad_db::Db::open_in_memory().expect("client Db");
    db.insert_file(
        &naiad_core::FileRecord::new(blake3_hash, "/lib/sidecar-e2e.png".into(), 10, Some(1))
            .with_sha256(SIDECAR_H1.to_string()),
        1,
    )
    .expect("insert file with sha256=SIDECAR_H1");
    db.add_shared_service("sidecar-e2e", &url, None)
        .expect("add_shared_service");
    let db = Mutex::new(db);

    // ── Step 5: pull and assert ───────────────────────────────────────────────
    let stats = pull_repo(&db, &CapsCache::new(), "sidecar-e2e", 256, None)
        .expect("pull_repo must succeed against sidecar-backed server");
    assert!(
        stats.matched_files >= 1,
        "at least one file must match: {stats:?}"
    );

    let guard = db.lock().unwrap();
    let fid = guard
        .file_id_by_hash(&blake3_hash)
        .expect("file_id_by_hash")
        .expect("file must be in the library");
    let tags: Vec<String> = guard
        .tags_of(fid)
        .expect("tags_of")
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    assert!(
        tags.contains(&"character:samus".to_string()),
        "character:samus must land on blake3 identity via sidecar seed; got {tags:?}"
    );
    assert!(
        tags.contains(&"maid".to_string()),
        "maid must land on blake3 identity via sidecar seed; got {tags:?}"
    );
    assert!(
        tags.contains(&"series:metroid".to_string()),
        "series:metroid must land on blake3 identity via sidecar sync; got {tags:?}"
    );
}
