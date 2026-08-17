//! #142: incremental SHA-256 sync against a live mirror. The wire-shape `since`
//! assertions are what actually pin incrementality (a full bucket fetch and an
//! incremental one merge to the same tags; only the request bodies differ) —
//! modelled on dual_domain_incremental.rs (#151) from the SHA-256 mirror side.
//!
//! The server in these tests is a native-sha256 `RepoStore` (no snapshot
//! backend), so `/repo/caps` advertises `incremental_domains: ["sha256"]` (Task
//! 7). A `since`-recording middleware layer lets each test read back the exact
//! request vectors the client sent.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use naiad_core::{FileRecord, Hash, hash_reader_dual};
use naiad_daemon::{CapsCache, pull_repo};
use naiad_db::Db;
use naiad_netproto::{HashDomain, REPO_BUCKETS, bucket_key};
use naiad_server::RepoStore;

// ── Capturing server wrapper ───────────────────────────────────────────────────

/// Every `POST /repo/buckets` body the client sent, in order.
type Captured = Arc<Mutex<Vec<serde_json::Value>>>;

/// A native-SHA-256 `RepoStore` served in bucketed mode, with a layer that
/// records every `/repo/buckets` request body. Exposes the backing store so
/// tests can mutate mirror content between pulls.
struct Sha256Mirror {
    addr: SocketAddr,
    /// The backing store, shared with the server. Mutate it between pulls to
    /// simulate mirror content changes.
    store: Arc<Mutex<RepoStore>>,
    requests: Captured,
    _handle: JoinHandle<()>,
}

