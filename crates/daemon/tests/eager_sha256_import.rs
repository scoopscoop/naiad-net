//! Regression lock (issue #124): a scanned-and-imported file must persist its
//! SHA-256 eagerly, so the bridge's sha256-domain pull can rely on it.
//!
//! The import path already does this (indexer::hash_file → Db::insert_file with
//! `ON CONFLICT … DO UPDATE SET sha256 = COALESCE(excluded.sha256, files.sha256)`);
//! this test guards against a refactor silently dropping it.

use std::sync::Mutex;

use naiad_core::hash_reader_dual;
use naiad_daemon::{ScanProfile, scan_streaming};
use naiad_db::Db;
use naiad_test_support::fixture_dir;

const PAYLOAD: &[u8] = b"eager-dual-hash-payload";

#[test]
fn scanned_file_row_has_matching_sha256() {
    // A single PNG-named file (extension is the scanner's admission criterion;
    // content need not be a valid image for hashing to succeed).
    let files = fixture_dir(&[("a.png", PAYLOAD)]);

    let db = Mutex::new(Db::open_in_memory().unwrap());
    let summary = scan_streaming(
        &db,
        files.path(),
        ScanProfile::Interactive,
        |e| panic!("unexpected scan error: {e}"),
        |_, _, _| {},
    )
    .unwrap();
    assert!(
        summary.imported >= 1,
        "expected one file imported; got {summary:?}"
    );

    // Compute the expected hashes from the same bytes.
    let (blake, expected_sha) = hash_reader_dual(PAYLOAD).unwrap();

    let stored = db.lock().unwrap().sha256_of(&blake).unwrap();
    assert_eq!(
        stored.as_deref(),
        Some(expected_sha.as_str()),
        "files.sha256 must equal hash_reader_dual over the same bytes; got {stored:?}"
    );
}
