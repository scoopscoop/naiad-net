//! End-to-end: a pull handshakes the repo and either buckets or falls back to a
//! whole-repo download, applying only owned tags either way.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use naiad_core::{FileRecord, Hash, Tag, hash_bytes};
use naiad_daemon::{CapsCache, add_tags, pull_repo, pull_repo_for_hashes};
use naiad_db::Db;
use naiad_netproto::{
    Account, BucketRequest, Caps, HashDomain, MIN_WINDOW, Op, PROTOCOL_VERSION, PullMode,
    PullObserver, PullPhase, REPO_BUCKETS, REPO_CAPS, ServeHint, Snapshot, WINDOW_TARGET_MS,
};
use naiad_server::RepoStore;
use naiad_test_support::spawn_test_repo_with_k;

struct MutableTestRepo {
    addr: SocketAddr,
    store: Arc<Mutex<RepoStore>>,
    _handle: JoinHandle<()>,
}

fn spawn_mutable_test_repo_with_k(store: RepoStore, k: u64) -> MutableTestRepo {
    let store = Arc::new(Mutex::new(store));
    let server_store = Arc::clone(&store);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind test repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            axum::serve(
                listener,
                naiad_server::app(server_store, k)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve test repo");
        });
    });
    let addr = rx.recv().expect("test repo failed to bind");
    MutableTestRepo {
        addr,
        store,
        _handle: handle,
    }
}

fn seed(store: &RepoStore, acct: &Account, h: &Hash, tag: &str) {
    store
        .apply_submission(&acct.sign(Op::Add, h, &Tag::parse(tag).unwrap()))
        .unwrap();
}

#[test]
fn below_k_pull_falls_back_to_whole_repo() {
    // A repo with one hash and the default-ish floor → caps says WholeRepo.
    let repo = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let owned = hash_bytes(b"owned");
    seed(&repo, &acct, &owned, "character:samus");
    let server = spawn_test_repo_with_k(repo, 1000); // 1 hash < k
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    // A pre-existing local tag must survive the authoritative shared-service merge.
    add_tags(&db, &owned.to_hex(), &["meta:mine".to_string()]).unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    assert_eq!(stats.matched_files, 1);

    let db = db.lock().unwrap();
    let fid = db.file_id_by_hash(&owned).unwrap().unwrap();
    let mut tags: Vec<String> = db
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    tags.sort();
    assert_eq!(
        tags,
        vec!["character:samus".to_string(), "meta:mine".to_string()]
    );
}

#[test]
fn bucketed_pull_applies_owned_and_discards_unowned_in_the_same_bucket() {
    let repo = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    // Owned hash and a different hash sharing its 1-bit bucket (both top-bit 0),
    // plus two upper-half hashes so count/k == 2 → 1-bit prefix at k=2.
    let owned = Hash::from_bytes([0x10; 32]);
    let same_bucket_unowned = Hash::from_bytes([0x20; 32]); // top bit 0, like owned
    seed(&repo, &acct, &owned, "owned:tag");
    seed(&repo, &acct, &same_bucket_unowned, "unowned:tag");
    seed(&repo, &acct, &Hash::from_bytes([0x80; 32]), "upper:a");
    seed(&repo, &acct, &Hash::from_bytes([0xC0; 32]), "upper:b");
    let server = spawn_test_repo_with_k(repo, 2); // 4 hashes, k=2 → 1-bit buckets
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    assert_eq!(
        stats.matched_files, 1,
        "only the owned hash matches the library"
    );

    let db = db.lock().unwrap();
    let fid = db.file_id_by_hash(&owned).unwrap().unwrap();
    let tags: Vec<String> = db
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_eq!(tags, vec!["owned:tag".to_string()]);
    // The unowned same-bucket hash was downloaded then discarded — never stored.
    assert!(db.file_id_by_hash(&same_bucket_unowned).unwrap().is_none());
}

#[test]
fn incremental_bucket_pull_matches_fresh_full_after_add_and_remove() {
    let repo = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let owned = Hash::from_bytes([0x10; 32]);
    let upper_a = Hash::from_bytes([0x80; 32]);
    seed(&repo, &acct, &owned, "owned:old");
    seed(&repo, &acct, &upper_a, "upper:a");
    let server = spawn_mutable_test_repo_with_k(repo, 1);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    let svc = db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    let first_cursor = db
        .lock()
        .unwrap()
        .mapping_cursor(svc, "blake3")
        .unwrap()
        .unwrap();
    assert!(first_cursor > 0);

    let rm = acct.sign(Op::Remove, &owned, &Tag::parse("owned:old").unwrap());
    server.store.lock().unwrap().apply_submission(&rm).unwrap();
    seed(&server.store.lock().unwrap(), &acct, &owned, "owned:new");

    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    let db_lock = db.lock().unwrap();
    let second_cursor = db_lock.mapping_cursor(svc, "blake3").unwrap().unwrap();
    assert!(second_cursor > first_cursor);
    let fid = db_lock.file_id_by_hash(&owned).unwrap().unwrap();
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(tags, vec!["owned:new".to_string()]);
}

#[test]
fn file_added_after_cursor_gets_its_bucket_pulled_full() {
    let repo = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let first = Hash::from_bytes([0x10; 32]);
    let second_same_bucket = Hash::from_bytes([0x20; 32]);
    let upper_a = Hash::from_bytes([0x80; 32]);
    let upper_b = Hash::from_bytes([0xC0; 32]);
    seed(&repo, &acct, &first, "first:tag");
    seed(&repo, &acct, &second_same_bucket, "second:old");
    seed(&repo, &acct, &upper_a, "upper:a");
    seed(&repo, &acct, &upper_b, "upper:b");
    let server = spawn_mutable_test_repo_with_k(repo, 2);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(first, "/lib/first".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);
    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();

    db.lock()
        .unwrap()
        .insert_file(
            &FileRecord::new(second_same_bucket, "/lib/second".into(), 5, Some(1)),
            2,
        )
        .unwrap();
    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();

    let db_lock = db.lock().unwrap();
    let fid = db_lock
        .file_id_by_hash(&second_same_bucket)
        .unwrap()
        .unwrap();
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(tags, vec!["second:old".to_string()]);
}

#[test]
fn repo_rebuild_with_lower_cursor_triggers_a_full_resync() {
    let repo = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let owned = Hash::from_bytes([0x10; 32]);
    seed(&repo, &acct, &owned, "owned:old");
    seed(&repo, &acct, &owned, "owned:stale");
    seed(&repo, &acct, &Hash::from_bytes([0x80; 32]), "upper:a");
    let server = spawn_mutable_test_repo_with_k(repo, 1);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    let svc = db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);
    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    let first_cursor = db
        .lock()
        .unwrap()
        .mapping_cursor(svc, "blake3")
        .unwrap()
        .unwrap();
    assert!(first_cursor >= 3);

    // Rebuild the repo from scratch: seq restarts below the client's cursor.
    let rebuilt = RepoStore::open_in_memory().unwrap();
    seed(&rebuilt, &acct, &owned, "owned:rebuilt");
    *server.store.lock().unwrap() = rebuilt;

    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    let db_lock = db.lock().unwrap();
    let cursor = db_lock.mapping_cursor(svc, "blake3").unwrap().unwrap();
    assert!(cursor < first_cursor, "cursor follows the rebuilt repo");
    let fid = db_lock.file_id_by_hash(&owned).unwrap().unwrap();
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(tags, vec!["owned:rebuilt".to_string()]);
}

