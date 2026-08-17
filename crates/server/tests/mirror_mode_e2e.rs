//! Mirror-mode end-to-end tests (design:
//! docs/superpowers/specs/2026-08-05-mirror-mode-e2e-design.md).
//!
//! A mirror repo has `DomainConfig.added_sha256 == None` and a native SHA-256 store,
//! so every request takes the NATIVE branch of `buckets_handler`. These tests
//! drive `bridge seed` -> serve -> bucket pull -> stubbed follow-loop on a small
//! real-schema Hydrus snapshot built inline, and gate a real carved-snapshot run
//! behind `NAIAD_MINI_SNAPSHOT`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use naiad_core::Hash;
use naiad_netproto::{
    Account, Caps, HashDomain, PROTOCOL_VERSION, PullMode, Snapshot, StreamHeader, StreamRow,
    StreamTrailer, bucket_key,
};
use naiad_server::bridge::ptr_client::{Metadata, MetadataEntry};
use naiad_server::bridge::seed;
use naiad_server::bridge::state::StateDb;
use naiad_server::bridge::sync::{UpdateSource, sync_once};
use naiad_server::bridge::{AuditOutcome, parity_audit};
use naiad_server::{DomainConfig, RepoStore, app_domains};
use rusqlite::Connection;
use tower::ServiceExt;

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

    let master = Connection::open(dir.join("client.master.db")).unwrap();
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

    let client = Connection::open(dir.join("client.db")).unwrap();
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

    let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
    mappings
        .execute_batch(
            "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO current_mappings_9 VALUES (2, 1);
             CREATE TABLE deleted_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO deleted_mappings_9 VALUES (3, 1);",
        )
        .unwrap();
}

/// Build the fixture under `dir/snap`, then seed a fresh store + state db under
/// `dir`. Returns the opened (store, state) — both keep their own connection.
fn seed_into(dir: &Path) -> (RepoStore, StateDb) {
    let snap = dir.join("snap");
    build_seed_fixture(&snap);
    let repo = RepoStore::open(dir.join("repo.db")).unwrap();
    let state = StateDb::open(dir.join("state.db")).unwrap();
    seed::run(&snap, Some(9), &repo, &state, &Account::generate(), false).unwrap();
    (repo, state)
}