impl Sha256Mirror {
    fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }

    /// All captured request bodies (cloned). Used for per-bucket since
    /// assertions in the watermark soundness test.
    fn captured_bodies(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// Spawn a native-SHA-256 `RepoStore` server in bucketed mode with a
/// `since`-capturing middleware, combining the patterns from
/// `sha256_domain_pull.rs` and `dual_domain_incremental.rs`. Uses
/// `app_split(..., HashDomain::Sha256)` with no snapshot backend so
/// `/repo/caps` advertises `incremental_domains: ["sha256"]` (Task 7).
fn spawn_sha256_mirror_capturing(store: RepoStore, k: u64) -> Sha256Mirror {
    let store = Arc::new(Mutex::new(store));
    let server_store = Arc::clone(&store);
    let requests: Captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&requests);

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build sha256 mirror runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind sha256 mirror");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            let app =
                naiad_server::app_split(server_store, None, k, None, None, HashDomain::Sha256)
                    .layer(axum::middleware::from_fn(
                        move |req: axum::extract::Request, next: axum::middleware::Next| {
                            let sink = Arc::clone(&sink);
                            async move {
                                if req.uri().path() != REPO_BUCKETS {
                                    return next.run(req).await;
                                }
                                let (parts, body) = req.into_parts();
                                let bytes = axum::body::to_bytes(body, usize::MAX)
                                    .await
                                    .expect("buffer bucket request body");
                                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                                    sink.lock().unwrap().push(v);
                                }
                                next.run(axum::extract::Request::from_parts(
                                    parts,
                                    axum::body::Body::from(bytes),
                                ))
                                .await
                            }
                        },
                    ));
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .expect("serve sha256 mirror");
        });
    });

    Sha256Mirror {
        addr: rx.recv().expect("sha256 mirror failed to bind"),
        store,
        requests,
        _handle: handle,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// The wire-shape delta assertion: a second pull against an unchanged mirror
/// must carry non-zero `since` values for its bucket requests, proving the
/// client is on the incremental delta path and not re-fetching everything from
/// scratch. A delta and a full pull merge to the same final state on the client
/// — the request body is the only observable difference.
///
/// Step 3 check: after the first pull, verify that `caps.serves_deltas(sha256)`
/// was true against this mirror (i.e. `mapping_cursor(svc, "sha256") > 0` in
/// the DB — the cursor is stored only if the incremental path was taken).
#[test]
fn sha256_second_pull_sends_nonzero_since_for_changed_bucket() {
    let content_a = b"sha256-incr-delta-file-a";
    let content_b = b"sha256-incr-delta-file-b";
    let (blake3_a, sha256_a) = hash_reader_dual(&content_a[..]).expect("hash content_a");
    let (blake3_b, sha256_b) = hash_reader_dual(&content_b[..]).expect("hash content_b");

    // Seed the mirror with one tag per sha256 key.
    let store = RepoStore::open_in_memory().expect("open mirror store");
    store
        .apply_mappings_bulk(vec![(sha256_a.clone(), "tag:initial-a".to_string(), false)])
        .expect("seed sha256_a mapping");
    store
        .apply_mappings_bulk(vec![(sha256_b.clone(), "tag:initial-b".to_string(), false)])
        .expect("seed sha256_b mapping");
    // Two distinct sha256 keys → count = 2 ≥ k = 1 → Bucketed { prefix_bits: 1 }.

    let server = spawn_sha256_mirror_capturing(store, 1);
    let url = format!("http://{}", server.addr);

    // Client library: both files have their sha256 so bucket keys are derived
    // and sha256_seq is stamped at import time.
    let db = Db::open_in_memory().expect("open client db");
    let m1 = db.next_scan_marker().expect("scan marker 1");
    db.insert_file(
        &FileRecord::new(
            blake3_a,
            "/lib/a.png".into(),
            content_a.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_a.clone()),
        m1,
    )
    .expect("insert file_a");
    let m2 = db.next_scan_marker().expect("scan marker 2");
    db.insert_file(
        &FileRecord::new(
            blake3_b,
            "/lib/b.png".into(),
            content_b.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_b.clone()),
        m2,
    )
    .expect("insert file_b");
    let svc_id = db
        .add_shared_service("sha256-mirror", &url, None)
        .expect("subscribe");
    let db = Mutex::new(db);
    let cache = CapsCache::new();

    // ── First pull: establishes the incremental cursor ──────────────────────
    pull_repo(&db, &cache, "sha256-mirror", 256, None).expect("first pull");

    // Step 3 check: the sha256 domain cursor must be non-zero — it is only
    // stored when the delta path is taken. If it were zero here the second
    // pull's since would also be zero and the test would be green for the
    // WRONG reason (full re-fetch disguised as incremental).
    {
        let g = db.lock().unwrap();
        let cursor = g
            .mapping_cursor(svc_id, "sha256")
            .expect("read sha256 cursor")
            .unwrap_or(0);
        assert!(
            cursor > 0,
            "sha256 mapping_cursor must be non-zero after the first pull; \
             got 0 — the mirror must advertise incremental_domains:[sha256] \
             (Task 7) and the pull must route through pull_domain_delta"
        );
        // Both files must have their initial tags.
        let fid_a = g.file_id_by_hash(&blake3_a).unwrap().unwrap();
        let tags_a: Vec<String> = g
            .tags_of(fid_a)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert!(
            tags_a.contains(&"tag:initial-a".to_string()),
            "file_a must carry initial tag after first pull; got {tags_a:?}"
        );
    }

    // Mutate the mirror: add a delta tag for sha256_a. This bumps the mirror
    // cursor above the stored value, so the second pull carries new content in
    // sha256_a's bucket.
    server
        .store
        .lock()
        .unwrap()
        .apply_mappings_bulk(vec![(sha256_a.clone(), "tag:delta-a".to_string(), false)])
        .expect("add delta tag");

    server.clear();

    // ── Second pull: must be incremental ───────────────────────────────────
    pull_repo(&db, &cache, "sha256-mirror", 256, None).expect("second pull");

    // Wire-shape assertion: sha256_a's specific bucket must have since > 0,
    // proving the changed bucket was fetched incrementally rather than as a
    // full re-fetch (since=0). A full-refetch regression would also deliver
    // the new tag but would show since=0 for every bucket.
    let captured = server.captured_bodies();
    assert!(
        !captured.is_empty(),
        "the second pull must have issued bucket requests; got none"
    );
    let first_body = &captured[0];
    let prefix_bits = first_body["prefix_bits"].as_u64().unwrap_or(1) as u32;
    let sha256_a_hash: Hash = sha256_a.parse().expect("parse sha256_a as Hash");
    let bkey_a = bucket_key(&sha256_a_hash, prefix_bits);
    let buckets = first_body["buckets"].as_array().expect("buckets array");
    let since_arr = first_body["since"].as_array().expect("since array");
    let pos_a = buckets
        .iter()
        .position(|b| b.as_str() == Some(&bkey_a))
        .expect("sha256_a's bucket must appear in the second pull's request");
    let since_a = since_arr[pos_a].as_u64().unwrap_or(0);
    assert!(
        since_a > 0,
        "sha256_a's bucket must be requested with since > 0 on the second pull \
         (incremental delta, not a full re-fetch); got since={since_a} — \
         if since=0 the delta path is not active"
    );

    // State assertion: the delta arrived (file_a has the new tag) and the
    // first pull's rows were not wiped (initial tags survive on both files).
    let g = db.lock().unwrap();
    let fid_a = g.file_id_by_hash(&blake3_a).unwrap().unwrap();
    let mut tags_a: Vec<String> = g
        .tags_of(fid_a)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    tags_a.sort();
    assert!(
        tags_a.contains(&"tag:initial-a".to_string()),
        "initial tag must survive the incremental second pull (no bucket clear); got {tags_a:?}"
    );
    assert!(
        tags_a.contains(&"tag:delta-a".to_string()),
        "delta tag must land on file_a after the second pull; got {tags_a:?}"
    );
    let fid_b = g.file_id_by_hash(&blake3_b).unwrap().unwrap();
    let tags_b: Vec<String> = g
        .tags_of(fid_b)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags_b.contains(&"tag:initial-b".to_string()),
        "file_b initial tag must survive the second pull; got {tags_b:?}"
    );
}