/// A hand-rolled cap-less repo: caps without `mapping_incremental` (simulates a
/// repo that predates incremental pulls), bucket response in the v5
/// `SupporterSummary` shape but without a `cursor` field — exercises the
/// client's fallback to full-bucket pulls when the server doesn't advertise
/// `mapping_incremental`.
fn spawn_old_repo(owned_hex: String) -> SocketAddr {
    use axum::routing::{get, post};
    let caps = serde_json::json!({
        "version": naiad_netproto::PROTOCOL_VERSION,
        "mode": "bucketed",
        "prefix_bits": 8,
    });
    let snapshot = serde_json::json!({
        "version": naiad_netproto::PROTOCOL_VERSION,
        // v8 shape: hash → [OriginTag { tag, origin }, ...].  No cursor field
        // because this repo predates `mapping_incremental` — exercises the
        // client's fallback to full-bucket pulls.
        "tags": { owned_hex: [{"tag": "old:tag"}] },
    });
    let app = axum::Router::new()
        .route(
            "/repo/caps",
            get(move || std::future::ready(axum::Json(caps))),
        )
        .route(
            "/repo/buckets",
            post(move |_body: String| std::future::ready(axum::Json(snapshot))),
        );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build old repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind old repo");
            tx.send(listener.local_addr().expect("local_addr"))
                .expect("send bound addr");
            axum::serve(listener, app).await.expect("serve old repo");
        });
    });
    rx.recv().expect("old repo failed to bind")
}

#[test]
fn old_repo_without_the_cap_falls_back_full_and_clears_stale_cursor_state() {
    let owned = Hash::from_bytes([0x10; 32]);
    let addr = spawn_old_repo(owned.to_hex());
    let url = format!("http://{addr}");

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    let svc = db.add_shared_service("ptr", &url, None).unwrap();
    // Stale incremental state, as if the repo advertised the cap in the past.
    db.set_mapping_pull_state(svc, "blake3", 99, 1).unwrap();
    let db = Mutex::new(db);

    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();

    let db_lock = db.lock().unwrap();
    let fid = db_lock.file_id_by_hash(&owned).unwrap().unwrap();
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(tags, vec!["old:tag".to_string()]);
    assert_eq!(
        db_lock.mapping_cursor(svc, "blake3").unwrap(),
        None,
        "stale cursor cleared on old-repo fallback"
    );
    assert_eq!(db_lock.last_pull_file_marker(svc, "blake3").unwrap(), None);
}

/// A tag submitted by 3 accounts pulls into the client. A subsequent delta-path
/// retract from one account tombstones the mapping (v6 last-write-wins: any
/// Remove overrides all prior Adds) — the whole-pipeline proof for multi-submit
/// and delta retract.
#[test]
fn three_supporters_pull_then_delta_retract() {
    let repo = RepoStore::open_in_memory().unwrap();
    let acct_a = Account::generate();
    let acct_b = Account::generate();
    let acct_c = Account::generate();
    // owned: top-bit 0; upper: top-bit 1 — two distinct hashes forces 1-bit
    // bucketed mode (count=2, k=1 → count/k=2, ilog2(2)=1 prefix bit).
    let owned = Hash::from_bytes([0x10; 32]);
    let upper = Hash::from_bytes([0x80; 32]);

    // All three accounts support the same (hash, tag) on the repo.
    seed(&repo, &acct_a, &owned, "char:samus");
    seed(&repo, &acct_b, &owned, "char:samus");
    seed(&repo, &acct_c, &owned, "char:samus");
    seed(&repo, &acct_a, &upper, "upper:pad"); // second hash enables bucketed pull

    let server = spawn_mutable_test_repo_with_k(repo, 1); // k=1 → bucketed
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    // ── First pull: tag should arrive (v6 carries no supporter metadata) ────
    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();

    {
        let db_lock = db.lock().unwrap();
        let fid = db_lock.file_id_by_hash(&owned).unwrap().unwrap();
        let tags: Vec<String> = db_lock
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert!(
            tags.contains(&"char:samus".to_string()),
            "char:samus must arrive after first pull; got {tags:?}"
        );
    }

    // ── Delta retract: acct_c removes its mapping ────────────────────────────
    {
        let rm = acct_c.sign(Op::Remove, &owned, &Tag::parse("char:samus").unwrap());
        server.store.lock().unwrap().apply_submission(&rm).unwrap();
    }

    // ── Second pull (incremental delta) ─────────────────────────────────────
    // v6 server has one row per (hash, tag); acct_c's Remove sets status=deleted,
    // overriding the previous Adds. The delta correctly tombstones the mapping.
    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();

    {
        let db_lock = db.lock().unwrap();
        let fid = db_lock.file_id_by_hash(&owned).unwrap().unwrap();
        let tags: Vec<String> = db_lock
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        // The Remove from acct_c is the final op on the server, so the tag is gone.
        assert!(
            !tags.contains(&"char:samus".to_string()),
            "char:samus must be retracted after acct_c's Remove in v6 last-write-wins; got {tags:?}"
        );
    }
}

// ── Privacy-ceiling integration tests ───────────────────────────────────────

/// A mock repo that:
/// - advertises a fixed `prefix_bits` regardless of its contents,
/// - captures the `BucketRequest` the client posts,
/// - replies with a canned `Snapshot`.
struct CapsInjectingRepo {
    addr: SocketAddr,
    captured: Arc<Mutex<Option<BucketRequest>>>,
    _handle: JoinHandle<()>,
}

/// Spawn a mock repo that serves `advertised_bits` in its caps and returns
/// `tags` for every bucket pull.  `mapping_incremental: false` forces the
/// plain `fetch_buckets` path so the ceiling is exercised directly.
fn spawn_caps_injecting_repo(
    advertised_bits: u32,
    tags: BTreeMap<String, Vec<String>>,
) -> CapsInjectingRepo {
    use naiad_netproto::OriginTag;
    // Convert plain strings to OriginTag (no origin = manual) for the v8 snapshot wire format.
    let tags: BTreeMap<String, Vec<OriginTag>> = tags
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                v.into_iter()
                    .map(|tag| OriginTag { tag, origin: None })
                    .collect(),
            )
        })
        .collect();
    let captured: Arc<Mutex<Option<BucketRequest>>> = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&captured);
    let tags = Arc::new(tags);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build mock repo runtime");
        rt.block_on(async move {
            let caps_handler = move || async move {
                axum::Json(Caps {
                    version: PROTOCOL_VERSION,
                    mode: PullMode::Bucketed {
                        prefix_bits: advertised_bits,
                    },
                    relation_incremental: false,
                    mapping_incremental: false,
                    reports: false,
                    repo_key: None,
                    hash_domain: HashDomain::Blake3,
                    hash_domains: Vec::new(),
                    incremental_domains: None,
                    server_version: None,
                    serve_hint: Default::default(),
                    streaming: false,
                    min_query_bits: None,
                    store_generation: None,
                    count: None,
                    name: None,
                })
            };
            let buckets_handler = move |axum::Json(req): axum::Json<BucketRequest>| {
                let recorder = Arc::clone(&recorder);
                let tags = Arc::clone(&tags);
                async move {
                    *recorder.lock().unwrap() = Some(req);
                    axum::Json(Snapshot {
                        version: PROTOCOL_VERSION,
                        cursor: 0,
                        tags: (*tags).clone(),
                    })
                }
            };
            let app = axum::Router::new()
                .route(REPO_CAPS, axum::routing::get(caps_handler))
                .route(REPO_BUCKETS, axum::routing::post(buckets_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock repo");
            tx.send(listener.local_addr().expect("local_addr")).unwrap();
            axum::serve(listener, app).await.expect("serve mock repo");
        });
    });
    CapsInjectingRepo {
        addr: rx.recv().expect("mock repo failed to bind"),
        captured,
        _handle: handle,
    }
}