/// zlib-compress a JSON value exactly as Hydrus does for update files.
fn zlib_json(v: &serde_json::Value) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
    enc.write_all(serde_json::to_string(v).unwrap().as_bytes())
        .unwrap();
    enc.finish().unwrap()
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut d = GzDecoder::new(bytes);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// Build a mirror-mode router (native sha256, no snapshot backend) and attach the
/// mock connect-info layer the handlers require. (The lib adds this only under its
/// own `#[cfg(test)]`, which is inactive when the crate is compiled as a
/// dependency of this integration-test crate.)
fn mirror_router(store: RepoStore) -> Router {
    app_domains(
        Arc::new(Mutex::new(store)),
        None,
        1000, // k: 1 hash < k -> advise() yields WholeRepo, proving the advise branch
        None,
        None,
        DomainConfig::native_only(HashDomain::Sha256),
    )
    .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

#[test]
fn seed_populates_store_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, state) = seed_into(dir.path());

    assert!(
        repo.distinct_hash_count().unwrap() >= 1,
        "seed must populate at least one hash"
    );
    let snap = repo.snapshot().unwrap();
    assert!(
        snap.values()
            .any(|tags| tags.iter().any(|t| t.tag.contains("samus"))),
        "current mapping character:samus must be present"
    );
    assert!(
        !snap
            .values()
            .any(|tags| tags.iter().any(|t| t.tag.contains("badtag"))),
        "deleted mapping meta:badtag must NOT be present"
    );
    assert_eq!(
        state.next_update_index().unwrap(),
        1,
        "cursor must be watermark (0) + 1"
    );

    // Re-run is a no-op (flags already done).
    seed::run(
        &dir.path().join("snap"),
        Some(9),
        &repo,
        &state,
        &Account::generate(),
        false,
    )
    .unwrap();
    assert_eq!(state.next_update_index().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn caps_and_bucket_pull_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _state) = seed_into(dir.path());
    let router = mirror_router(repo);

    // --- caps ---
    let resp = router
        .clone()
        .oneshot(Request::get("/repo/caps").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let caps: Caps = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        caps.hash_domain,
        HashDomain::Sha256,
        "mirror serves sha256 natively"
    );
    assert!(caps.streaming, "server advertises streaming");
    // #195: mirror mode now advertises the floor so clients respect it.
    assert_eq!(
        caps.min_query_bits,
        Some(naiad_server::domain::SNAPSHOT_MIN_QUERY_BITS),
        "mirror mode must advertise min_query_bits (#195)"
    );
    assert_eq!(
        caps.mode,
        PullMode::WholeRepo,
        "advise branch, not max_query_bits"
    );

    // --- buffered bucket pull for SHA_A's 16-bit bucket ---
    let sha = SHA_A.parse::<Hash>().unwrap();
    let req = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 16,
        "buckets": [bucket_key(&sha, 16)],
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post("/repo/buckets")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let snap: Snapshot = serde_json::from_slice(&body).unwrap();
    let tags = snap.tags.get(SHA_A).expect("SHA_A present in bucket");
    assert!(
        tags.iter().any(|t| t.tag == "character:samus"),
        "bucket returns the current tag"
    );
    assert!(
        !tags.iter().any(|t| t.tag.contains("badtag")),
        "deleted mapping must not be served"
    );

    // --- streamed pull ---
    let req = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "prefix_bits": 16,
        "buckets": [bucket_key(&sha, 16)],
        "stream": true,
    });
    let resp = router
        .oneshot(
            Request::post("/repo/buckets")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let header: StreamHeader = serde_json::from_str(lines[0]).unwrap();
    assert!(
        header.cursor >= 1,
        "mirror store carries a real cursor, not 0"
    );
    // Parse the middle line(s) as StreamRow and assert the expected hash/tag.
    let row: StreamRow = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(row.h, SHA_A, "streamed row hash must be SHA_A");
    assert!(
        row.t.iter().any(|t| t.tag == "character:samus"),
        "streamed row must contain character:samus"
    );
    let trailer: StreamTrailer = serde_json::from_str(lines[lines.len() - 1]).unwrap();
    assert!(
        matches!(trailer, StreamTrailer::Done { done: true }),
        "clean finish trailer, got {trailer:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_round_trip_buffered_and_streamed() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _state) = seed_into(dir.path());
    let router = mirror_router(repo);
    let sha = SHA_A.parse::<Hash>().unwrap();

    for stream in [false, true] {
        let req = serde_json::json!({
            "version": PROTOCOL_VERSION,
            "prefix_bits": 16,
            "buckets": [bucket_key(&sha, 16)],
            "stream": stream,
        });
        let resp = router
            .clone()
            .oneshot(
                Request::post("/repo/buckets")
                    .header("content-type", "application/json")
                    .header("accept-encoding", "gzip")
                    .body(Body::from(req.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "response must be gzip-compressed (stream={stream})"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let plain = gunzip(&body);
        let text = String::from_utf8(plain).unwrap();
        assert!(
            text.contains("character:samus"),
            "decoded body must contain the tag (stream={stream})"
        );
    }
}

#[test]
fn follow_loop_applies_delta_past_cursor_and_is_idempotent() {
    struct Fake {
        meta: Metadata,
        files: HashMap<String, Vec<u8>>,
    }
    impl UpdateSource for Fake {
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

    let dir = tempfile::tempdir().unwrap();
    let (repo, state) = seed_into(dir.path());
    assert_eq!(state.next_update_index().unwrap(), 1);

    // Index 1 = one definitions update (introduces service_tag_id 801 ->
    // series:metroid, service_hash_id 500 -> SHA_A) + one content update that
    // adds (tag 801, hash 500). Both are applied within index 1 (defs first).
    let def = zlib_json(&serde_json::json!([
        36,
        1,
        [[0, [[500, SHA_A]]], [1, [[801, "series:metroid"]]]]
    ]));
    let content = zlib_json(&serde_json::json!([34, 1, [[0, [[0, [[801, [500]]]]]]]]));
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
    let files = HashMap::from([(def_h, def), (content_h, content)]);
    let mut src = Fake { meta, files };

    // A single, stable bridge author across both passes — a fresh key per pass
    // would be a latent trap if this fixture ever carried relations.
    let bridge = Account::generate();
    sync_once(&state, &repo, &bridge, &mut src).unwrap();
    assert_eq!(
        state.next_update_index().unwrap(),
        2,
        "cursor advanced past index 1"
    );
    let snap = repo.snapshot().unwrap();
    let tags = snap.get(SHA_A).expect("SHA_A present after follow");
    assert!(
        tags.iter().any(|t| t.tag.contains("metroid")),
        "follow-loop added series:metroid"
    );
    assert_eq!(tags.len(), 2, "exactly samus + metroid");

    // Idempotent replay: rewind the cursor and re-apply the same index.
    state.set_next_update_index(1).unwrap();
    sync_once(&state, &repo, &bridge, &mut src).unwrap();
    assert_eq!(state.next_update_index().unwrap(), 2);
    assert_eq!(
        repo.snapshot().unwrap().get(SHA_A).map(Vec::len),
        Some(2),
        "replay must not duplicate the mapping"
    );
}

/// Parity audit E2E: PASS after clean seed, FAIL after perturbation, REFUSED on
/// watermark mismatch.
#[test]
fn parity_audit_pass_fail_refused() {
    let snap = tempfile::tempdir().unwrap();
    build_seed_fixture(snap.path());
    let repo_dir = tempfile::tempdir().unwrap();
    let db = repo_dir.path().join("repo.db");
    let state_db = repo_dir.path().join("state.db");

    // Seed the mirror from the snapshot.
    {
        let store = RepoStore::open(&db).unwrap();
        let st = StateDb::open(&state_db).unwrap();
        seed::run(
            snap.path(),
            Some(9),
            &store,
            &st,
            &Account::generate(),
            false,
        )
        .unwrap();
    }

    // Watermarks are aligned (W_m = next_update_index - 1 = 0, W_h = 0) -> PASS.
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Pass),
        "fresh seed must produce PASS — tag byte-equality end-to-end"
    );

    // Perturb one current repo_mappings row (mark deleted) -> FAIL.
    // repo_mappings is WITHOUT ROWID; use PRIMARY KEY columns to target the row.
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        let n = c
            .execute("UPDATE repo_mappings SET status = 1 WHERE status = 0", [])
            .unwrap();
        assert!(n >= 1, "perturbation must touch at least one row");
    }
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Fail),
        "perturbed mirror must produce FAIL"
    );

    // Bump the mirror cursor so W_m (= 4) != W_h (= 0) -> REFUSED.
    {
        let st = StateDb::open(&state_db).unwrap();
        st.set_next_update_index(5).unwrap(); // W_m = 5 - 1 = 4, W_h = 0
    }
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Refused),
        "watermark gap must produce REFUSED"
    );
}

