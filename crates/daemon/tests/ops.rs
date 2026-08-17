//! End-to-end: scan a real folder into a real (temp) database, reconcile, and
//! read it back — the full tracer bullet, in-process, against the ADR 0003
//! content/location split.

use std::fs;
use std::sync::Mutex;

use naiad_core::hash_bytes;
use naiad_daemon::{
    ScanProfile, add_tags, import_path, list_roots, list_tags, reindex_remove, reindex_upsert,
    remove_tags, rescan_roots, scan_streaming,
};
use naiad_test_support::{fixture_dir, temp_db};

#[test]
fn scan_then_list_round_trip() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[
        ("photo.jpg", b"jpeg-bytes"),
        ("nested/clip.png", b"png-bytes"),
        ("notes.webp", b"hello"),
    ]);

    let summary = import_path(&db, files.path(), |e| panic!("unexpected scan error: {e}")).unwrap();
    assert_eq!(summary.imported, 3);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.marked_missing, 0);

    let listed = db.list_files().unwrap();
    assert_eq!(listed.len(), 3);

    // The known-content file's content row is retrievable by its hash.
    let by_hash = db.get_by_hash(&hash_bytes(b"hello")).unwrap().unwrap();
    assert_eq!(by_hash.size, 5);

    // ...and its (single) location points at notes.webp.
    let locs = db.locations_of(&hash_bytes(b"hello")).unwrap();
    assert_eq!(locs.len(), 1);
    assert!(locs[0].path.ends_with("notes.webp"));
    assert!(locs[0].present);
}

#[test]
fn mark_missing_under_hides_files_but_keeps_content() {
    use naiad_daemon::mark_missing_under;
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.jpg", b"aaa"), ("sub/b.png", b"bbb")]);

    let summary = import_path(&db, files.path(), |_| {}).unwrap();
    assert_eq!(summary.imported, 2);
    assert_eq!(db.list_files().unwrap().len(), 2);

    // Hiding the whole scanned root flips both locations missing.
    let hidden = mark_missing_under(&db, files.path()).unwrap();
    assert_eq!(hidden, 2);

    // Files no longer listed (list_files returns only present locations)...
    assert!(db.list_files().unwrap().is_empty());
    // ...but content rows are NOT deleted — still retrievable by hash.
    assert!(db.get_by_hash(&hash_bytes(b"aaa")).unwrap().is_some());
}

#[test]
fn import_extracts_image_metadata() {
    let (db, _db_dir) = temp_db();
    // Minimal GIF89a declaring a 4x7 canvas (see indexer::metadata tests).
    const GIF_4X7: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x04, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x3B,
    ];
    // pic.gif is a real GIF; fake.png has an allowlisted extension but its bytes
    // are not a valid image — it is admitted by extension yet its content sniff
    // finds nothing, so its metadata stays NULL.
    let files = fixture_dir(&[("pic.gif", GIF_4X7), ("fake.png", b"hello")]);

    import_path(&db, files.path(), |e| panic!("unexpected scan error: {e}")).unwrap();

    // The image's content row carries extracted dimensions + MIME.
    let img = db.get_by_hash(&hash_bytes(GIF_4X7)).unwrap().unwrap();
    assert_eq!(img.mime.as_deref(), Some("image/gif"));
    assert_eq!(img.width, Some(4));
    assert_eq!(img.height, Some(7));

    // A file with no recognizable image content stays NULL.
    let txt = db.get_by_hash(&hash_bytes(b"hello")).unwrap().unwrap();
    assert_eq!(txt.mime, None);
    assert_eq!(txt.width, None);
}

#[test]
fn rescan_is_idempotent() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.png", b"data")]);

    import_path(&db, files.path(), |_| {}).unwrap();
    import_path(&db, files.path(), |_| {}).unwrap();

    assert_eq!(db.file_count().unwrap(), 1);
    assert_eq!(db.locations_of(&hash_bytes(b"data")).unwrap().len(), 1);
}