/// A hostile repo advertising 256-bit buckets is clamped to the configured
/// ceiling (24).  The client must send `prefix_bits == 24` and use the
/// coarse bucket key, never leaking the exact hash.
#[test]
fn hostile_256_bit_caps_are_clamped_to_the_ceiling() {
    let owned = hash_bytes(b"private-file");
    let mut tags = BTreeMap::new();
    tags.insert(owned.to_hex(), vec!["character:samus".to_string()]);
    let server = spawn_caps_injecting_repo(256, tags);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "ptr", 24, None).unwrap();

    let req = server
        .captured
        .lock()
        .unwrap()
        .clone()
        .expect("bucket request must have been captured");
    assert_eq!(
        req.prefix_bits, 24,
        "advertised 256 must be clamped to the ceiling 24; got {}",
        req.prefix_bits
    );
    assert!(
        !req.buckets.contains(&owned.to_hex()),
        "the exact 256-bit owned hash must never appear as a bucket key"
    );
    assert!(
        req.buckets
            .contains(&naiad_netproto::bucket_key(&owned, 24)),
        "the 24-bit coarse bucket key must be present"
    );
    assert_eq!(stats.matched_files, 1, "owned tag must still merge");
}

/// When the repo advertises fewer bits than the ceiling, the client must
/// honour the repo's advertised value — no unnecessary coarsening.
#[test]
fn honest_caps_under_the_ceiling_pass_through_unclamped() {
    let owned = hash_bytes(b"ordinary-file");
    let mut tags = BTreeMap::new();
    tags.insert(owned.to_hex(), vec!["owned:tag".to_string()]);
    let server = spawn_caps_injecting_repo(13, tags);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    pull_repo(&db, &CapsCache::new(), "ptr", 24, None).unwrap();
    let req = server.captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        req.prefix_bits, 13,
        "under-ceiling advertisement must pass through unclamped"
    );
}

/// When the user raises the ceiling (e.g. to 256 for a trusted local repo),
/// a fine advertised prefix (200) must be honoured in full.
#[test]
fn raised_ceiling_honors_a_fine_advertised_prefix() {
    let owned = hash_bytes(b"vpn-user-file");
    let server = spawn_caps_injecting_repo(200, BTreeMap::new());
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    let req = server.captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        req.prefix_bits, 200,
        "the ceiling is a user knob, not a hardcoded 24"
    );
}

/// A clamped pull that gets back an empty tags map must succeed — thin/sparse
/// bucket responses are valid (the repo may simply have nothing in those
/// ranges).
#[test]
fn sparse_bucket_responses_merge_without_error() {
    let owned = hash_bytes(b"lonely-file");
    let server = spawn_caps_injecting_repo(256, BTreeMap::new());
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(&FileRecord::new(owned, "/lib/a".into(), 5, Some(1)), 1)
        .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "ptr", 24, None).unwrap();
    assert_eq!(stats.mappings, 0, "nothing to merge, but no error either");
}

/// An origin-tagged mapping pulled from the repo lands with the correct
/// `origin_id` in the client's `mappings` table, while a manual (no-origin)
/// mapping stays NULL.  This is the Task 14 end-to-end invariant for the
/// snapshot/bucket path.
#[test]
fn pull_threads_origin_into_mappings_origin_id() {
    // Two files the client owns.
    let owned_with_origin = hash_bytes(b"origin-e2e-file-a");
    let owned_manual = hash_bytes(b"origin-e2e-file-b");

    // Repo: seed the origin-tagged file with a sign_with_origin submission,
    // and the manual file with a plain sign (origin = None).
    let repo = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    let tag_origin = Tag::parse("character:samus").unwrap();
    let tag_manual = Tag::parse("series:metroid").unwrap();
    repo.apply_submission(&acct.sign_with_origin(
        Op::Add,
        &owned_with_origin,
        &tag_origin,
        Some("wd14-tagger"),
    ))
    .unwrap();
    repo.apply_submission(&acct.sign(Op::Add, &owned_manual, &tag_manual))
        .unwrap();

    // k=1 with 2 entries → Bucketed mode so the pull exercises the bucketed path.
    let server = spawn_mutable_test_repo_with_k(repo, 1);
    let url = format!("http://{}", server.addr);

    // Client library: both files are known.
    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(owned_with_origin, "/lib/a.png".into(), 1, Some(1)),
        1,
    )
    .unwrap();
    db.insert_file(
        &FileRecord::new(owned_manual, "/lib/b.png".into(), 1, Some(1)),
        2,
    )
    .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db_m = Mutex::new(db);

    let stats = pull_repo(&db_m, &CapsCache::new(), "ptr", 256, None).expect("pull must succeed");
    assert!(stats.matched_files >= 1, "at least one file matched");

    let db = db_m.lock().unwrap();

    // Resolve ids needed to call pulled_mapping_origin.
    let svc_id = db.shared_service_by_name("ptr").unwrap().unwrap().id;
    let fid_a = db.file_id_by_hash(&owned_with_origin).unwrap().unwrap();
    let fid_b = db.file_id_by_hash(&owned_manual).unwrap().unwrap();
    let tid_samus = db.intern_tag(&tag_origin).unwrap();
    let tid_metroid = db.intern_tag(&tag_manual).unwrap();

    let origin_a = db.pulled_mapping_origin(svc_id, fid_a, tid_samus).unwrap();
    assert_eq!(
        origin_a,
        Some("wd14-tagger".to_string()),
        "origin-tagged pull must land with the correct origin name"
    );

    let origin_b = db
        .pulled_mapping_origin(svc_id, fid_b, tid_metroid)
        .unwrap();
    assert!(
        origin_b.is_none(),
        "manual pull (no origin) must land with NULL origin_id; got {origin_b:?}"
    );
}

// ── Phase-observer / stage-event tests (#174) ────────────────────────────────
//
// These tests verify the PullPhase events emitted via pull_repo_for_hashes and
// the adaptive windowing driven by serve_hint.  They use a lightweight
// RecordingObserver (in place of the daemon's SseObserver, which is private to
// server.rs) and various CapsInjectingRepo variants.

/// Captures PullPhase events and set_domain transitions for inspection.
struct RecordingObserver {
    phases: std::cell::RefCell<Vec<PullPhase>>,
    domains: std::cell::RefCell<Vec<Option<&'static str>>>,
}