/// Parity audit E2E: FAIL when the current-mapping COUNT is unchanged but the
/// tag content differs (digest path).  A regression that dropped `&& m_dig ==
/// h_dig` from the compare would pass this test incorrectly, proving that the
/// digest branch is exercised independently of the count comparison.
#[test]
fn parity_audit_detects_same_count_corruption() {
    let snap = tempfile::tempdir().unwrap();
    build_seed_fixture(snap.path());
    let repo_dir = tempfile::tempdir().unwrap();
    let db = repo_dir.path().join("repo.db");
    let state_db = repo_dir.path().join("state.db");

    // Seed the mirror from the snapshot.
    {
        let store = RepoStore::open(&db).unwrap();
        let st = StateDb::open(&state_db).unwrap();
        seed::run(
            snap.path(),
            Some(9),
            &store,
            &st,
            &Account::generate(),
            false,
        )
        .unwrap();
    }

    // Sanity: fresh seed must be PASS.
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Pass),
        "fresh seed must produce PASS before any corruption"
    );

    // Perturb: rename a tag string so the COUNT of current mappings stays the
    // same but the tag content (and therefore the digest) changes.
    // repo_tags.tag is the string indexed by repo_mappings.tag_id; rewriting it
    // does not add or remove any repo_mappings row.
    let (count_before, count_after) = {
        let c = rusqlite::Connection::open(&db).unwrap();

        let before: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM repo_mappings WHERE status = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let n = c
            .execute(
                "UPDATE repo_tags SET tag = 'character:CORRUPTED' WHERE tag = 'character:samus'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1, "perturbation must rename exactly one tag row");

        let after: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM repo_mappings WHERE status = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();

        (before, after)
    };

    // The count must be identical — only the tag string changed.
    assert_eq!(
        count_before, count_after,
        "current-mapping count must be unchanged after tag rename \
         (before={count_before}, after={count_after})"
    );
    assert!(
        count_before >= 1,
        "fixture must have at least one current mapping"
    );

    // With equal counts but corrupted tag content the audit must return FAIL,
    // which can ONLY come from the digest comparison.
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Fail),
        "same-count tag corruption must produce FAIL (digest path)"
    );
}