/// A moderation delete on the mirror retracts the corresponding tag on the
/// client via the next delta pull. The sha256 provenance bit is cleared; if
/// that was the only domain supplying the mapping, the row is reaped entirely
/// (Tasks 3/4 implement the per-domain provenance bitmask that makes this
/// correct without touching tags supplied by other services or domains).
#[test]
fn sha256_tombstone_retracts_on_client() {
    let content_a = b"sha256-tombstone-file-a";
    let content_b = b"sha256-tombstone-filler-b"; // filler so bucketed mode kicks in
    let (blake3_a, sha256_a) = hash_reader_dual(&content_a[..]).expect("hash content_a");
    let (blake3_b, sha256_b) = hash_reader_dual(&content_b[..]).expect("hash content_b");

    let store = RepoStore::open_in_memory().expect("open mirror store");
    store
        .apply_mappings_bulk(vec![(
            sha256_a.clone(),
            "character:samus".to_string(),
            false,
        )])
        .expect("seed sha256_a mapping");
    store
        .apply_mappings_bulk(vec![(sha256_b.clone(), "filler:tag".to_string(), false)])
        .expect("seed sha256_b filler");

    let server = spawn_sha256_mirror_capturing(store, 1);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().expect("open client db");
    let m1 = db.next_scan_marker().expect("m1");
    db.insert_file(
        &FileRecord::new(
            blake3_a,
            "/lib/a.png".into(),
            content_a.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_a.clone()),
        m1,
    )
    .expect("insert file_a");
    let m2 = db.next_scan_marker().expect("m2");
    db.insert_file(
        &FileRecord::new(
            blake3_b,
            "/lib/b.png".into(),
            content_b.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_b.clone()),
        m2,
    )
    .expect("insert file_b");
    db.add_shared_service("sha256-tombstone", &url, None)
        .expect("subscribe");
    let db = Mutex::new(db);
    let cache = CapsCache::new();

    // First pull: file_a gets "character:samus".
    pull_repo(&db, &cache, "sha256-tombstone", 256, None).expect("first pull");
    {
        let g = db.lock().unwrap();
        let fid_a = g.file_id_by_hash(&blake3_a).unwrap().unwrap();
        let tags: Vec<String> = g
            .tags_of(fid_a)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert!(
            tags.contains(&"character:samus".to_string()),
            "file_a must carry the tag after the first pull; got {tags:?}"
        );
    }

    // Moderator delete on the mirror: marks the row deleted and bumps seq.
    server
        .store
        .lock()
        .unwrap()
        .apply_mappings_bulk(vec![(
            sha256_a.clone(),
            "character:samus".to_string(),
            true, // is_delete
        )])
        .expect("apply tombstone");

    server.clear();

    // Second pull: the delta carries the deletion; the client must retract it.
    pull_repo(&db, &cache, "sha256-tombstone", 256, None).expect("second pull (tombstone)");

    // Wire-shape assertion: the tombstoned bucket must have been fetched
    // incrementally (since > 0). A full-refetch (since=0) would also produce
    // the correct end state (the deleted row is absent from a full snapshot
    // too), so the end-state check alone cannot distinguish the delta path from
    // a regression that silently fell back to full pulls.
    let captured = server.captured_bodies();
    assert!(
        !captured.is_empty(),
        "the tombstone pull must have issued bucket requests; got none"
    );
    let first_body = &captured[0];
    let prefix_bits = first_body["prefix_bits"].as_u64().unwrap_or(1) as u32;
    let sha256_a_hash: Hash = sha256_a.parse().expect("parse sha256_a as Hash");
    let bkey_a = bucket_key(&sha256_a_hash, prefix_bits);
    let buckets = first_body["buckets"].as_array().expect("buckets array");
    let since_arr = first_body["since"].as_array().expect("since array");
    let pos_a = buckets
        .iter()
        .position(|b| b.as_str() == Some(&bkey_a))
        .expect("sha256_a's bucket must appear in the tombstone pull's request");
    let since_a = since_arr[pos_a].as_u64().unwrap_or(0);
    assert!(
        since_a > 0,
        "sha256_a's bucket must be fetched incrementally (since > 0) on the tombstone pull; \
         got since={since_a} — a full-refetch regression would also satisfy the end-state \
         check but would be invisible without this wire assertion"
    );

    // State assertion: the tag is gone.
    let g = db.lock().unwrap();
    let fid_a = g.file_id_by_hash(&blake3_a).unwrap().unwrap();
    let tags: Vec<String> = g
        .tags_of(fid_a)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        !tags.contains(&"character:samus".to_string()),
        "sha256 tombstone must retract the tag on the client after the next delta pull; \
         got {tags:?}"
    );
}