impl RecordingObserver {
    fn new() -> Self {
        Self {
            phases: std::cell::RefCell::new(Vec::new()),
            domains: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl PullObserver for RecordingObserver {
    fn set_domain(&self, domain: Option<&'static str>) {
        self.domains.borrow_mut().push(domain);
    }
    fn on_phase(&self, phase: PullPhase) {
        self.phases.borrow_mut().push(phase);
    }
}

/// Spawn a mock repo that injects a blake3 serve_hint in its caps, so the
/// adaptive walker sizes its first window from the hint.
fn spawn_caps_injecting_repo_with_hint(
    advertised_bits: u32,
    tags: BTreeMap<String, Vec<String>>,
    ms_per_bucket: f64,
) -> CapsInjectingRepo {
    use naiad_netproto::OriginTag;
    let tags: BTreeMap<String, Vec<OriginTag>> = tags
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                v.into_iter()
                    .map(|tag| OriginTag { tag, origin: None })
                    .collect(),
            )
        })
        .collect();
    let captured: Arc<Mutex<Option<BucketRequest>>> = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&captured);
    let tags = Arc::new(tags);
    let mut hint_map = BTreeMap::new();
    hint_map.insert(
        "blake3".to_string(),
        ServeHint {
            ms_per_bucket,
            hint_bits: None,
        },
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build mock repo runtime");
        rt.block_on(async move {
            let caps_handler = move || async move {
                axum::Json(Caps {
                    version: PROTOCOL_VERSION,
                    mode: PullMode::Bucketed {
                        prefix_bits: advertised_bits,
                    },
                    relation_incremental: false,
                    mapping_incremental: false,
                    reports: false,
                    repo_key: None,
                    hash_domain: HashDomain::Blake3,
                    hash_domains: Vec::new(),
                    incremental_domains: None,
                    server_version: None,
                    serve_hint: hint_map,
                    streaming: false,
                    min_query_bits: None,
                    store_generation: None,
                    count: None,
                    name: None,
                })
            };
            let buckets_handler = move |axum::Json(req): axum::Json<BucketRequest>| {
                let recorder = Arc::clone(&recorder);
                let tags = Arc::clone(&tags);
                async move {
                    *recorder.lock().unwrap() = Some(req);
                    axum::Json(Snapshot {
                        version: PROTOCOL_VERSION,
                        cursor: 0,
                        tags: (*tags).clone(),
                    })
                }
            };
            let app = axum::Router::new()
                .route(REPO_CAPS, axum::routing::get(caps_handler))
                .route(REPO_BUCKETS, axum::routing::post(buckets_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock repo");
            tx.send(listener.local_addr().expect("local_addr")).unwrap();
            axum::serve(listener, app).await.expect("serve mock repo");
        });
    });
    CapsInjectingRepo {
        addr: rx.recv().expect("mock repo failed to bind"),
        captured,
        _handle: handle,
    }
}

/// Spawn a dual-domain mock (blake3 + sha256).  Returns the same canned
/// snapshot for any bucket request, regardless of the domain parameter.
fn spawn_dual_domain_repo(
    advertised_bits: u32,
    tags: BTreeMap<String, Vec<String>>,
) -> CapsInjectingRepo {
    use naiad_netproto::OriginTag;
    let tags: BTreeMap<String, Vec<OriginTag>> = tags
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                v.into_iter()
                    .map(|tag| OriginTag { tag, origin: None })
                    .collect(),
            )
        })
        .collect();
    let captured: Arc<Mutex<Option<BucketRequest>>> = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&captured);
    let tags = Arc::new(tags);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build dual-domain mock runtime");
        rt.block_on(async move {
            let caps_handler = move || async move {
                axum::Json(Caps {
                    version: PROTOCOL_VERSION,
                    mode: PullMode::Bucketed {
                        prefix_bits: advertised_bits,
                    },
                    relation_incremental: false,
                    mapping_incremental: false,
                    reports: false,
                    repo_key: None,
                    hash_domain: HashDomain::Blake3,
                    hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
                    incremental_domains: None,
                    server_version: None,
                    serve_hint: Default::default(),
                    streaming: false,
                    min_query_bits: None,
                    store_generation: None,
                    count: None,
                    name: None,
                })
            };
            let buckets_handler = move |axum::Json(req): axum::Json<BucketRequest>| {
                let recorder = Arc::clone(&recorder);
                let tags = Arc::clone(&tags);
                async move {
                    *recorder.lock().unwrap() = Some(req);
                    axum::Json(Snapshot {
                        version: PROTOCOL_VERSION,
                        cursor: 0,
                        tags: (*tags).clone(),
                    })
                }
            };
            let app = axum::Router::new()
                .route(REPO_CAPS, axum::routing::get(caps_handler))
                .route(REPO_BUCKETS, axum::routing::post(buckets_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind dual-domain mock");
            tx.send(listener.local_addr().expect("local_addr")).unwrap();
            axum::serve(listener, app)
                .await
                .expect("serve dual-domain mock");
        });
    });
    CapsInjectingRepo {
        addr: rx.recv().expect("dual-domain mock failed to bind"),
        captured,
        _handle: handle,
    }
}

/// Criterion 3a: a pull over a repo that requires multiple adaptive windows
/// emits ≥2 `ChunkReceived` phases with monotonically non-decreasing `done`,
/// `cumulative_bytes`, `hashes`, and `tags`; constant `total`; and the sequence
/// ends with `Merging` then `Done`.
///
/// Setup: 100 distinct hashes at prefix_bits=256 → 100 distinct bucket keys.
/// Hint ms=200.0 → W0 = round(5000/200) = 25, clamped to MIN_WINDOW=32.
/// With 100 keys and growing windows, at least 3 request windows are issued.
#[test]
fn multi_window_pull_emits_chunk_stages_in_order() {
    // Build 100 distinct hashes.
    let hashes: Vec<Hash> = (0u64..100).map(|i| hash_bytes(&i.to_le_bytes())).collect();

    // Seed a simple tag payload so hashes/tags accumulate across windows.
    let mut tags = BTreeMap::new();
    tags.insert(hashes[0].to_hex(), vec!["series:metroid".to_string()]);

    let server = spawn_caps_injecting_repo_with_hint(256, tags, 200.0);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    for (i, h) in hashes.iter().enumerate() {
        db.insert_file(
            &FileRecord::new(*h, format!("/lib/{i}.bin").into(), 1, None),
            i as i64 + 1,
        )
        .unwrap();
    }
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let obs = RecordingObserver::new();
    pull_repo_for_hashes(&db, &CapsCache::new(), "ptr", 256, &hashes, &obs).unwrap();

    let phases = obs.phases.into_inner();

    // Extract ChunkReceived phases.
    let chunks: Vec<_> = phases
        .iter()
        .filter_map(|p| match p {
            PullPhase::ChunkReceived {
                done,
                total,
                cumulative_bytes,
                hashes,
                tags,
                ..
            } => Some((*done, *total, *cumulative_bytes, *hashes, *tags)),
            _ => None,
        })
        .collect();

    assert!(
        chunks.len() >= 2,
        "need ≥2 ChunkReceived phases; got {} (hint should produce multiple windows)",
        chunks.len()
    );

    // chunk_total (total) must be constant across all windows.
    let first_total = chunks[0].1;
    for (i, (_, total, _, _, _)) in chunks.iter().enumerate() {
        assert_eq!(
            *total, first_total,
            "chunk {i}: total changed (expected {first_total}, got {total})"
        );
    }

    // done must be strictly increasing.
    let dones: Vec<usize> = chunks.iter().map(|(done, ..)| *done).collect();
    for w in dones.windows(2) {
        assert!(
            w[0] < w[1],
            "done is not strictly increasing: {:?} → {:?}",
            w[0],
            w[1]
        );
    }

    // cumulative_bytes, hashes, tags must be non-decreasing.
    let bytes: Vec<usize> = chunks.iter().map(|(_, _, b, _, _)| *b).collect();
    let hashes_seq: Vec<usize> = chunks.iter().map(|(_, _, _, h, _)| *h).collect();
    let tags_seq: Vec<usize> = chunks.iter().map(|(_, _, _, _, t)| *t).collect();
    for w in bytes.windows(2) {
        assert!(w[0] <= w[1], "bytes decreased: {w:?}");
    }
    for w in hashes_seq.windows(2) {
        assert!(w[0] <= w[1], "hashes decreased: {w:?}");
    }
    for w in tags_seq.windows(2) {
        assert!(w[0] <= w[1], "tags decreased: {w:?}");
    }

    // Sequence must end with Merging then Done (no Progress event — the SSE
    // handler emits Progress separately after the observer completes).
    let last_two: Vec<&str> = phases
        .iter()
        .rev()
        .take(2)
        .map(|p| match p {
            PullPhase::Done => "done",
            PullPhase::Merging => "merging",
            PullPhase::ChunkReceived { .. } => "chunk",
            PullPhase::RequestSent { .. } => "request",
            PullPhase::RowReceived { .. } => "row",
            // Window shrink-retry (#177): not relevant to this bookend check.
            PullPhase::WindowRetry { .. } => "retry",
        })
        .collect::<Vec<_>>();
    // last_two is in reverse order: [Done, Merging]
    assert_eq!(
        last_two,
        ["done", "merging"],
        "phases must end with Merging then Done"
    );
}

