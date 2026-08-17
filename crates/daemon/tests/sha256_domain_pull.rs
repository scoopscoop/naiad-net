//! End-to-end: pull from a sha256-domain repo (bridge node) lands tags on the
//! correct BLAKE3 file identity. Files with NULL sha256 are silently skipped.
//!
//! Modelled on `bucketed_pull.rs`. The server is spawned with
//! `app_split(..., HashDomain::Sha256)` and seeded via `apply_mappings_bulk`
//! using SHA-256 hex keys (as a bridge node would).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use naiad_core::{FileRecord, hash_reader_dual};
use naiad_daemon::{CapsCache, pull_repo};
use naiad_db::Db;
use naiad_netproto::HashDomain;
use naiad_server::RepoStore;

struct Sha256Repo {
    addr: SocketAddr,
    _handle: JoinHandle<()>,
}

fn spawn_sha256_repo(store: RepoStore, k: u64) -> Sha256Repo {
    let store = Arc::new(Mutex::new(store));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build sha256 repo runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind sha256 repo");
            let addr = listener.local_addr().expect("local_addr");
            tx.send(addr).expect("send bound addr");
            axum::serve(
                listener,
                naiad_server::app_split(store, None, k, None, None, HashDomain::Sha256)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve sha256 repo");
        });
    });
    Sha256Repo {
        addr: rx.recv().expect("sha256 repo failed to bind"),
        _handle: handle,
    }
}

/// A sha256-domain repo in bucketed mode: pull lands tags on the BLAKE3
/// identity derived from the file's sha256, and a file with NULL sha256 is
/// silently skipped (no panic, no mismatched tags).
#[test]
fn sha256_domain_pull_lands_tags_on_blake3_and_skips_null_sha() {
    // Compute real (blake3, sha256) for our test content.
    let content = b"sha256-domain-test-file";
    let (blake3_hash, sha256_hex) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual content");

    // Seed the repo with sha256-keyed mappings. Two entries so count/k >= 2
    // → bucketed mode with k=1 (exercising the bucketed sha256 path).
    let store = RepoStore::open_in_memory().unwrap();
    store
        .apply_mappings_bulk(vec![(
            sha256_hex.clone(),
            "character:samus".to_string(),
            false,
        )])
        .unwrap();
    let (_, sha256_filler) =
        hash_reader_dual(&b"filler-entry"[..]).expect("hash_reader_dual filler");
    store
        .apply_mappings_bulk(vec![(sha256_filler, "filler:tag".to_string(), false)])
        .unwrap();

    // k=1 with 2 distinct hashes → Bucketed { prefix_bits: 1 }.
    let server = spawn_sha256_repo(store, 1);
    let url = format!("http://{}", server.addr);

    // Client library: file-a has both blake3 and sha256; file-b has NULL sha256.
    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(
            blake3_hash,
            "/lib/test.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_hex.clone()),
        1,
    )
    .unwrap();
    let (null_sha_blake3, _) =
        hash_reader_dual(&b"no-sha256-file"[..]).expect("hash_reader_dual null-sha");
    db.insert_file(
        &FileRecord::new(null_sha_blake3, "/lib/null.png".into(), 14, Some(1)),
        2,
    )
    .unwrap();

    db.add_shared_service("bridge", &url, None).unwrap();
    let db = Mutex::new(db);

    // Pull must succeed without error.
    let stats = pull_repo(&db, &CapsCache::new(), "bridge", 256, None)
        .expect("pull_repo must succeed for sha256-domain repo");
    assert!(stats.matched_files >= 1, "at least one file must match");

    let db_lock = db.lock().unwrap();

    // The file with sha256 must carry the tag on its BLAKE3 identity.
    let fid = db_lock
        .file_id_by_hash(&blake3_hash)
        .unwrap()
        .expect("blake3 file must be in the library");
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags.contains(&"character:samus".to_string()),
        "tag must land on the blake3 file identity; got {tags:?}"
    );

    // The NULL-sha file must get no tags and must not have caused any error.
    let null_fid = db_lock
        .file_id_by_hash(&null_sha_blake3)
        .unwrap()
        .expect("null-sha file must be in the library");
    let null_tags = db_lock.tags_of(null_fid).unwrap();
    assert!(
        null_tags.is_empty(),
        "NULL-sha file must receive no tags; got {null_tags:?}"
    );
}