/// Chunked fetch min-cursor idempotence: forcing a genuine chunk split would
/// require ~850+ distinct bucket keys (BUCKET_REQUEST_BODY_BUDGET = 56 KiB,
/// each key+since entry ≈ 68 bytes), which means hundreds of in-memory hashes
/// and a correspondingly slow test. The min-cursor merge logic
/// (`merged.cursor = merged.cursor.min(delta.cursor)` in
/// `RepoClient::fetch_bucket_delta_inner`) is covered by the netproto unit test
/// `fetch_bucket_delta_in_chunks_buckets_and_since_in_lockstep`
/// (crates/netproto/src/lib.rs ~1612). Injecting a cursor advance between
/// the first and second chunk replies is impractical with the current
/// request-side capturing middleware (it would require the middleware to mutate
/// the store between serial chunk POSTs, which is possible but would significantly
/// complicate the harness for marginal integration coverage beyond the unit test).
/// Marked ignored: reported in the coordinator summary.
#[test]
#[ignore = "genuine chunk split needs ~850 bucket keys; min-cursor semantics \
            covered by netproto::fetch_bucket_delta_in_chunks_buckets_and_since_in_lockstep; \
            see plan §Task 8 note"]
fn sha256_chunked_fetch_takes_min_cursor_and_rereads() {
    // Would need: ~900 files with distinct sha256 values to exceed
    // BUCKET_REQUEST_BODY_BUDGET (56 KiB) in one fetch_bucket_delta_in call,
    // then advance the mirror cursor between chunk POSTs. Impractical here;
    // the min-cursor logic is unit-tested in crates/netproto/src/lib.rs.
}