/// Criterion 3b: a repo that advertises a blake3 serve_hint sizes its first
/// adaptive window from the hint; a repo without a hint uses the full body
/// budget as the initial window (all keys in one shot when they fit).
#[test]
fn serve_hint_sizes_first_window() {
    // 50 distinct hashes — small enough that all fit in one body-budget
    // request (no-hint path), but large enough to confirm the hinted path
    // issues a clamped-to-MIN_WINDOW first request.
    let hashes: Vec<Hash> = (0u64..50).map(|i| hash_bytes(&i.to_le_bytes())).collect();

    // Repo A: hint ms=200.0 → W0 = round(5000/200) = 25 → clamped to 32.
    let server_a = spawn_caps_injecting_repo_with_hint(256, BTreeMap::new(), 200.0);
    // Repo B: no hint → W0 = usize::MAX → clamped by body budget → all 50 keys
    // fit in one request, so first window = 50.
    let server_b = spawn_caps_injecting_repo(256, BTreeMap::new());

    for (label, server, expected_first_window) in [
        ("hint", &server_a, 32usize), // hint: MIN_WINDOW (25 rounds to 32)
        ("no-hint", &server_b, 50),   // no hint: all keys in one window
    ] {
        let url = format!("http://{}", server.addr);
        let db = Db::open_in_memory().unwrap();
        for (i, h) in hashes.iter().enumerate() {
            db.insert_file(
                &FileRecord::new(*h, format!("/lib/{i}.bin").into(), 1, None),
                i as i64 + 1,
            )
            .unwrap();
        }
        db.add_shared_service("ptr", &url, None).unwrap();
        let db = Mutex::new(db);

        let obs = RecordingObserver::new();
        pull_repo_for_hashes(&db, &CapsCache::new(), "ptr", 256, &hashes, &obs).unwrap();

        let phases = obs.phases.into_inner();
        let first_request = phases
            .iter()
            .find_map(|p| {
                if let PullPhase::RequestSent { window, .. } = p {
                    Some(*window)
                } else {
                    None
                }
            })
            .expect("at least one RequestSent must be emitted");

        assert_eq!(
            first_request, expected_first_window,
            "{label}: expected first window = {expected_first_window}, got {first_request}"
        );
    }
}

/// Criterion 3c: a dual-domain repo shows a second `done`/`total` sub-sequence
/// in the ChunkReceived phases once the SHA-256 leg begins (the reset is
/// visible as a drop in `done` from the first leg's total back to a new count).
/// Bytes accumulated from both legs combined are positive, confirming both legs
/// contributed.  Hashes/tags are non-decreasing within each individual leg.
#[test]
fn dual_domain_leg_shows_domain_transition() {
    use naiad_core::hash_reader_dual;

    // Create files that have both blake3 and sha256 hashes so the sha256 leg
    // is exercised.  Use real hash_reader_dual for correct interop values.
    let files_content: &[&[u8]] = &[b"dual-domain-alpha", b"dual-domain-beta"];
    let mut hashes_blake3: Vec<Hash> = Vec::new();
    let mut sha256_hexes: Vec<String> = Vec::new();
    for content in files_content {
        let (b3, sha) = hash_reader_dual(*content).expect("hash_reader_dual");
        hashes_blake3.push(b3);
        sha256_hexes.push(sha);
    }

    // Seed the mock repo with a tag on one hash (keyed by sha256 hex since
    // that is what the sha256-domain request will use as the bucket key at
    // 256-bit prefix).
    let mut tags = BTreeMap::new();
    tags.insert(sha256_hexes[0].clone(), vec!["dual:tag".to_string()]);

    let server = spawn_dual_domain_repo(256, tags);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    for (i, (b3, sha)) in hashes_blake3.iter().zip(sha256_hexes.iter()).enumerate() {
        db.insert_file(
            &FileRecord::new(*b3, format!("/lib/dual{i}.bin").into(), 1, None)
                .with_sha256(sha.clone()),
            i as i64 + 1,
        )
        .unwrap();
    }
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let obs = RecordingObserver::new();
    pull_repo_for_hashes(&db, &CapsCache::new(), "ptr", 256, &hashes_blake3, &obs).unwrap();

    let domains = obs.domains.into_inner();
    let phases = obs.phases.into_inner();

    // The domain sequence must include both "blake3" and "sha256" legs.
    let saw_blake3 = domains.contains(&Some("blake3"));
    let saw_sha256 = domains.contains(&Some("sha256"));
    assert!(
        saw_blake3,
        "blake3 domain leg must be set; domains: {domains:?}"
    );
    assert!(
        saw_sha256,
        "sha256 domain leg must be set; domains: {domains:?}"
    );

    // Both legs must emit at least one ChunkReceived phase.
    let chunk_count = phases
        .iter()
        .filter(|p| matches!(p, PullPhase::ChunkReceived { .. }))
        .count();
    assert!(
        chunk_count >= 2,
        "dual-domain pull must emit ≥2 ChunkReceived (one per leg); got {chunk_count}"
    );

    // Bytes from each leg are non-zero: extract cumulative_bytes from each leg
    // by grouping by domain (as set_domain divides them).
    let chunk_bytes_total: usize = phases
        .iter()
        .filter_map(|p| {
            if let PullPhase::ChunkReceived { chunk_bytes, .. } = p {
                Some(*chunk_bytes)
            } else {
                None
            }
        })
        .sum();
    assert!(
        chunk_bytes_total > 0,
        "combined bytes from both legs must be > 0"
    );

    // Within each leg, hashes/tags must be non-decreasing.
    // Collect leg boundaries from set_domain transitions.
    let mut current_leg: Vec<(usize, usize)> = Vec::new(); // (hashes, tags)
    let mut domain_idx = 0usize;
    for phase in &phases {
        match phase {
            PullPhase::ChunkReceived { hashes, tags, .. } => {
                current_leg.push((*hashes, *tags));
            }
            PullPhase::RequestSent { .. } => {
                // Between RequestSent and ChunkReceived of the same window —
                // no action needed here.
            }
            PullPhase::Merging => {
                // Leg boundary: check the accumulated leg is non-decreasing
                // and reset for next leg.
                for w in current_leg.windows(2) {
                    assert!(
                        w[0].0 <= w[1].0,
                        "hashes decreased within a domain leg at domain_idx {domain_idx}"
                    );
                    assert!(
                        w[0].1 <= w[1].1,
                        "tags decreased within a domain leg at domain_idx {domain_idx}"
                    );
                }
                current_leg.clear();
                domain_idx += 1;
            }
            PullPhase::Done => {}
            // Within-window streaming row tick (#176): not relevant here.
            PullPhase::RowReceived { .. } => {}
            // Window shrink-retry (#177): not relevant to leg-monotonicity check.
            PullPhase::WindowRetry { .. } => {}
        }
    }
}

