//! End-to-end live watching over a real socket: scan a folder, then create and
//! delete a file in it and watch the library update without another scan.
//! Timing-tolerant: the watcher debounces (~500ms), so we poll with a timeout.

use std::time::{Duration, Instant};

use naiad_api::{FileDto, ScanReq, ScanSummary};
use naiad_db::Db;
use naiad_test_support::{fixture_dir, spawn_test_daemon_watching};

fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
fn watcher_imports_creates_and_marks_deletes_missing() {
    let db = Db::open_in_memory().unwrap();
    let daemon = spawn_test_daemon_watching(db, 64);
    let base = format!("http://{}", daemon.addr);
    let agent = ureq::AgentBuilder::new().build();

    // An (initially empty) folder; scanning it registers + live-watches it.
    let dir = fixture_dir(&[]);
    let _summary: ScanSummary = agent
        .post(&format!("{base}/api/scan"))
        .send_json(ScanReq {
            folder: dir.path().to_str().unwrap().to_string(),
        })
        .unwrap()
        .into_json()
        .unwrap();

    // Poll until the watcher is live on the just-registered root.  The
    // watcher starts asynchronously after scan registration; a file written
    // before watching begins is invisible (no event fires).  To handle that
    // race, the closure (re)writes the probe on every iteration so the next
    // write always fires a CREATE or MODIFY event once the watcher is up.
    // The 700ms inner sleep gives the debouncer (~500ms) time to flush.
    let probe = dir.path().join("_probe.png");
    wait_for(Duration::from_secs(15), || {
        std::fs::write(&probe, b"probe-bytes").ok();
        std::thread::sleep(Duration::from_millis(700));
        let files: Vec<FileDto> = agent
            .get(&format!("{base}/api/files"))
            .call()
            .ok()?
            .into_json()
            .ok()?;
        files.iter().any(|f| f.name == "_probe.png").then_some(())
    })
    .expect("watcher did not index the probe file within 15 seconds");
    std::fs::remove_file(&probe).ok();

    // CREATE: the watcher should import it with no further scan.
    let file = dir.path().join("fresh.png");
    std::fs::write(&file, b"fresh-bytes").unwrap();
    let hash = wait_for(Duration::from_secs(15), || {
        let files: Vec<FileDto> = agent
            .get(&format!("{base}/api/files"))
            .call()
            .ok()?
            .into_json()
            .ok()?;
        files
            .into_iter()
            .find(|f| f.name == "fresh.png")
            .map(|f| f.hash)
    })
    .expect("watcher should import the created file");

    // While present, its bytes are served.
    assert!(
        agent.get(&format!("{base}/file/{hash}")).call().is_ok(),
        "present file should be served"
    );

    // DELETE: the watcher should mark the location missing -> /file 404s.
    std::fs::remove_file(&file).unwrap();
    let gone = wait_for(Duration::from_secs(15), || {
        match agent.get(&format!("{base}/file/{hash}")).call() {
            Err(ureq::Error::Status(404, _)) => Some(()),
            _ => None,
        }
    });
    assert!(
        gone.is_some(),
        "watcher should mark the deleted file missing"
    );
}