/// Build a Hydrus snapshot fixture where SHA_A has TWO tag_ids that both
/// normalize to the same naiad string "maid":
///   - tag_id=1: namespace="" subtag="maid"  → raw "maid"  → normalized "maid"
///   - tag_id=2: namespace="" subtag="Maid"  → raw "Maid"  → normalized "maid"
///
/// The collision is due to `Tag::parse`'s `normalize()` which lowercases the
/// subtag. Hydrus stores these as distinct rows; naiad treats them as one
/// after dedup. The fixture has a valid watermark (index 0 fully processed,
/// index 1 partial) to satisfy `parity_audit`'s watermark gate.
fn build_colliding_tag_fixture(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    let master = Connection::open(dir.join("client.master.db")).unwrap();
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
            // namespace_id=1 is the empty namespace.
            // subtag_id=1 -> "maid" and subtag_id=2 -> "Maid".
            // Both normalize to "maid" via Tag::parse (lowercasing).
            "INSERT INTO namespaces VALUES (1, '');
             INSERT INTO subtags VALUES (1, 'maid'), (2, 'Maid');
             INSERT INTO tags VALUES (1, 1, 1), (2, 1, 2);
             INSERT INTO repository_hash_id_map_9 VALUES (500, 1);
             INSERT INTO repository_tag_id_map_9 VALUES (800, 1);",
        )
        .unwrap();

    let client = Connection::open(dir.join("client.db")).unwrap();
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

    let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
    mappings
        .execute_batch(
            // Both tag_ids (1 and 2) are current mappings for hash_id=1.
            // tag_id=1 → "maid", tag_id=2 → "Maid" — same normalized string.
            "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);
             CREATE TABLE deleted_mappings_9 (tag_id INTEGER, hash_id INTEGER);",
        )
        .unwrap();
}

/// Parity audit must return PASS when the Hydrus snapshot has two distinct
/// tag_ids that normalize to the same string ("maid" / "Maid"), while the
/// mirror store holds the single normalized form ("maid").
///
/// Without `tags.dedup()` in `flush_hash_hydrus`, the Hydrus side would count
/// 2 instead of 1, producing a false FAIL.  With the fix both sides count 1
/// and the digests match → PASS.
///
/// Colliding pair: subtag "maid" and subtag "Maid" — differ only by case.
/// Rule: `normalize()` in `Tag::parse` calls `.to_lowercase()` on each part,
/// so both produce the canonical string "maid".
#[test]
fn parity_audit_dedup_fixes_false_mismatch() {
    let snap = tempfile::tempdir().unwrap();
    build_colliding_tag_fixture(snap.path());

    let repo_dir = tempfile::tempdir().unwrap();
    let db = repo_dir.path().join("repo.db");
    let state_db = repo_dir.path().join("state.db");

    // Seed the mirror: the seed path reads raw Hydrus tags and normalizes them
    // via Tag::parse before storing.  Both "maid" and "Maid" map to "maid", so
    // only ONE mapping row is written to the mirror (the second is idempotent).
    {
        let store = RepoStore::open(&db).unwrap();
        let st = StateDb::open(&state_db).unwrap();
        seed::run(
            snap.path(),
            Some(9),
            &store,
            &st,
            &Account::generate(),
            false,
        )
        .unwrap();
    }

    // The mirror holds exactly 1 mapping; parity_audit must return PASS.
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Pass),
        "colliding-tag snapshot with correctly-seeded mirror must produce PASS; \
         a FAIL would indicate the dedup is missing (pre-fix false mismatch)"
    );
}