/// Headline soundness test: a file whose SHA-256 was backfilled AFTER the
/// file-id marker advanced must still have its bucket fetched with `since = 0`
/// on the next pull, so its mapping lands even though the mapping was seeded
/// on the mirror before any pull.
///
/// This test would FAIL under a `files.id` watermark and PASS under
/// `sha256_seq`, which is the exact regression the new watermark guards against.
///
/// Timeline:
///   1. Mirror seeded: sha256_x → "tag:x-soundness" (seq=1),
///      sha256_y → "tag:y" (seq=2), sha256_z → "tag:z" (seq=3). cursor=3.
///   2. Client: insert X (no sha256), Y (sha256_seq=1), Z (sha256_seq=2).
///   3. Subscribe; pull #1 → Y+Z buckets fetched (X has no sha256 → no key).
///      Stored: cursor=3, sha256_marker=2.
///      Note: even if sha256_x's bucket overlaps Y/Z's bucket, the delta entry
///      for sha256_x is dropped by translate_sha256_delta_inputs because X has
///      no sha256 yet and is not in the sha256→blake3 map.
///   4. set_sha256(X, sha256_x) → sha256_seq=3 (ABOVE stored_marker=2).
///   5. Pull #2:
///      - new_keys = {sha256_x's bucket} (sha256_seq=3 > stored_marker=2)
///      - sha256_x's bucket gets since=0 (full fetch)
///      - sha256_y/z's buckets get since=cursor=3 (incremental, no new seq)
///      - Full fetch of sha256_x's bucket delivers "tag:x-soundness" (seq=1)
///        because since=0 fetches seq > 0 = everything
///   6. Assert: X carries "tag:x-soundness".
///
/// Under files.id watermark (how it would break):
///   stored_marker = max_file_id = 3 (X has file_id=1 < Y=2 < Z=3).
///   After set_sha256, file_id does not change. new_keys = {} (no file_id > 3).
///   sha256_x's bucket gets since=cursor=3. Delta since=3 delivers seq > 3.
///   But sha256_x's mapping is at seq=1 (< 3) → NEVER delivered → bug.
#[test]
fn sha256_backfill_after_marker_advance_is_pulled() {
    // Contents whose sha256 values we know in advance (needed to seed the
    // mirror before any client pull).
    let content_x = b"sha256-soundness-file-x"; // inserted WITHOUT sha256 initially
    let content_y = b"sha256-soundness-file-y";
    let content_z = b"sha256-soundness-file-z";
    let (blake3_x, sha256_x) = hash_reader_dual(&content_x[..]).expect("hash x");
    let (blake3_y, sha256_y) = hash_reader_dual(&content_y[..]).expect("hash y");
    let (blake3_z, sha256_z) = hash_reader_dual(&content_z[..]).expect("hash z");

    // 1. Seed mirror: sha256_x at seq=1, sha256_y at seq=2, sha256_z at seq=3.
    //    mirror.mapping_cursor() == 3 after this.
    let store = RepoStore::open_in_memory().expect("open mirror store");
    store
        .apply_mappings_bulk(vec![(
            sha256_x.clone(),
            "tag:x-soundness".to_string(),
            false,
        )])
        .expect("seed sha256_x");
    store
        .apply_mappings_bulk(vec![(sha256_y.clone(), "tag:y".to_string(), false)])
        .expect("seed sha256_y");
    store
        .apply_mappings_bulk(vec![(sha256_z.clone(), "tag:z".to_string(), false)])
        .expect("seed sha256_z");

    let server = spawn_sha256_mirror_capturing(store, 1);
    let url = format!("http://{}", server.addr);

    // 2. Client: insert X WITHOUT sha256 (gets the lowest file_id), then Y and
    //    Z WITH sha256. Y → sha256_seq=1, Z → sha256_seq=2.
    let db = Db::open_in_memory().expect("open client db");
    let m1 = db.next_scan_marker().expect("m1");
    // X has no sha256 — do NOT call .with_sha256(). This is the key difference:
    // X's sha256_seq is NULL here, so it produces no bucket key and the pull
    // cannot request its bucket.
    db.insert_file(
        &FileRecord::new(
            blake3_x,
            "/lib/x.png".into(),
            content_x.len() as u64,
            Some(1),
        ),
        m1,
    )
    .expect("insert file_x (no sha256)");
    let m2 = db.next_scan_marker().expect("m2");
    db.insert_file(
        &FileRecord::new(
            blake3_y,
            "/lib/y.png".into(),
            content_y.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_y.clone()),
        m2,
    )
    .expect("insert file_y");
    let m3 = db.next_scan_marker().expect("m3");
    db.insert_file(
        &FileRecord::new(
            blake3_z,
            "/lib/z.png".into(),
            content_z.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_z.clone()),
        m3,
    )
    .expect("insert file_z");
    let svc_id = db
        .add_shared_service("sha256-soundness", &url, None)
        .expect("subscribe");
    let db = Mutex::new(db);
    let cache = CapsCache::new();

    // 3. Pull #1: only Y+Z have sha256 → only their buckets are requested.
    //    X's bucket is NOT requested (sha256 IS NULL → no bucket key).
    //    sha256_x's mapping may or may not be in Y/Z's bucket range, but even if
    //    it is, translate_sha256_delta_inputs drops it because X is not in the
    //    sha256→blake3 map yet (X has no sha256 on the client).
    //    After pull: stored_cursor=3, sha256_marker=max_sha256_seq=2.
    pull_repo(&db, &cache, "sha256-soundness", 256, None).expect("pull #1");

    {
        let g = db.lock().unwrap();
        // The sha256 cursor must be 3 (mirror cursor after seeding) — proves
        // the delta path ran and we stored the right cursor.
        let cursor = g
            .mapping_cursor(svc_id, "sha256")
            .expect("read cursor")
            .unwrap_or(0);
        assert!(
            cursor > 0,
            "sha256 cursor must be non-zero after pull #1; got 0 — \
             the incremental path was not taken"
        );
        // X must have NO tags (no sha256 → never requested).
        let x_fid = g.file_id_by_hash(&blake3_x).unwrap().unwrap();
        let x_tags: Vec<String> = g
            .tags_of(x_fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert!(
            x_tags.is_empty(),
            "X must have no tags after pull #1 (no sha256 → no bucket key); got {x_tags:?}"
        );
    }

    // 4. Backfill X's sha256: stamps sha256_seq = 3 (ABOVE stored_marker=2).
    //    This is the sha256_seq watermark's soundness moment: seq=3 > marker=2
    //    means X's bucket will be treated as "new" on the next pull.
    {
        let g = db.lock().unwrap();
        let x_fid = g.file_id_by_hash(&blake3_x).unwrap().unwrap();
        g.set_sha256(x_fid, &sha256_x)
            .expect("backfill sha256 for X");
    }

    server.clear();

    // 5. Pull #2: X's sha256_seq=3 > stored_marker=2 → X's bucket key is in
    //    new_keys → since=0 for X's bucket.
    //    Y/Z's buckets: sha256_seq ≤ 2 → since=cursor=3 (incremental).
    //    Full fetch of X's bucket (since=0) delivers "tag:x-soundness" (seq=1
    //    is > 0, so it arrives on the full fetch).
    pull_repo(&db, &cache, "sha256-soundness", 256, None).expect("pull #2");

    // Wire-shape assertion: X's bucket was requested with since=0.
    let captured = server.captured_bodies();
    assert!(
        !captured.is_empty(),
        "pull #2 must have sent at least one bucket request"
    );
    // Use the first chunk (there may be only one chunk for 3 bucket keys).
    let first_body = &captured[0];
    let prefix_bits = first_body["prefix_bits"].as_u64().unwrap_or(1) as u32;
    let sha256_x_hash: Hash = sha256_x.parse().expect("parse sha256_x as Hash");
    let bkey_x = bucket_key(&sha256_x_hash, prefix_bits);
    let buckets = first_body["buckets"].as_array().expect("buckets array");
    let since_arr = first_body["since"].as_array().expect("since array");
    let pos_x = buckets
        .iter()
        .position(|b| b.as_str() == Some(&bkey_x))
        .expect("sha256_x's bucket must be in pull #2's bucket request");
    let since_x = since_arr[pos_x].as_u64().unwrap_or(u64::MAX);
    assert_eq!(
        since_x, 0,
        "sha256_x's bucket must be requested with since=0 (sha256_seq=3 > stored_marker=2); \
         got since={since_x} — under a files.id watermark this would be since=cursor (non-zero) \
         and the mapping at seq=1 would be skipped, causing the soundness bug"
    );

    // State assertion: X now carries its mapping.
    let g = db.lock().unwrap();
    let x_fid = g.file_id_by_hash(&blake3_x).unwrap().unwrap();
    let x_tags: Vec<String> = g
        .tags_of(x_fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        x_tags.contains(&"tag:x-soundness".to_string()),
        "X must carry 'tag:x-soundness' after pull #2 (backfilled sha256 → \
         bucket requested with since=0 → full fetch delivers the mapping); \
         got {x_tags:?}"
    );
}