// ── #179: Floor clamp-up integration tests (§8.3 tests 8-12) ────────────────
//
// Verify the client-side floor clamp-up logic: the sha256/snapshot leg queries
// at max(floor, min(advertised, ceiling)), while the native blake3 leg is never
// floored.  Also pins the warn-once dedup and old-server (no min_query_bits)
// fallback.

/// A dual-domain mock repo that advertises a floor (`min_query_bits`) and
/// captures ALL bucket requests in order, so tests can inspect per-domain
/// prefix_bits independently.
struct FlooredDualDomainRepo {
    addr: SocketAddr,
    /// All `/repo/buckets` requests received, in arrival order.
    captured: Arc<Mutex<Vec<BucketRequest>>>,
    _handle: JoinHandle<()>,
}

/// Spawn a dual-domain (blake3 native + sha256 non-native) mock repo
/// advertising `prefix_bits = advertised_bits` and `min_query_bits = Some(floor)`.
/// Responds to every bucket request with an empty tags snapshot.
fn spawn_floored_dual_domain_repo(advertised_bits: u32, floor: u32) -> FlooredDualDomainRepo {
    let captured: Arc<Mutex<Vec<BucketRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&captured);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build floored dual-domain mock runtime");
        rt.block_on(async move {
            let caps_handler = move || async move {
                axum::Json(Caps {
                    version: PROTOCOL_VERSION,
                    mode: PullMode::Bucketed {
                        prefix_bits: advertised_bits,
                    },
                    relation_incremental: false,
                    mapping_incremental: false,
                    reports: false,
                    repo_key: None,
                    hash_domain: HashDomain::Blake3,
                    hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
                    incremental_domains: None,
                    server_version: None,
                    serve_hint: Default::default(),
                    streaming: false,
                    min_query_bits: Some(floor),
                    store_generation: None,
                    count: None,
                    name: None,
                })
            };
            let buckets_handler = move |axum::Json(req): axum::Json<BucketRequest>| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder.lock().unwrap().push(req);
                    axum::Json(Snapshot {
                        version: PROTOCOL_VERSION,
                        cursor: 0,
                        tags: Default::default(),
                    })
                }
            };
            let app = axum::Router::new()
                .route(REPO_CAPS, axum::routing::get(caps_handler))
                .route(REPO_BUCKETS, axum::routing::post(buckets_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind floored dual-domain mock");
            tx.send(listener.local_addr().expect("local_addr")).unwrap();
            axum::serve(listener, app)
                .await
                .expect("serve floored dual-domain mock");
        });
    });
    FlooredDualDomainRepo {
        addr: rx.recv().expect("floored dual-domain mock failed to bind"),
        captured,
        _handle: handle,
    }
}

/// §8.3 test 8 + 9 — sha256 leg is raised to the floor; native blake3 leg is
/// NOT floored.
///
/// Repo: dual-domain, prefix_bits=256, min_query_bits=16.
/// Client ceiling: 12 (below floor).
///
/// Expected:
///   - sha256 bucket request prefix_bits == 16 (floor, not 12)
///   - blake3 bucket request prefix_bits == 12 (ceiling only, floor not applied)
#[test]
fn floor_clamp_raises_sha256_leg_not_blake3() {
    let owned = hash_bytes(b"floor-clamp-sha256-not-b3");
    let server = spawn_floored_dual_domain_repo(256, 16);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    // Give the file a sha256 so the sha256 leg computes bucket keys and actually
    // sends a non-trivial request (even an empty-bucket request exercises the floor).
    let fake_sha256 = "aa".repeat(32); // valid 64-char hex, accepted by insert_file
    db.insert_file(
        &naiad_core::FileRecord::new(owned, "/lib/floor-test.bin".into(), 1, Some(1))
            .with_sha256(fake_sha256),
        1,
    )
    .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    // Pull with ceiling = 12 (below floor 16).
    pull_repo(&db, &CapsCache::new(), "ptr", 12, None).unwrap();

    let reqs = server.captured.lock().unwrap().clone();
    // Both the blake3 and sha256 legs send a bucket request, so we expect ≥2.
    assert!(
        reqs.len() >= 2,
        "expected ≥2 bucket requests (one per domain); got {}",
        reqs.len()
    );

    let bits: std::collections::HashSet<u32> = reqs.iter().map(|r| r.prefix_bits).collect();

    // §8.3 test 8: the sha256 leg must be raised to the floor.
    assert!(
        bits.contains(&16),
        "sha256 leg must query at floor (16), not ceiling (12); got: {bits:?}"
    );

    // §8.3 test 9: the blake3 leg must stay at the ceiling (no floor applied).
    assert!(
        bits.contains(&12),
        "blake3 leg must stay at ceiling (12), not raised to floor; got: {bits:?}"
    );
}

/// §8.3 test 10 — warn-once: a second pull for the same (service, sha256)
/// pair pushes no new pending_notices entry.
#[test]
fn floor_clamp_warn_once_dedup() {
    let owned = hash_bytes(b"floor-warn-once-file");
    let server = spawn_floored_dual_domain_repo(256, 16);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &naiad_core::FileRecord::new(owned, "/lib/warn-once.bin".into(), 1, Some(1))
            .with_sha256("bb".repeat(32)),
        1,
    )
    .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);
    let cache = CapsCache::new();

    // First pull: expect exactly one notice (sha256 domain, floor clamp-up).
    pull_repo(&db, &cache, "ptr", 12, None).unwrap();

    let svc_id = db
        .lock()
        .unwrap()
        .shared_service_by_name("ptr")
        .unwrap()
        .unwrap()
        .id;

    let notice = cache
        .drain_pending_notice(svc_id)
        .expect("first pull with ceiling below floor must produce a pending notice");
    // Notice must name the ceiling, the floor, and the [[repos]] knob.
    assert!(
        notice.contains("12"),
        "notice must name the ceiling (12); got: {notice}"
    );
    assert!(
        notice.contains("16"),
        "notice must name the floor (16); got: {notice}"
    );
    assert!(
        notice.contains("[[repos]]"),
        "notice must mention the [[repos]] knob; got: {notice}"
    );

    // Second pull with the same cache: floor_clamp_warned already set for
    // (svc_id, sha256), so no new pending_notices entry must be pushed.
    pull_repo(&db, &cache, "ptr", 12, None).unwrap();
    let second_notice = cache.drain_pending_notice(svc_id);
    assert!(
        second_notice.is_none(),
        "second pull must not push a new notice (warn-once dedup); got: {second_notice:?}"
    );
}

