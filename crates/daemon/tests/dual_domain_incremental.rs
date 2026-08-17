//! Regression test for #151: a repo that co-serves a SHA-256 domain must NOT
//! lose the native BLAKE3 domain's incremental (delta) pull.
//!
//! Before the fix, `pull_repo` routed on the advertised domain list and sent
//! anything that was not exactly `[Blake3]` or `[Sha256]` down a full-snapshot
//! path that ignored `caps.mapping_incremental` and the stored cursor. Enabling
//! a bridge on a repo therefore downgraded every subscriber to a full pull in
//! both domains, forever — a silent, unbounded performance regression.
//!
//! Both facts are asserted here:
//!
//! 1. the second pull's BLAKE3 request carries a non-zero `since`, i.e. it is a
//!    real delta and not a disguised full fetch, and
//! 2. the two domains no longer destroy each other's rows, which is what made
//!    the single coalesced merge necessary in the first place.
//!
//! The wire assertion needs the request bodies, because a full bucket fetch and
//! a bucket delta are the same route (`POST /repo/buckets`) distinguished only
//! by the `since` array. Final database state is identical either way, so it
//! cannot tell the two paths apart.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use naiad_core::{FileRecord, hash_reader_dual};
use naiad_daemon::{CapsCache, pull_repo};
use naiad_db::Db;
use naiad_netproto::HashDomain;
use naiad_server::{DomainConfig, RepoStore, SnapshotBackend};

/// Every `POST /repo/buckets` body the client sent, in order.
type Captured = Arc<Mutex<Vec<serde_json::Value>>>;

struct DualRepo {
    addr: SocketAddr,
    requests: Captured,
    _handle: JoinHandle<()>,
}

impl DualRepo {
    /// The `since` arrays of every bucket request seen so far.
    fn since_arrays(&self) -> Vec<Vec<u64>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|body| {
                body.get("since")
                    .and_then(|s| s.as_array())
                    .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }
}

/// Serve a genuinely dual-domain repo: a native BLAKE3 `RepoStore` plus an
/// attached SHA-256 snapshot backend, which is what makes `/repo/caps`
/// advertise `hash_domains: ["blake3","sha256"]`.
fn spawn_dual_repo(store: RepoStore, snapshot_dir: &std::path::Path, k: u64) -> DualRepo {
    let store = Arc::new(Mutex::new(store));
    let backend = SnapshotBackend::open(snapshot_dir, Some(9)).expect("open snapshot backend");
    let domains = DomainConfig {
        native: HashDomain::Blake3,
        added_sha256: Some(Arc::new(backend) as Arc<dyn naiad_server::Sha256Backend>),
        max_query_bits: 256,
        min_query_bits: 8,
    };
    let requests: Captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&requests);

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build dual repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind dual repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            let app = naiad_server::app_domains(store, None, k, None, None, domains).layer(
                axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let sink = Arc::clone(&sink);
                        async move {
                            // Only bucket requests matter; everything else passes
                            // through untouched.
                            if req.uri().path() != naiad_netproto::REPO_BUCKETS {
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
                ),
            );
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .expect("serve dual repo");
        });
    });

    DualRepo {
        addr: rx.recv().expect("dual repo failed to bind"),
        requests,
        _handle: handle,
    }
}

/// The core #151 assertion: co-serving SHA-256 must leave the BLAKE3 delta path
/// intact, and the two domains must not clobber one another.
#[test]
fn dual_domain_pull_keeps_blake3_incremental_and_preserves_both_domains() {
    let content = b"dual-domain-incremental-file";
    let (blake3_hash, sha256_hex) = hash_reader_dual(&content[..]).expect("hash content");
    let (filler_blake3, _) = hash_reader_dual(&b"dual-domain-filler"[..]).expect("hash filler");

    // ── Native BLAKE3 side ──────────────────────────────────────────────────
    // Two distinct hashes so the repo auto-sizes into Bucketed mode with k=1,
    // which is the only mode that offers an incremental delta.
    let store = RepoStore::open_in_memory().expect("open repo store");
    store
        .apply_mappings_bulk(vec![(
            blake3_hash.to_hex(),
            "character:samus".to_string(),
            false,
        )])
        .expect("seed native mapping");
    store
        .apply_mappings_bulk(vec![(
            filler_blake3.to_hex(),
            "filler:tag".to_string(),
            false,
        )])
        .expect("seed filler mapping");

    // ── Added SHA-256 side ──────────────────────────────────────────────────
    // The same file, keyed by its interop hash, carrying a tag the native
    // domain does not know about.
    let snap_dir = tempfile::tempdir().expect("snapshot tempdir");
    naiad_plugin_hydrus::fixture::write_snapshot(
        snap_dir.path(),
        9,
        &[(sha256_hex.as_str(), "series:metroid")],
    )
    .expect("write sha256 snapshot fixture");

    let server = spawn_dual_repo(store, snap_dir.path(), 1);
    let url = format!("http://{}", server.addr);

    // ── Client library ──────────────────────────────────────────────────────
    let db = Db::open_in_memory().expect("open client db");
    db.insert_file(
        &FileRecord::new(
            blake3_hash,
            "/lib/dual.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_hex.clone()),
        1,
    )
    .expect("insert file");
    let svc_id = db
        .add_shared_service("dual", &url, None)
        .expect("subscribe");
    let db = Mutex::new(db);

    // ── First pull: establishes the cursor in both domains ──────────────────
    pull_repo(&db, &CapsCache::new(), "dual", 256, None).expect("first dual-domain pull");

    {
        let g = db.lock().unwrap();
        let fid = g
            .file_id_by_hash(&blake3_hash)
            .unwrap()
            .expect("file present");
        let mut tags: Vec<String> = g
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        tags.sort();
        assert_eq!(
            tags,
            vec!["character:samus", "series:metroid"],
            "the first pull must land BOTH domains' tags"
        );
        assert!(
            g.mapping_cursor(svc_id, "blake3").unwrap().unwrap_or(0) > 0,
            "the blake3 leg must record an incremental cursor, not skip pull-state"
        );
    }

    // ── Second pull: the one that used to be a full re-fetch ────────────────
    server.clear();
    pull_repo(&db, &CapsCache::new(), "dual", 256, None).expect("second dual-domain pull");

    // 1. The BLAKE3 leg must have asked for a DELTA: at least one bucket
    //    request carried a non-zero `since`. Before #151 every dual-domain pull
    //    sent since = 0 (or omitted it) for every bucket, forever.
    let sinces = server.since_arrays();
    assert!(
        !sinces.is_empty(),
        "the second pull must have issued bucket requests; got none"
    );
    assert!(
        sinces.iter().any(|s| s.iter().any(|&v| v > 0)),
        "the blake3 leg of a dual-domain pull must be incremental (non-zero `since`); \
         got {sinces:?} — this is exactly the #151 regression"
    );

    // 2. Neither domain destroyed the other's rows across the second pull.
    {
        let g = db.lock().unwrap();
        let fid = g
            .file_id_by_hash(&blake3_hash)
            .unwrap()
            .expect("file present");
        let mut tags: Vec<String> = g
            .tags_of(fid)
            .unwrap()
            .iter()
            .map(|t| t.to_string())
            .collect();
        tags.sort();
        assert_eq!(
            tags,
            vec!["character:samus", "series:metroid"],
            "a second dual-domain pull must preserve both domains' tags; the blake3 \
             leg's bucket clear must not reap sha256-sourced rows and vice versa"
        );
    }
}