#[test]
fn removed_file_is_marked_missing_not_deleted() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("keep.png", b"keep"), ("gone.png", b"gone")]);

    let first = import_path(&db, files.path(), |_| {}).unwrap();
    assert_eq!(first.imported, 2);
    assert_eq!(first.marked_missing, 0);

    // Delete one file on disk, then rescan.
    fs::remove_file(files.path().join("gone.png")).unwrap();
    let second = import_path(&db, files.path(), |_| {}).unwrap();
    assert_eq!(second.imported, 1); // only keep.png seen
    assert_eq!(second.marked_missing, 1); // gone.txt's location flipped

    // Content row survives; count unchanged.
    assert_eq!(db.file_count().unwrap(), 2);
    let gone = db.locations_of(&hash_bytes(b"gone")).unwrap();
    assert_eq!(gone.len(), 1);
    assert!(!gone[0].present);
    let keep = db.locations_of(&hash_bytes(b"keep")).unwrap();
    assert!(keep[0].present);
}

#[test]
fn scanning_one_folder_leaves_another_folders_files_present() {
    // Regression: the post-scan reconcile must be scoped to the scanned root.
    // Scanning folder B used to flip every location under folder A to missing,
    // because the reconcile ran a global UPDATE with no root scope.
    let (db, _db_dir) = temp_db();
    let a = fixture_dir(&[("a.png", b"alpha")]);
    let b = fixture_dir(&[("b.png", b"beta")]);

    import_path(&db, a.path(), |e| panic!("scan a: {e}")).unwrap();
    let second = import_path(&db, b.path(), |e| panic!("scan b: {e}")).unwrap();

    // Scanning B touched only B's files, so it must reconcile nothing in A.
    assert_eq!(
        second.marked_missing, 0,
        "scanning folder B wrongly reconciled folder A"
    );

    // A's file is still present; B's file is present too.
    let a_loc = db.locations_of(&hash_bytes(b"alpha")).unwrap();
    assert!(
        a_loc[0].present,
        "folder A's file went missing after scanning folder B"
    );
    let b_loc = db.locations_of(&hash_bytes(b"beta")).unwrap();
    assert!(b_loc[0].present);
}

#[test]
fn unchanged_size_and_mtime_skips_the_rehash() {
    // Prove the fast path *skips hashing*, not just that it's idempotent: rewrite
    // the file's CONTENT but force size + mtime to stay identical. If the scan
    // re-hashed, the DB would learn the new bytes; because it skips, it keeps the
    // old hash.
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("x.png", b"aaaa")]);
    let x = files.path().join("x.png");

    import_path(&db, files.path(), |e| panic!("scan 1: {e}")).unwrap();
    assert!(db.get_by_hash(&hash_bytes(b"aaaa")).unwrap().is_some());
    let original_mtime = fs::metadata(&x).unwrap().modified().unwrap();

    // Same 4-byte size, then pin mtime back to the original instant.
    fs::write(&x, b"bbbb").unwrap();
    let f = fs::OpenOptions::new().write(true).open(&x).unwrap();
    f.set_modified(original_mtime).unwrap();
    drop(f);

    import_path(&db, files.path(), |e| panic!("scan 2: {e}")).unwrap();

    // The new bytes were never hashed: only the original content row exists.
    assert!(
        db.get_by_hash(&hash_bytes(b"bbbb")).unwrap().is_none(),
        "unchanged size+mtime should have skipped the re-hash"
    );
    assert!(db.get_by_hash(&hash_bytes(b"aaaa")).unwrap().is_some());
    assert_eq!(db.file_count().unwrap(), 1);
}

#[test]
fn changed_content_is_rehashed() {
    // The flip side: a real content change (different size) is re-hashed and the
    // new content row appears, present.
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("x.png", b"aaaa")]);
    let x = files.path().join("x.png");

    import_path(&db, files.path(), |e| panic!("scan 1: {e}")).unwrap();

    fs::write(&x, b"ccccc").unwrap(); // 5 bytes — size differs, so it can't be skipped
    import_path(&db, files.path(), |e| panic!("scan 2: {e}")).unwrap();

    let changed = db.get_by_hash(&hash_bytes(b"ccccc")).unwrap();
    assert!(changed.is_some(), "a real content change must be re-hashed");
    let loc = db.locations_of(&hash_bytes(b"ccccc")).unwrap();
    assert!(loc[0].present);
}