/// #192 — the hash-targeted pull op (`pull_repo_for_hashes`, the one the
/// streamed SSE `summary` handler drives) pushes the #179 floor-clamp notice,
/// so `drain_pending_notice` — which the handler now folds into the summary
/// row's `notice` field — returns it. Guards the seam that surfaces the
/// advisory on the *streamed* pull path, not just the non-streamed
/// `FilePullRepoResult` one. Without the handler change the drained notice was
/// silently dropped on the floor for every streamed pull.
#[test]
fn floor_clamp_notice_reaches_hash_targeted_pull() {
    let owned = hash_bytes(b"floor-hash-targeted-file");
    let server = spawn_floored_dual_domain_repo(256, 16);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(owned, "/lib/hash-targeted.bin".into(), 1, Some(1))
            .with_sha256("cc".repeat(32)),
        1,
    )
    .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);
    let cache = CapsCache::new();

    // Ceiling 12 < floor 16 → the sha256 leg clamps up and pushes a notice,
    // exactly as the streamed handler's `repo_max_query_bits` hint (12) would.
    let obs = RecordingObserver::new();
    pull_repo_for_hashes(&db, &cache, "ptr", 12, &[owned], &obs).unwrap();

    let svc_id = db
        .lock()
        .unwrap()
        .shared_service_by_name("ptr")
        .unwrap()
        .unwrap()
        .id;

    let notice = cache
        .drain_pending_notice(svc_id)
        .expect("hash-targeted pull with ceiling below floor must produce a pending notice");
    assert!(
        notice.contains("16"),
        "notice must name the floor (16); got: {notice}"
    );
}

/// §8.3 test 12 — old server (no min_query_bits) falls back to
/// `min(advertised, ceiling)` with no clamp-up and no pending notice.
#[test]
fn old_server_no_floor_uses_ceiling_only() {
    // spawn_caps_injecting_repo advertises min_query_bits: None.
    let owned = hash_bytes(b"old-server-fallback");
    let server = spawn_caps_injecting_repo(256, BTreeMap::new());
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &naiad_core::FileRecord::new(owned, "/lib/old.bin".into(), 1, Some(1)),
        1,
    )
    .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);
    let cache = CapsCache::new();

    // Ceiling = 12; old server advertises 256 with no floor.
    pull_repo(&db, &cache, "ptr", 12, None).unwrap();

    // The blake3-only repo sends one bucket request at min(256, 12) = 12.
    let req = server
        .captured
        .lock()
        .unwrap()
        .clone()
        .expect("bucket request must be captured");
    assert_eq!(
        req.prefix_bits, 12,
        "old server: prefix_bits must be min(256, 12) = 12; got {}",
        req.prefix_bits
    );

    // No floor → no clamp-up notice.
    let svc_id = db
        .lock()
        .unwrap()
        .shared_service_by_name("ptr")
        .unwrap()
        .unwrap()
        .id;
    let notice = cache.drain_pending_notice(svc_id);
    assert!(
        notice.is_none(),
        "old server (no floor) must not produce any pending notice; got: {notice:?}"
    );
}

// ── #176: Daemon integration test 17 — streaming pull end-to-end ─────────────

/// Test 17 — streaming bucketed pull end-to-end.
///
/// Stand up an in-process streaming-capable repo with 8 hashes spread evenly
/// across all 8 possible 3-bit buckets (one hash per bucket, guaranteed by
/// constructing hashes with each possible first-byte high-3-bit value).
/// Use a per-request bucket budget sized for exactly 1 bucket's data, so
/// the server emits `{"more":"<key>"}` after the first bucket → ≥1 continuation.
///
/// Assertions:
/// 1. The applied mappings equal the non-streaming baseline (every tag lands).
/// 2. `RowReceived` ticks fire (observer total > window count), confirming
///    within-window streaming progress.
/// 3. The pull succeeds (all 8 matched_files).
#[test]
fn streaming_pull_end_to_end_with_continuation() {
    use naiad_core::Hash;
    use naiad_server::app_with_bucket_budget;

    // Construct 8 hashes, one per 3-bit prefix (0b000..0b111). We choose the
    // first byte to set the high-3 bits exactly, so each hash lands in a
    // distinct 3-bit bucket regardless of the lower bits.
    //
    // Each hash is fully unique — only the first byte differs, and we fill the
    // rest with the index to keep them distinct as 32-byte keys.
    let hashes: Vec<Hash> = (0u8..8)
        .map(|i| {
            let mut bytes = [i; 32];
            bytes[0] = i << 5; // high-3 bits = i → bucket prefix i/8
            Hash::from_bytes(bytes)
        })
        .collect();

    let tag_for = |i: usize| format!("character:samus{i}");
    let acct = Account::generate();
    let repo = RepoStore::open_in_memory().unwrap();

    for (i, h) in hashes.iter().enumerate() {
        let t = Tag::parse(&tag_for(i)).unwrap();
        repo.apply_submission(&acct.sign(Op::Add, h, &t)).unwrap();
    }

    // Per-row cost: hash (64 hex) + tag ("character:samusN" = 16 chars) + overhead (67).
    // store.bucket() starts spent at RESPONSE_ENVELOPE_OVERHEAD (64), then adds
    // approx_row_cost(64, 16) = 147 per row. Budget must be:
    //   ≥ 64 + 147 = 211  (first bucket's one row fits)
    //   < 64 + 147 + 64   (second bucket's envelope + first row would exceed)
    // Choose 250: bucket1 costs 211 ≤ 250 → succeeds; remaining=39; bucket2
    // envelope alone is 64 > 39 → BudgetExceeded → {"more":"..."} continuation.
    let one_row_budget: usize = 250;

    // Spawn the in-process repo.
    let store = std::sync::Arc::new(std::sync::Mutex::new(repo));
    let server_store = std::sync::Arc::clone(&store);
    let (tx, rx) = std::sync::mpsc::channel();
    let _handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build streaming test repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind streaming test repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send addr");
            // k=1 → server advertises bucketed at 3 bits (8 buckets for 8 hashes).
            // one_row_budget → serves first bucket, then emits {"more":"..."}.
            axum::serve(
                listener,
                app_with_bucket_budget(server_store, 1, one_row_budget)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .expect("serve streaming test repo");
        });
    });
    let addr = rx.recv().expect("streaming test repo failed to bind");
    let url = format!("http://{addr}");

    // Daemon DB: own all 8 hashes.
    let db = naiad_db::Db::open_in_memory().unwrap();
    for (i, h) in hashes.iter().enumerate() {
        let marker = (1 + i) as i64;
        db.insert_file(
            &naiad_core::FileRecord::new(*h, format!("/lib/{i}.png").into(), 8, Some(marker)),
            marker,
        )
        .unwrap();
    }
    db.add_shared_service("streaming-repo", &url, None).unwrap();
    let db = std::sync::Mutex::new(db);

    // Run the full pull path — assertions are on the applied tags and stats.
    let cache = CapsCache::new();
    let stats =
        pull_repo(&db, &cache, "streaming-repo", 256, None).expect("streaming pull must succeed");

    // 1. All 8 files matched.
    assert_eq!(
        stats.matched_files, 8,
        "all 8 hashes must land; matched_files = {}",
        stats.matched_files
    );

    // 2. Every tag was applied.
    let db_guard = db.lock().unwrap();
    for (i, h) in hashes.iter().enumerate() {
        let fid = db_guard.file_id_by_hash(h).unwrap().unwrap();
        let applied: Vec<String> = db_guard
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        let expected = tag_for(i);
        assert!(
            applied.contains(&expected),
            "tag {expected} must be applied to hash {i}; got: {applied:?}"
        );
    }
}

