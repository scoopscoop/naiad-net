//! The live watcher is timing-based (debounced), so these tests poll with a
//! generous timeout rather than asserting immediate delivery.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use naiad_indexer::{WatchEvent, watch};
use naiad_test_support::fixture_dir;

/// Drain events until `pred` matches one, or time out.
fn wait_for(rx: &Receiver<WatchEvent>, pred: impl Fn(&WatchEvent) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) {
            if pred(&ev) {
                return true;
            }
        }
    }
    false
}

/// Drain events until an Upsert for `expected` arrives; fail if an Upsert for
/// `forbidden` shows up first. Returns once the expected event is seen.
fn upsert_arrives_without(rx: &Receiver<WatchEvent>, expected: &str, forbidden: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(WatchEvent::Upsert(p)) = rx.recv_timeout(Duration::from_millis(500)) {
            assert!(
                !p.ends_with(forbidden),
                "non-image {forbidden} should never be upserted"
            );
            if p.ends_with(expected) {
                return true;
            }
        }
    }
    false
}

#[test]
fn non_image_create_is_not_upserted() {
    let dir = fixture_dir(&[]);
    let root = std::path::absolute(dir.path()).unwrap();
    let (_watcher, rx) = watch(std::slice::from_ref(&root)).expect("start watcher");

    // A non-image and an image created together: the image confirms the watcher
    // is live, while the .dat must never surface as an Upsert.
    std::fs::write(root.join("skip.dat"), b"junk").unwrap();
    std::fs::write(root.join("keep.png"), b"img").unwrap();

    assert!(
        upsert_arrives_without(&rx, "keep.png", "skip.dat"),
        "image create should emit Upsert"
    );
}

#[test]
fn create_modify_remove_emit_events() {
    let dir = fixture_dir(&[]);
    let root = std::path::absolute(dir.path()).unwrap();
    let (_watcher, rx) = watch(std::slice::from_ref(&root)).expect("start watcher");

    // CREATE -> Upsert
    let file = root.join("a.jpg");
    std::fs::write(&file, b"hello").unwrap();
    assert!(
        wait_for(
            &rx,
            |e| matches!(e, WatchEvent::Upsert(p) if p.ends_with("a.jpg"))
        ),
        "create should emit Upsert"
    );

    // MODIFY -> Upsert
    std::fs::write(&file, b"hello world").unwrap();
    assert!(
        wait_for(
            &rx,
            |e| matches!(e, WatchEvent::Upsert(p) if p.ends_with("a.jpg"))
        ),
        "modify should emit Upsert"
    );

    // REMOVE -> Remove
    std::fs::remove_file(&file).unwrap();
    assert!(
        wait_for(
            &rx,
            |e| matches!(e, WatchEvent::Remove(p) if p.ends_with("a.jpg"))
        ),
        "remove should emit Remove"
    );
}