/// Same end-to-end assertion for the WholeRepo pull path (k large enough that
/// the server advertises WholeRepo for a small store). Exercises the WholeRepo
/// arm of `HashDomain::Sha256` in `pull_repo`.
#[test]
fn sha256_domain_wholerepo_pull_lands_tags_on_blake3() {
    let content = b"sha256-wholerepo-test-file";
    let (blake3_hash, sha256_hex) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual content");

    let store = RepoStore::open_in_memory().unwrap();
    store
        .apply_mappings_bulk(vec![(
            sha256_hex.clone(),
            "series:metroid".to_string(),
            false,
        )])
        .unwrap();

    // k=100 with only 1 entry → count(1) < k(100) → WholeRepo mode.
    let server = spawn_sha256_repo(store, 100);
    let url = format!("http://{}", server.addr);

    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(
            blake3_hash,
            "/lib/wr.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_hex.clone()),
        1,
    )
    .unwrap();
    db.add_shared_service("bridge-wr", &url, None).unwrap();
    let db = Mutex::new(db);

    pull_repo(&db, &CapsCache::new(), "bridge-wr", 256, None)
        .expect("wholerepo sha256-domain pull must succeed");

    let db_lock = db.lock().unwrap();
    let fid = db_lock
        .file_id_by_hash(&blake3_hash)
        .unwrap()
        .expect("blake3 file must be in the library");
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags.contains(&"series:metroid".to_string()),
        "tag must land on the blake3 identity via wholerepo sha256 pull; got {tags:?}"
    );
}

/// #158: one malformed `files.sha256` row must not abort the pull. The
/// well-formed file must still receive its tags; the malformed row is skipped.
///
/// We inject a malformed sha256 directly via SQLite (bypassing the normal
/// insert path, which validates the format) to simulate a DB integrity issue.
#[test]
fn malformed_sha256_row_does_not_abort_pull() {
    let content = b"sha256-malformed-test-file";
    let (blake3_good, sha256_good) =
        hash_reader_dual(&content[..]).expect("hash_reader_dual good content");

    // Seed the repo with the good file's sha256-keyed tag.
    let store = RepoStore::open_in_memory().unwrap();
    store
        .apply_mappings_bulk(vec![(
            sha256_good.clone(),
            "character:ridley".to_string(),
            false,
        )])
        .unwrap();
    // k large enough to force WholeRepo (avoids complicating the bucketed path).
    let server = spawn_sha256_repo(store, 100);
    let url = format!("http://{}", server.addr);

    // Client library: one good file + one file with a malformed sha256.
    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(
            blake3_good,
            "/lib/good.png".into(),
            content.len() as u64,
            Some(1),
        )
        .with_sha256(sha256_good.clone()),
        1,
    )
    .unwrap();

    // Inject a malformed sha256 for a second file. `set_sha256_batch` stores
    // whatever string we supply without validating the format, simulating a
    // real data-integrity corruption without needing raw SQL access.
    let (blake3_bad, _) = hash_reader_dual(&b"bad-file"[..]).expect("hash bad");
    db.insert_file(
        &FileRecord::new(blake3_bad, "/lib/bad.png".into(), 8, Some(1)),
        2,
    )
    .unwrap();
    let bad_fid = db.file_id_by_hash(&blake3_bad).unwrap().unwrap();
    db.set_sha256_batch(&[(bad_fid, "NOT-A-VALID-HEX-HASH".to_string())])
        .unwrap();

    db.add_shared_service("bridge-malformed", &url, None)
        .unwrap();
    let db_mutex = Mutex::new(db);

    // Pull must succeed without error (#158: malformed row must not abort).
    pull_repo(&db_mutex, &CapsCache::new(), "bridge-malformed", 256, None)
        .expect("pull must succeed despite a malformed sha256 row");

    let db_lock = db_mutex.lock().unwrap();

    // The good file must carry its tag.
    let fid = db_lock
        .file_id_by_hash(&blake3_good)
        .unwrap()
        .expect("good file must be in the library");
    let tags: Vec<String> = db_lock
        .tags_of(fid)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        tags.contains(&"character:ridley".to_string()),
        "tag must land on the good file despite a malformed sibling row; got {tags:?}"
    );
}