#[test]
fn scan_streaming_imports_across_batch_boundaries() {
    // More files than SCAN_WRITE_BATCH (256), so the streaming scan flushes in
    // several locked bursts. Each file needs distinct content (files dedupe by
    // hash), so we vary the bytes per file.
    let names: Vec<String> = (0..260).map(|i| format!("f{i:04}.png")).collect();
    let contents: Vec<Vec<u8>> = (0..260)
        .map(|i| format!("content-{i}").into_bytes())
        .collect();
    let specs: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&contents)
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let files = fixture_dir(&specs);

    let (db, _db_dir) = temp_db();
    let db = Mutex::new(db);
    let mut ticks: Vec<(u64, u64, u64)> = Vec::new();
    let summary = scan_streaming(
        &db,
        files.path(),
        ScanProfile::Interactive,
        |e| panic!("scan: {e}"),
        |imported, skipped, total| ticks.push((imported, skipped, total)),
    )
    .unwrap();

    assert_eq!(summary.imported, 260);
    assert_eq!(summary.errors, 0);
    assert_eq!(summary.marked_missing, 0);
    assert_eq!(db.lock().unwrap().file_count().unwrap(), 260);
    // 260 files at batch size 256 → at least two flushes → at least two ticks.
    assert!(
        ticks.len() >= 2,
        "expected ≥2 progress ticks, got {}",
        ticks.len()
    );
    assert_eq!(*ticks.last().unwrap(), (260, 0, 260));
}

#[test]
fn scan_streaming_reports_total_image_count() {
    // 2 supported images + 1 non-image; total must equal 2 (extension filter).
    let files = fixture_dir(&[("a.png", b"aaa"), ("b.png", b"bbb"), ("notes.txt", b"hi")]);

    let (db, _db_dir) = temp_db();
    let db = Mutex::new(db);
    let mut ticks: Vec<(u64, u64, u64)> = Vec::new();
    let summary = scan_streaming(
        &db,
        files.path(),
        ScanProfile::Interactive,
        |e| panic!("scan: {e}"),
        |imported, skipped, total| ticks.push((imported, skipped, total)),
    )
    .unwrap();

    assert_eq!(summary.imported, 2);
    // Every tick must report total == 2 — only the two images are walked, the
    // .txt is filtered out by extension.
    assert!(
        ticks.iter().all(|t| t.2 == 2),
        "every tick must carry total == 2, got {ticks:?}"
    );
}

#[test]
fn scan_streaming_total_is_zero_for_an_empty_dir() {
    let files = fixture_dir(&[]);

    let (db, _db_dir) = temp_db();
    let db = Mutex::new(db);
    let mut ticks: Vec<(u64, u64, u64)> = Vec::new();
    let summary = scan_streaming(
        &db,
        files.path(),
        ScanProfile::Interactive,
        |e| panic!("scan: {e}"),
        |imported, skipped, total| ticks.push((imported, skipped, total)),
    )
    .unwrap();

    assert_eq!(summary.imported, 0);
    // No ticks fire for an empty dir, so this is vacuously true — it only guards
    // against a regression that emits a non-zero total tick. The real check is
    // that the scan succeeds with imported == 0.
    assert!(
        ticks.iter().all(|&(_, _, total)| total == 0),
        "unexpected non-zero total in empty-dir ticks: {ticks:?}",
    );
}

#[test]
fn rescan_roots_completes_an_interrupted_import() {
    // Regression (#59): an import stopped midway leaves the root registered but
    // only the flushed batches persisted; nothing re-imported the rest on the
    // next startup. The catch-up rescan must pick up the never-flushed files.
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("done.png", b"done-bytes"), ("missed.png", b"missed-bytes")]);
    let root = std::path::absolute(files.path()).unwrap();

    // Simulate the interruption: root registered (scan_streaming does this
    // first), one file flushed, the other never reached.
    db.add_root(&root).unwrap();
    reindex_upsert(&db, &root.join("done.png")).unwrap();
    assert_eq!(db.list_files().unwrap().len(), 1);

    let db = Mutex::new(db);
    let summary = rescan_roots(&db, |_| {}).unwrap();
    assert_eq!(
        summary.imported, 2,
        "both files touched by the catch-up scan"
    );
    assert_eq!(summary.errors, 0);

    let db = db.into_inner().unwrap();
    assert_eq!(db.list_files().unwrap().len(), 2);
    let missed = db.locations_of(&hash_bytes(b"missed-bytes")).unwrap();
    assert!(missed[0].present, "the never-flushed file is now imported");
}

