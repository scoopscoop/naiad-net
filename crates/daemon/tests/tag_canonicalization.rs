//! Issue #77: tag canonicalization end-to-end tests.
//!
//! Verifies that leading-colon tags ("::)", ":a:b") and bare-colon tags (")")
//! survive a sync pull as the correct (namespace, subtag) pairs, and that
//! the protocol version guard is in place.

use std::collections::BTreeMap;
use std::sync::Mutex;

use naiad_core::{FileRecord, Tag, hash_bytes};
use naiad_daemon::{CapsCache, pull_repo};
use naiad_db::Db;
use naiad_netproto::{MIN_SUPPORTED_VERSION, PROTOCOL_VERSION, REPO_BUCKETS, REPO_CAPS};

use std::net::SocketAddr;

/// Spawn a minimal mock repo that serves a canned `Snapshot` as a WholeRepo
/// response. Serves `REPO_CAPS` with `mode = WholeRepo` so `pull_repo` uses
/// the snapshot path rather than the bucketed path.
fn spawn_snapshot_repo(tags: BTreeMap<String, Vec<String>>) -> SocketAddr {
    use axum::routing::{get, post};
    let caps = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "mode": "wholerepo",
        "relation_incremental": false,
        "mapping_incremental": false,
        "reports": false,
    });
    // Convert plain strings to v8 OriginTag objects { "tag": "..." } for the
    // snapshot body — the client deserialises them as OriginTag structs.
    let origin_tags: BTreeMap<String, Vec<serde_json::Value>> = tags
        .into_iter()
        .map(|(h, ts)| {
            let ots = ts
                .into_iter()
                .map(|t| serde_json::json!({"tag": t}))
                .collect();
            (h, ots)
        })
        .collect();
    let snapshot_body = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "cursor": 0,
        "tags": origin_tags,
    });

    let app = axum::Router::new()
        .route(
            REPO_CAPS,
            get(move || {
                let caps = caps.clone();
                async move { axum::Json(caps) }
            }),
        )
        .route(
            "/repo/snapshot",
            get(move || {
                let snap = snapshot_body.clone();
                async move { axum::Json(snap) }
            }),
        )
        // Bucket endpoint for compatibility (returns empty if called).
        .route(
            REPO_BUCKETS,
            post(move || async move {
                axum::Json(serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "cursor": 0,
                    "tags": {},
                }))
            }),
        );

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build snapshot repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind snapshot repo");
            tx.send(listener.local_addr().expect("local_addr"))
                .expect("send addr");
            axum::serve(listener, app)
                .await
                .expect("serve snapshot repo");
        });
    });
    rx.recv().expect("snapshot repo failed to bind")
}

/// Leading-colon and bare-colon tags pulled from a sync repo must be
/// stored in their canonical (namespace, subtag) forms without any discards.
///
/// Tag strings in the snapshot:
///   - "::)"  → Display form of ("", ":)") → must store namespace="" subtag=":)"
///   - ":a:b" → Display form of ("", "a:b") → must store namespace="" subtag="a:b"
///   - ")"    → plain unnamespaced tag      → must store namespace="" subtag=")"
#[test]
fn sync_pull_stores_leading_colon_tags_canonically() {
    // One owned file in the library.
    let owned = hash_bytes(b"owned-for-leading-colon-test");

    // The snapshot uses the file's hex hash as the key, with three tag strings.
    let mut tags = BTreeMap::new();
    tags.insert(
        owned.to_hex(),
        vec![
            "::)".to_string(),  // canonical display of ("",":)")
            ":a:b".to_string(), // canonical display of ("","a:b")
            ")".to_string(),    // plain unnamespaced tag ("",")")
        ],
    );

    let addr = spawn_snapshot_repo(tags);
    let url = format!("http://{addr}");

    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(owned, "/lib/owned.jpg".into(), 1, Some(1)),
        1,
    )
    .unwrap();
    db.add_shared_service("ptr", &url, None).unwrap();
    let db = Mutex::new(db);

    let stats = pull_repo(&db, &CapsCache::new(), "ptr", 256, None).unwrap();
    assert_eq!(stats.matched_files, 1, "owned file must match");

    let db_lock = db.lock().unwrap();
    let fid = db_lock.file_id_by_hash(&owned).unwrap().unwrap();
    let tags_of: Vec<Tag> = db_lock.tags_of(fid).unwrap();

    // Collect (namespace, subtag) pairs for assertion.
    let pairs: Vec<(String, String)> = tags_of
        .iter()
        .map(|t| (t.namespace.clone(), t.subtag.clone()))
        .collect();

    assert!(
        pairs.contains(&("".to_string(), ":)".to_string())),
        "'::)' must be stored as namespace='' subtag=':)'; got {:?}",
        pairs
    );
    assert!(
        pairs.contains(&("".to_string(), "a:b".to_string())),
        "':a:b' must be stored as namespace='' subtag='a:b'; got {:?}",
        pairs
    );
    assert!(
        pairs.contains(&("".to_string(), ")".to_string())),
        "')' must be stored as namespace='' subtag=')'; got {:?}",
        pairs
    );
    assert_eq!(
        pairs.len(),
        3,
        "exactly 3 tags must be stored; got {:?}",
        pairs
    );
}

/// Guard against silent grammar changes: the protocol version constants must
/// remain at v8. Any future tag-grammar change that breaks wire compatibility
/// MUST bump these (per the comment at PROTOCOL_VERSION in naiad-netproto).
///
/// v8 (#162, ADR 0026) folds `origin` (the generation source asserted by the
/// signer) into the signed submission canonical bytes; the two constants move
/// together because pre-1.0 has no compatibility window (ADR 0015).
#[test]
fn protocol_version_is_v8() {
    assert_eq!(
        PROTOCOL_VERSION, 8,
        "PROTOCOL_VERSION must be 8; a grammar change that affects wire \
         compatibility requires a version bump"
    );
    assert_eq!(MIN_SUPPORTED_VERSION, 8, "MIN_SUPPORTED_VERSION must be 8");
}
