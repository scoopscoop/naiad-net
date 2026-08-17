//! End-to-end: `submit_to_repo` threads the source local service's origin into
//! the wire submission (ADR 0026, provenance-by-location). A tag whose winning
//! local service carries `origin = "hydrus"` is published with that origin; a
//! tag mapped only via manual "my tags" (NULL origin) is published with None.
//! Verified by pulling both tags back and checking `pulled_mapping_origin`.

use std::sync::Mutex;

use naiad_core::{FileRecord, Tag, hash_bytes};
use naiad_daemon::{CapsCache, pull_repo, submit_to_repo};
use naiad_db::Db;
use naiad_netproto::Op;
use naiad_server::RepoStore;

#[test]
fn submit_threads_source_service_origin_through_to_repo() {
    // ── 1. Repo ──────────────────────────────────────────────────────────────
    let repo_store = RepoStore::open_in_memory().unwrap();
    let repo = naiad_test_support::spawn_test_repo(repo_store);
    let repo_url = format!("http://{}", repo.addr);

    // ── 2. Client DB: two files, two local services ───────────────────────────
    let file_bytes_a: &[u8] = b"submit-origin-e2e-file-a";
    let file_bytes_b: &[u8] = b"submit-origin-e2e-file-b";
    let hash_a = hash_bytes(file_bytes_a);
    let hash_b = hash_bytes(file_bytes_b);
    let hex_a = hash_a.to_hex();
    let hex_b = hash_b.to_hex();

    let db = Db::open_in_memory().unwrap();
    db.insert_file(
        &FileRecord::new(hash_a, "/lib/a.txt".into(), file_bytes_a.len() as u64, None),
        1,
    )
    .unwrap();
    db.insert_file(
        &FileRecord::new(hash_b, "/lib/b.txt".into(), file_bytes_b.len() as u64, None),
        2,
    )
    .unwrap();

    // Seeded "my tags" has NULL origin (priority 1000 by convention).
    let my_tags = db.local_service_id().unwrap();
    // A second local service that represents an automated tagger.
    let hydrus_svc = db
        .add_local_service("Hydrus: imported tags", Some("hydrus"))
        .unwrap();

    db.add_shared_service("ptr", &repo_url, None).unwrap();

    // File A → tag supplied by the hydrus service (origin = "hydrus").
    let tag_a = Tag::parse("char:samus").unwrap();
    let tag_id_a = db.intern_tag(&tag_a).unwrap();
    let file_id_a = db.file_id_by_hash(&hash_a).unwrap().unwrap();
    db.add_mapping(file_id_a, tag_id_a, hydrus_svc).unwrap();

    // File B → tag supplied by my-tags (origin = NULL).
    let tag_b = Tag::parse("series:metroid").unwrap();
    let tag_id_b = db.intern_tag(&tag_b).unwrap();
    let file_id_b = db.file_id_by_hash(&hash_b).unwrap().unwrap();
    db.add_mapping(file_id_b, tag_id_b, my_tags).unwrap();

    let db_m = Mutex::new(db);
    let key_dir = tempfile::tempdir().unwrap();
    let key = key_dir.path().join("naiad.key");
    let cache = CapsCache::new();

    // ── 3. Submit both tags to the repo ──────────────────────────────────────
    submit_to_repo(&db_m, &cache, &key, "ptr", &hex_a, "char:samus", Op::Add)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .unwrap();
    submit_to_repo(
        &db_m,
        &cache,
        &key,
        "ptr",
        &hex_b,
        "series:metroid",
        Op::Add,
    )
    .map_err(|e| anyhow::anyhow!("{e:?}"))
    .unwrap();

    // ── 4. Pull both tags back ────────────────────────────────────────────────
    let stats = pull_repo(&db_m, &cache, "ptr", 256, None).expect("pull must succeed");
    assert!(
        stats.mappings >= 2,
        "expected at least 2 pulled mappings, got {}",
        stats.mappings
    );

    // ── 5. Assert origins on the pulled mappings ──────────────────────────────
    let db = db_m.lock().unwrap();
    let svc_id = db.shared_service_by_name("ptr").unwrap().unwrap().id;
    let fid_a = db.file_id_by_hash(&hash_a).unwrap().unwrap();
    let fid_b = db.file_id_by_hash(&hash_b).unwrap().unwrap();
    let tid_a = db.intern_tag(&tag_a).unwrap();
    let tid_b = db.intern_tag(&tag_b).unwrap();

    let origin_a = db.pulled_mapping_origin(svc_id, fid_a, tid_a).unwrap();
    assert_eq!(
        origin_a,
        Some("hydrus".to_string()),
        "hydrus-sourced submission must carry origin through to repo and back"
    );

    let origin_b = db.pulled_mapping_origin(svc_id, fid_b, tid_b).unwrap();
    assert!(
        origin_b.is_none(),
        "my-tags-sourced submission must carry NULL origin; got {origin_b:?}"
    );
}