#[test]
fn rescan_roots_covers_every_registered_root() {
    let (db, _db_dir) = temp_db();
    let a = fixture_dir(&[("a.png", b"alpha")]);
    let b = fixture_dir(&[("b.png", b"beta")]);
    db.add_root(&std::path::absolute(a.path()).unwrap())
        .unwrap();
    db.add_root(&std::path::absolute(b.path()).unwrap())
        .unwrap();

    let db = Mutex::new(db);
    let summary = rescan_roots(&db, |_| {}).unwrap();
    assert_eq!(summary.imported, 2);

    let db = db.into_inner().unwrap();
    assert!(db.locations_of(&hash_bytes(b"alpha")).unwrap()[0].present);
    assert!(db.locations_of(&hash_bytes(b"beta")).unwrap()[0].present);
}

#[test]
fn rescan_roots_skips_an_unavailable_root_without_hiding_its_files() {
    // A root can be temporarily unreachable at startup (unmounted drive,
    // detached network share). The catch-up scan must skip it — running the
    // scan+reconcile against a vanished directory would flip the whole
    // subtree's locations to missing.
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.png", b"aaa")]);
    import_path(&db, files.path(), |_| {}).unwrap();
    drop(files); // TempDir cleanup: the root's directory no longer exists

    let db = Mutex::new(db);
    let summary = rescan_roots(&db, |_| {}).unwrap();
    assert_eq!(summary.imported, 0);
    assert_eq!(
        summary.marked_missing, 0,
        "unavailable root must not be reconciled"
    );

    let db = db.into_inner().unwrap();
    let loc = db.locations_of(&hash_bytes(b"aaa")).unwrap();
    assert!(
        loc[0].present,
        "files under an unavailable root must stay present, not be hidden"
    );
}

#[test]
fn tag_add_list_remove_by_path_and_hash() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.png", b"aaa"), ("b.png", b"bbb")]);
    import_path(&db, files.path(), |_| {}).unwrap();

    // Tag the first file by PATH; normalization is applied.
    let a = files.path().join("a.png");
    let a = a.to_str().unwrap();
    add_tags(
        &db,
        a,
        &["character:samus".into(), "Creator: Nintendo".into()],
    )
    .unwrap();

    let shown: Vec<String> = list_tags(&db, a)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_eq!(shown, vec!["character:samus", "creator:nintendo"]);

    // Tag the second file by HASH.
    let bhash = hash_bytes(b"bbb").to_hex();
    add_tags(&db, &bhash, &["series:metroid".into()]).unwrap();
    let blist: Vec<String> = list_tags(&db, &bhash)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_eq!(blist, vec!["series:metroid"]);

    // Remove one tag from the first file.
    remove_tags(&db, a, &["creator:nintendo".into()]).unwrap();
    let after: Vec<String> = list_tags(&db, a)
        .unwrap()
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert_eq!(after, vec!["character:samus"]);

    // The file layer is untouched by tagging.
    assert_eq!(db.file_count().unwrap(), 2);
}

#[test]
fn reindex_upsert_then_remove_tracks_presence() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("pic.png", b"img")]);
    let path = files.path().join("pic.png");

    // Upsert imports the file and marks its location present.
    reindex_upsert(&db, &path).unwrap();
    let locs = db.locations_of(&hash_bytes(b"img")).unwrap();
    assert_eq!(locs.len(), 1);
    assert!(locs[0].present);

    // Remove marks the location missing (content row survives).
    reindex_remove(&db, &path).unwrap();
    let locs = db.locations_of(&hash_bytes(b"img")).unwrap();
    assert!(!locs[0].present);
    assert_eq!(db.file_count().unwrap(), 1);
}

