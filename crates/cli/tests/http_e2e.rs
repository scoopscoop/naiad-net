//! End-to-end over a real socket: a `ureq` client (the same crate + DTOs the CLI
//! uses) against a `spawn_test_daemon`. Proves serialization, routing, and the
//! daemon's DB wiring across an actual HTTP hop.

use naiad_api::{FileDto, ScanReq, ScanSummary, TagsReq};
use naiad_db::Db;
use naiad_test_support::{fixture_dir, spawn_test_daemon};

#[test]
fn scan_search_tag_over_http() {
    let db = Db::open_in_memory().unwrap();
    let daemon = spawn_test_daemon(db, 64);
    let base = format!("http://{}", daemon.addr);
    let agent = ureq::AgentBuilder::new().build();

    // A folder to scan (real files on disk the daemon will hash).
    let files = fixture_dir(&[("a.png", b"alpha"), ("b.png", b"beta")]);

    // POST /api/scan
    let summary: ScanSummary = agent
        .post(&format!("{base}/api/scan"))
        .send_json(ScanReq {
            folder: files.path().to_str().unwrap().to_string(),
        })
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(summary.imported, 2);
    assert!(summary.errors.is_empty());

    // GET /api/files -> two files; grab a.png's hash.
    let listed: Vec<FileDto> = agent
        .get(&format!("{base}/api/files"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(listed.len(), 2);
    let hash = listed
        .iter()
        .find(|f| f.name == "a.png")
        .unwrap()
        .hash
        .clone();

    // POST /api/tags/add then GET /api/search?q=character:samus -> a.png only.
    agent
        .post(&format!("{base}/api/tags/add"))
        .send_json(TagsReq {
            file: hash.clone(),
            tags: vec!["character:samus".to_string()],
        })
        .unwrap();

    let hits: Vec<FileDto> = agent
        .get(&format!("{base}/api/search"))
        .query("q", "character:samus")
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "a.png");

    // A malformed query is a 400 over the wire (a `*` in the namespace is invalid;
    // leading/interior `*` in the subtag is now a supported wildcard).
    let err = agent
        .get(&format!("{base}/api/search"))
        .query("q", "ch*r:bad")
        .call()
        .unwrap_err();
    match err {
        ureq::Error::Status(code, _) => assert_eq!(code, 400),
        other => panic!("expected 400, got {other}"),
    }
}