/// Parity audit must return FAIL when the mirror genuinely lacks a tag that
/// the Hydrus snapshot carries — proving that dedup does not mask real drift.
///
/// Fixture: Hydrus has SHA_A with "maid" and "character:samus" (two genuinely
/// distinct tags).  The mirror is seeded from the same fixture so it starts as
/// PASS, then one of its mappings is deleted directly in SQL to simulate a
/// real missing-mapping drift.  The audit must return FAIL.
#[test]
fn parity_audit_dedup_does_not_mask_genuine_drift() {
    let snap = tempfile::tempdir().unwrap();
    build_seed_fixture(snap.path()); // SHA_A: character:samus (current)

    let repo_dir = tempfile::tempdir().unwrap();
    let db = repo_dir.path().join("repo.db");
    let state_db = repo_dir.path().join("state.db");

    // Seed the mirror from the snapshot.
    {
        let store = RepoStore::open(&db).unwrap();
        let st = StateDb::open(&state_db).unwrap();
        seed::run(
            snap.path(),
            Some(9),
            &store,
            &st,
            &Account::generate(),
            false,
        )
        .unwrap();
    }

    // Sanity: fresh seed is PASS.
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Pass),
        "fresh seed must be PASS before introducing drift"
    );

    // Introduce genuine drift: mark the current mapping as deleted in the
    // mirror, so the mirror has 0 current mappings while Hydrus has 1.
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        let n = c
            .execute("UPDATE repo_mappings SET status = 1 WHERE status = 0", [])
            .unwrap();
        assert!(
            n >= 1,
            "at least one current mapping must be deleted to create drift"
        );
    }

    // The audit must return FAIL — the genuine missing mapping is not hidden by dedup.
    let out = parity_audit(&db, &state_db, snap.path(), Some(9), None).unwrap();
    assert!(
        matches!(out, AuditOutcome::Fail),
        "genuine missing-mapping drift must produce FAIL even with dedup active"
    );
}

/// Real-data gate: seeds the carved mini-snapshot when `NAIAD_MINI_SNAPSHOT` is
/// set, skips otherwise. Service id is auto-discovered (None) — a real snapshot
/// carries the `services` table.
#[test]
fn mini_snapshot_env_gated_seed() {
    let Ok(snap_dir) = std::env::var("NAIAD_MINI_SNAPSHOT") else {
        eprintln!(
            "skipping mini_snapshot_env_gated_seed: set NAIAD_MINI_SNAPSHOT=<dir> \
             (a carved snapshot dir) to run this"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = RepoStore::open(dir.path().join("repo.db")).unwrap();
    let state = StateDb::open(dir.path().join("state.db")).unwrap();
    seed::run(
        Path::new(&snap_dir),
        None,
        &repo,
        &state,
        &Account::generate(),
        false,
    )
    .unwrap();
    let hashes = repo.distinct_hash_count().unwrap();
    assert!(hashes > 0, "mini-snapshot must seed at least one hash");
    let cursor = state.next_update_index().unwrap();
    // Close the store so the WAL checkpoints into the main file; the on-disk
    // size then reflects the whole store (#180 size gate: the micro corpus must
    // land well under the pre-interning 4.6 GB — spec §8.1 hard gate < 900 MB).
    drop(repo);
    let db = std::fs::metadata(dir.path().join("repo.db")).unwrap().len();
    let wal = std::fs::metadata(dir.path().join("repo.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "mini-snapshot seeded: {hashes} hashes, cursor {cursor}, store {:.1} MB (+{:.1} MB wal)",
        db as f64 / 1_048_576.0,
        wal as f64 / 1_048_576.0
    );
}