#[test]
fn reindex_upsert_on_vanished_file_marks_missing() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("gone.png", b"bye")]);
    let path = files.path().join("gone.png");

    // First import it, then delete it on disk and upsert the now-missing path:
    // the race is handled by falling through to a removal.
    reindex_upsert(&db, &path).unwrap();
    std::fs::remove_file(&path).unwrap();
    reindex_upsert(&db, &path).unwrap();

    let locs = db.locations_of(&hash_bytes(b"bye")).unwrap();
    assert!(!locs[0].present);
}

#[test]
fn import_path_registers_the_root() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.png", b"a")]);
    import_path(&db, files.path(), |_| {}).unwrap();

    let roots = list_roots(&db).unwrap();
    let expected = std::path::absolute(files.path()).unwrap();
    assert_eq!(roots, vec![expected]);
}

#[test]
fn tag_on_unknown_file_errors() {
    let (db, _db_dir) = temp_db();
    let err = add_tags(&db, "/nope/missing.png", &["x:y".into()]).unwrap_err();
    assert!(err.to_string().contains("not in library"));
}

#[test]
fn scan_completes_while_global_rayon_pool_is_saturated() {
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    // Two-phase barrier: `ready` lets us confirm all workers are actually
    // parked before the scan starts (the old single-barrier approach had a
    // race where the scan could finish before any worker blocked, so the
    // broken global-pool code would also pass).
    let n = rayon::current_num_threads();
    let ready = Arc::new(Barrier::new(n + 1)); // "I am parked"
    let gate = Arc::new(Barrier::new(n + 1)); // "you may leave"
    for _ in 0..n {
        let (ready, gate) = (Arc::clone(&ready), Arc::clone(&gate));
        rayon::spawn(move || {
            ready.wait();
            gate.wait();
        });
    }
    ready.wait(); // all n workers are now parked at gate.wait()

    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.jpg", b"aaa"), ("b.png", b"bbb")]);

    // Run the scan on a side thread so a regression shows up as a timeout,
    // not a hung test binary.
    let (tx, rx) = std::sync::mpsc::channel();
    let scan = std::thread::spawn(move || {
        let summary = import_path(&db, files.path(), |e| panic!("scan error: {e}")).unwrap();
        tx.send(summary).unwrap();
    });
    let summary = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("scan wedged behind the saturated global rayon pool");
    assert_eq!(summary.imported, 2);

    gate.wait(); // release the global pool for the rest of the suite
    scan.join().unwrap(); // scan already finished (recv succeeded); join is cleanup
}

#[test]
fn rescan_roots_reports_progress_ticks() {
    let (db, _db_dir) = temp_db();
    let a = fixture_dir(&[("a.png", b"alpha")]);
    let b = fixture_dir(&[("b.png", b"beta")]);
    db.add_root(&std::path::absolute(a.path()).unwrap())
        .unwrap();
    db.add_root(&std::path::absolute(b.path()).unwrap())
        .unwrap();

    let db = Mutex::new(db);
    // (roots_done, roots_total, imported, has_current)
    let mut ticks: Vec<(usize, usize, u64, bool)> = Vec::new();
    let summary = rescan_roots(&db, |p| {
        ticks.push((p.roots_done, p.roots_total, p.imported, p.current.is_some()));
    })
    .unwrap();

    assert_eq!(summary.imported, 2);
    assert!(!ticks.is_empty(), "expected at least one progress tick");
    assert!(
        ticks.iter().all(|&(_, total, _, _)| total == 2),
        "unexpected roots_total in ticks: {ticks:?}",
    );
    let last = *ticks.last().unwrap();
    assert_eq!(last.0, 2, "all roots done at the end");
    assert!(!last.3, "current is cleared at completion");
    let mut prev = 0;
    for &(_, _, imported, _) in &ticks {
        assert!(
            imported >= prev,
            "imported must not go backwards: {ticks:?}"
        );
        prev = imported;
    }
    assert_eq!(prev, 2, "final tick reflects the full import count");
}