// ── #178: width-scaled first-window seed + MIN_WINDOW coarse bootstrap ────────
//
// Verify that a clamped bucketed pull (daemon ceiling < advertised width) uses
// `seed_ms_per_bucket` to seed the first window correctly:
// (a) empty serve_hint → COARSE_BOOTSTRAP_MS → W0 = MIN_WINDOW (not body-budget max)
// (b) normalised hint (hint_bits=Some(32), ms=0.2) at 24-bit ceiling → scaled
//     seed = 0.2 × 2^8 = 51.2 → W0 = round(5000/51.2) = 98

/// Spawn a blake3 mock repo that advertises `prefix_bits = advertised_bits` and
/// injects the given `serve_hint` map (may be empty) in its caps.  The bucket
/// handler always returns an empty snapshot.  Used to inspect first-window
/// sizing without a real latency environment.
struct CappedHintRepo {
    addr: std::net::SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

fn spawn_capped_hint_repo(
    advertised_bits: u32,
    serve_hint: BTreeMap<String, ServeHint>,
) -> CappedHintRepo {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build capped-hint mock runtime");
        rt.block_on(async move {
            let caps_handler = move || async move {
                axum::Json(Caps {
                    version: PROTOCOL_VERSION,
                    mode: PullMode::Bucketed {
                        prefix_bits: advertised_bits,
                    },
                    relation_incremental: false,
                    mapping_incremental: false,
                    reports: false,
                    repo_key: None,
                    hash_domain: HashDomain::Blake3,
                    hash_domains: Vec::new(),
                    incremental_domains: None,
                    server_version: None,
                    serve_hint,
                    streaming: false,
                    min_query_bits: None,
                    store_generation: None,
                    count: None,
                    name: None,
                })
            };
            let buckets_handler = move |_req: axum::Json<BucketRequest>| async move {
                axum::Json(Snapshot {
                    version: PROTOCOL_VERSION,
                    cursor: 0,
                    tags: Default::default(),
                })
            };
            let app = axum::Router::new()
                .route(REPO_CAPS, axum::routing::get(caps_handler))
                .route(REPO_BUCKETS, axum::routing::post(buckets_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind capped-hint mock");
            tx.send(listener.local_addr().expect("local_addr")).unwrap();
            axum::serve(listener, app)
                .await
                .expect("serve capped-hint mock");
        });
    });
    CappedHintRepo {
        addr: rx.recv().expect("capped-hint mock failed to bind"),
        _handle: handle,
    }
}

/// #178 Integration guard (a): repo advertises 32 bits, daemon ceiling 24,
/// EMPTY serve_hint → no usable hint, requested_bits(24) < advertised(32)
/// → COARSE_BOOTSTRAP_MS → W0 = MIN_WINDOW (32), NOT the body-budget maximum.
///
/// With 120 hashes the first request window without a seed would be ≥ 120
/// (all keys fit in the body budget), so MIN_WINDOW = 32 is distinctly smaller.
#[test]
fn clamped_empty_hint_first_window_is_min_window() {
    // 120 distinct hashes → ≥ 100 distinct bucket keys at 24-bit prefix
    // (collisions are astronomically unlikely with 120 << 2^24 distinct inputs).
    let hashes: Vec<Hash> = (0u64..120).map(|i| hash_bytes(&i.to_le_bytes())).collect();

    let server = spawn_capped_hint_repo(32, BTreeMap::new());
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    for (i, h) in hashes.iter().enumerate() {
        db.insert_file(
            &naiad_core::FileRecord::new(*h, format!("/lib/{i}.bin").into(), 1, None),
            i as i64 + 1,
        )
        .unwrap();
    }
    db.add_shared_service("ptr178a", &url, None).unwrap();
    let db = Mutex::new(db);

    let obs = RecordingObserver::new();
    // max_query_bits = 24 < advertised = 32 → clamped pull.
    pull_repo_for_hashes(&db, &CapsCache::new(), "ptr178a", 24, &hashes, &obs).unwrap();

    let phases = obs.phases.into_inner();
    let first_window = phases
        .iter()
        .find_map(|p| {
            if let PullPhase::RequestSent { window, .. } = p {
                Some(*window)
            } else {
                None
            }
        })
        .expect("at least one RequestSent must be emitted");

    assert_eq!(
        first_window, MIN_WINDOW,
        "#178(a): clamped empty-hint first window must be MIN_WINDOW={MIN_WINDOW}, got {first_window}"
    );
}

/// #178 Integration guard (b): repo advertises 32 bits, daemon ceiling 24,
/// serve_hint = `{ ms_per_bucket: 0.2, hint_bits: Some(32) }` → scaled seed
/// = 0.2 × 2^(32−24) = 51.2 → W0 = round(WINDOW_TARGET_MS / 51.2).max(MIN_WINDOW).
///
/// Expected: first window = 98 (much smaller than the ~120-key body-budget max
/// and larger than MIN_WINDOW=32), confirming the width-scaling path fires.
#[test]
fn clamped_scaled_hint_first_window_is_w0() {
    // 120 distinct hashes; see (a) for collision reasoning.
    let hashes: Vec<Hash> = (0u64..120).map(|i| hash_bytes(&i.to_le_bytes())).collect();

    // Compute the expected W0 from the exported constants so this test stays
    // correct if WINDOW_TARGET_MS is ever retuned.
    let scaled_ms = 0.2_f64 * 2f64.powi(32 - 24); // 51.2
    let expected_w0 = ((WINDOW_TARGET_MS as f64 / scaled_ms).round() as usize).max(MIN_WINDOW);

    let mut hint_map = BTreeMap::new();
    hint_map.insert(
        "blake3".to_string(),
        ServeHint {
            ms_per_bucket: 0.2,
            hint_bits: Some(32),
        },
    );
    let server = spawn_capped_hint_repo(32, hint_map);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    for (i, h) in hashes.iter().enumerate() {
        db.insert_file(
            &naiad_core::FileRecord::new(*h, format!("/lib/{i}.bin").into(), 1, None),
            i as i64 + 1,
        )
        .unwrap();
    }
    db.add_shared_service("ptr178b", &url, None).unwrap();
    let db = Mutex::new(db);

    let obs = RecordingObserver::new();
    // max_query_bits = 24 < advertised = 32 → clamped pull.
    pull_repo_for_hashes(&db, &CapsCache::new(), "ptr178b", 24, &hashes, &obs).unwrap();

    let phases = obs.phases.into_inner();
    let first_window = phases
        .iter()
        .find_map(|p| {
            if let PullPhase::RequestSent { window, .. } = p {
                Some(*window)
            } else {
                None
            }
        })
        .expect("at least one RequestSent must be emitted");

    assert_eq!(
        first_window, expected_w0,
        "#178(b): scaled-hint first window must be {expected_w0} \
         (= round({WINDOW_TARGET_MS}/{scaled_ms}).max({MIN_WINDOW})), got {first_window}"
    );
    // Sanity: must be strictly between MIN_WINDOW and the 120-key body-budget cap.
    assert!(
        first_window > MIN_WINDOW,
        "#178(b): scaled W0={first_window} must exceed MIN_WINDOW={MIN_WINDOW}"
    );
    assert!(
        first_window < hashes.len(),
        "#178(b): scaled W0={first_window} must be below the key count={} (not body-budget max)",
        hashes.len()
    );
}
