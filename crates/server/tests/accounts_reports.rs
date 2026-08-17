//! End-to-end HTTP tests for the accounts, reports, and moderation flows.
//! Uses a real TCP server (port-0) so multiple sequential oneshot calls share
//! one store via an `Arc<Mutex<RepoStore>>`.
//!
//! Report-close semantics (spec §3): `DeleteMapping` auto-closes all open
//! reports for that `(hash, tag)` pair as part of the same transaction.
//! `Dismiss` closes a single report without touching the mapping.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use naiad_core::{Tag, hash_bytes};
use naiad_netproto::{
    Account, HDR_AUTH_KEY, HDR_AUTH_SIG, HDR_AUTH_TS, MappingStatus, ModerateAction, Op,
    PROTOCOL_VERSION, REPO_MODERATE, REPO_REPORT, REPO_REPORTS, REPO_SUBMIT, Report, ReportList,
    Submission,
};
use naiad_server::{RepoStore, app};
use tower::ServiceExt;

use common::unix_now;

/// Build auth headers for a request.
fn auth_headers(acct: &Account, method: &str, path: &str, body: &[u8]) -> (String, String, String) {
    let ts = unix_now();
    let sig = acct.sign_auth(method, path, None, ts, body);
    (acct.public_hex(), ts.to_string(), sig)
}

/// POST /repo/submit with valid auth headers. Returns the HTTP status.
async fn do_submit(store: Arc<Mutex<RepoStore>>, acct: &Account, sub: &Submission) -> StatusCode {
    let body = serde_json::to_vec(sub).unwrap();
    let (key, ts, sig) = auth_headers(acct, "POST", REPO_SUBMIT, &body);
    app(store, 1000)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, key)
                .header(HDR_AUTH_TS, ts)
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// POST /repo/report with valid auth headers. Returns the HTTP status.
async fn do_report(store: Arc<Mutex<RepoStore>>, acct: &Account, report: &Report) -> StatusCode {
    let body = serde_json::to_vec(report).unwrap();
    let (key, ts, sig) = auth_headers(acct, "POST", REPO_REPORT, &body);
    app(store, 1000)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_REPORT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, key)
                .header(HDR_AUTH_TS, ts)
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// GET /repo/reports with valid auth headers. Returns (status, body bytes).
async fn do_fetch_reports(store: Arc<Mutex<RepoStore>>, acct: &Account) -> (StatusCode, Vec<u8>) {
    let (key, ts, sig) = auth_headers(acct, "GET", REPO_REPORTS, b"");
    let resp = app(store, 1000)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(REPO_REPORTS)
                .header(HDR_AUTH_KEY, key)
                .header(HDR_AUTH_TS, ts)
                .header(HDR_AUTH_SIG, sig)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

/// POST /repo/moderate with valid auth headers. Returns the HTTP status.
async fn do_moderate(
    store: Arc<Mutex<RepoStore>>,
    acct: &Account,
    action: &ModerateAction,
) -> StatusCode {
    let body = serde_json::to_vec(action).unwrap();
    let (key, ts, sig) = auth_headers(acct, "POST", REPO_MODERATE, &body);
    app(store, 1000)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_MODERATE)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, key)
                .header(HDR_AUTH_TS, ts)
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// First signed submit auto-creates a `contributor` account.
#[tokio::test]
async fn first_submit_auto_creates_contributor_account() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    let sub = acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());

    // No account yet.
    assert!(
        store
            .lock()
            .unwrap()
            .account(&acct.public_hex())
            .unwrap()
            .is_none(),
        "account absent before first submit"
    );

    let status = do_submit(store.clone(), &acct, &sub).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Account auto-created as contributor.
    let row = store
        .lock()
        .unwrap()
        .account(&acct.public_hex())
        .unwrap()
        .expect("account must exist after first submit");
    assert_eq!(row.role, "contributor");
    assert!(!row.banned);
}

/// Promote → moderator can read the report queue (GET /repo/reports → 200).
/// A non-moderator gets 403.
#[tokio::test]
async fn moderator_can_read_report_queue_contributor_cannot() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let contributor = Account::generate();
    let moderator = Account::generate();
    let h = hash_bytes(b"file");

    // Both submit once to get accounts created.
    let sub_c = contributor.sign(Op::Add, &h, &Tag::parse("a:x").unwrap());
    let sub_m = moderator.sign(Op::Add, &h, &Tag::parse("a:y").unwrap());
    assert_eq!(
        do_submit(store.clone(), &contributor, &sub_c).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        do_submit(store.clone(), &moderator, &sub_m).await,
        StatusCode::NO_CONTENT
    );

    // Promote moderator.
    store
        .lock()
        .unwrap()
        .set_role(&moderator.public_hex(), "moderator")
        .unwrap();

    // Moderator can access the queue.
    let (mod_status, body) = do_fetch_reports(store.clone(), &moderator).await;
    assert_eq!(mod_status, StatusCode::OK, "moderator must get 200");
    let list: ReportList = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.version, PROTOCOL_VERSION);

    // Contributor (not a moderator) gets 403.
    let (contrib_status, _) = do_fetch_reports(store.clone(), &contributor).await;
    assert_eq!(
        contrib_status,
        StatusCode::FORBIDDEN,
        "contributor must get 403"
    );
}

/// Banned key's submit AND report are rejected with 403 (HTTP-layer assertion —
/// covers the `is_banned_err` seam). Existing rows are untouched.
#[tokio::test]
async fn banned_key_gets_403_on_submit_and_report() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let acct = Account::generate();
    let h = hash_bytes(b"file");
    let h_hex = h.to_hex();

    // First submit creates the account.
    let sub1 = acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());
    assert_eq!(
        do_submit(store.clone(), &acct, &sub1).await,
        StatusCode::NO_CONTENT
    );

    // Ban the account.
    store
        .lock()
        .unwrap()
        .set_banned(&acct.public_hex(), true)
        .unwrap();

    // Submit after ban → 403.
    let sub2 = acct.sign(Op::Add, &h, &Tag::parse("series:metroid").unwrap());
    assert_eq!(
        do_submit(store.clone(), &acct, &sub2).await,
        StatusCode::FORBIDDEN,
        "banned submit must yield 403"
    );

    // Report after ban → 403.
    let report = Report {
        version: PROTOCOL_VERSION,
        hash: h_hex.clone(),
        tag: "character:samus".to_string(),
        note: None,
    };
    assert_eq!(
        do_report(store.clone(), &acct, &report).await,
        StatusCode::FORBIDDEN,
        "banned report must yield 403"
    );

    // The original mapping is untouched: banning does not retract existing submissions.
    let snap = store.lock().unwrap().snapshot().unwrap();
    assert!(
        snap.get(&h_hex)
            .map(|tags| tags.iter().any(|t| t.tag == "character:samus"))
            .unwrap_or(false),
        "existing mapping must survive a ban"
    );
    assert!(
        !snap
            .get(&h_hex)
            .map(|tags| tags.iter().any(|t| t.tag == "series:metroid"))
            .unwrap_or(false),
        "the banned submit's tag must NOT appear (rejected before store)"
    );
}

/// Full report flow: client files a report → it appears in the moderator queue
/// → `DeleteMapping` auto-closes the report AND deletes the mapping (spec §3).
/// → `Dismiss` separately closes a report without touching the mapping.
#[tokio::test]
async fn report_flow_delete_mapping_auto_closes_report() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let reporter = Account::generate();
    let moderator = Account::generate();
    let h = hash_bytes(b"reported_file");
    let h_hex = h.to_hex();
    let tag = "character:samus";

    // Set up: reporter submits, moderator submits to get accounts; promote mod.
    let sub_r = reporter.sign(Op::Add, &h, &Tag::parse(tag).unwrap());
    let sub_m = moderator.sign(Op::Add, &h, &Tag::parse("a:y").unwrap());
    assert_eq!(
        do_submit(store.clone(), &reporter, &sub_r).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        do_submit(store.clone(), &moderator, &sub_m).await,
        StatusCode::NO_CONTENT
    );
    store
        .lock()
        .unwrap()
        .set_role(&moderator.public_hex(), "moderator")
        .unwrap();

    // Reporter files a report.
    let report = Report {
        version: PROTOCOL_VERSION,
        hash: h_hex.clone(),
        tag: tag.to_string(),
        note: Some("spam".to_string()),
    };
    assert_eq!(
        do_report(store.clone(), &reporter, &report).await,
        StatusCode::NO_CONTENT
    );

    // Report appears in the moderator queue.
    let (status, body) = do_fetch_reports(store.clone(), &moderator).await;
    assert_eq!(status, StatusCode::OK);
    let list: ReportList = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.rows.len(), 1, "one open report");
    assert_eq!(list.rows[0].hash, h_hex);
    assert_eq!(list.rows[0].tag, tag);
    assert_eq!(list.rows[0].note.as_deref(), Some("spam"));
    assert_eq!(list.rows[0].status, "open");

    // Moderator deletes the mapping.
    let delete_action = ModerateAction::DeleteMapping {
        hash: h_hex.clone(),
        tag: tag.to_string(),
    };
    assert_eq!(
        do_moderate(store.clone(), &moderator, &delete_action).await,
        StatusCode::NO_CONTENT
    );

    // Mapping is now deleted.
    let snap = store.lock().unwrap().snapshot().unwrap();
    assert!(
        !snap
            .get(&h_hex)
            .map(|tags| tags.iter().any(|t| t.tag == tag))
            .unwrap_or(false),
        "mapping must be deleted"
    );

    // spec §3: DeleteMapping auto-closes all open reports for that (hash, tag).
    // The queue is now empty without a separate Dismiss.
    let (status2, body2) = do_fetch_reports(store.clone(), &moderator).await;
    assert_eq!(status2, StatusCode::OK);
    let list2: ReportList = serde_json::from_slice(&body2).unwrap();
    assert!(
        list2.rows.is_empty(),
        "report must be auto-closed by DeleteMapping (spec §3)"
    );

    // The mapping deletion is visible in the bucket delta.
    let lo = "0".repeat(64);
    let hi = "g";
    let delta = store
        .lock()
        .unwrap()
        .bucket_delta(&lo, hi, 0, usize::MAX)
        .unwrap()
        .0;
    let deleted_entry = delta
        .iter()
        .find(|d| d.hash == h_hex && d.tag == tag)
        .expect("deleted mapping must appear in delta");
    assert_eq!(deleted_entry.status, MappingStatus::Deleted);
}

/// `Dismiss` closes a report without touching the mapping (spec §3).
#[tokio::test]
async fn dismiss_closes_report_without_deleting_mapping() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let reporter = Account::generate();
    let moderator = Account::generate();
    let h = hash_bytes(b"dismiss_test");
    let h_hex = h.to_hex();
    let tag = "character:samus";

    // Bootstrap.
    let sub_r = reporter.sign(Op::Add, &h, &Tag::parse(tag).unwrap());
    let sub_m = moderator.sign(Op::Add, &h, &Tag::parse("a:y").unwrap());
    do_submit(store.clone(), &reporter, &sub_r).await;
    do_submit(store.clone(), &moderator, &sub_m).await;
    store
        .lock()
        .unwrap()
        .set_role(&moderator.public_hex(), "moderator")
        .unwrap();

    // File a report.
    let report = Report {
        version: PROTOCOL_VERSION,
        hash: h_hex.clone(),
        tag: tag.to_string(),
        note: None,
    };
    do_report(store.clone(), &reporter, &report).await;

    // Get the report id.
    let (_, body) = do_fetch_reports(store.clone(), &moderator).await;
    let list: ReportList = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.rows.len(), 1);
    let report_id = list.rows[0].id;

    // Dismiss the report.
    let dismiss = ModerateAction::Dismiss { report_id };
    assert_eq!(
        do_moderate(store.clone(), &moderator, &dismiss).await,
        StatusCode::NO_CONTENT
    );

    // Queue is now empty.
    let (_, body2) = do_fetch_reports(store.clone(), &moderator).await;
    let list2: ReportList = serde_json::from_slice(&body2).unwrap();
    assert!(list2.rows.is_empty(), "queue empty after dismiss");

    // Mapping is still present (Dismiss does not delete it).
    let snap = store.lock().unwrap().snapshot().unwrap();
    assert!(
        snap.get(&h_hex)
            .map(|tags| tags.iter().any(|t| t.tag == tag))
            .unwrap_or(false),
        "Dismiss must not delete the mapping"
    );
}

/// `Ban` action blocks future submits.
#[tokio::test]
async fn ban_action_blocks_future_submits() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let target = Account::generate();
    let moderator = Account::generate();
    let h = hash_bytes(b"ban_test");

    // Bootstrap accounts.
    let sub_t = target.sign(Op::Add, &h, &Tag::parse("a:x").unwrap());
    let sub_m = moderator.sign(Op::Add, &h, &Tag::parse("a:y").unwrap());
    assert_eq!(
        do_submit(store.clone(), &target, &sub_t).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        do_submit(store.clone(), &moderator, &sub_m).await,
        StatusCode::NO_CONTENT
    );
    store
        .lock()
        .unwrap()
        .set_role(&moderator.public_hex(), "moderator")
        .unwrap();

    // Moderator bans the target via HTTP.
    let ban = ModerateAction::Ban {
        pubkey: target.public_hex(),
    };
    assert_eq!(
        do_moderate(store.clone(), &moderator, &ban).await,
        StatusCode::NO_CONTENT
    );

    // Target's next submit is blocked at the HTTP layer → 403.
    let sub2 = target.sign(Op::Add, &h, &Tag::parse("b:z").unwrap());
    assert_eq!(
        do_submit(store.clone(), &target, &sub2).await,
        StatusCode::FORBIDDEN,
        "banned account must get 403 on submit"
    );
}

/// Non-sticky moderator delete: delete → re-Add resurrects (HTTP-level).
#[tokio::test]
async fn moderator_delete_is_non_sticky_re_add_resurrects() {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let contributor = Account::generate();
    let moderator = Account::generate();
    let h = hash_bytes(b"nonsticky");
    let h_hex = h.to_hex();
    let tag = "character:samus";

    // Bootstrap.
    let sub_c = contributor.sign(Op::Add, &h, &Tag::parse(tag).unwrap());
    let sub_m = moderator.sign(Op::Add, &h, &Tag::parse("a:y").unwrap());
    assert_eq!(
        do_submit(store.clone(), &contributor, &sub_c).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        do_submit(store.clone(), &moderator, &sub_m).await,
        StatusCode::NO_CONTENT
    );
    store
        .lock()
        .unwrap()
        .set_role(&moderator.public_hex(), "moderator")
        .unwrap();

    // Moderator deletes the mapping.
    let delete_action = ModerateAction::DeleteMapping {
        hash: h_hex.clone(),
        tag: tag.to_string(),
    };
    assert_eq!(
        do_moderate(store.clone(), &moderator, &delete_action).await,
        StatusCode::NO_CONTENT
    );

    // Mapping is gone.
    let snap1 = store.lock().unwrap().snapshot().unwrap();
    assert!(
        !snap1
            .get(&h_hex)
            .map(|tags| tags.iter().any(|t| t.tag == tag))
            .unwrap_or(false),
        "mapping deleted"
    );

    // Re-Add by contributor resurrects the mapping (non-sticky delete).
    let sub_re = contributor.sign(Op::Add, &h, &Tag::parse(tag).unwrap());
    assert_eq!(
        do_submit(store.clone(), &contributor, &sub_re).await,
        StatusCode::NO_CONTENT
    );

    let snap2 = store.lock().unwrap().snapshot().unwrap();
    assert!(
        snap2
            .get(&h_hex)
            .map(|tags| tags.iter().any(|t| t.tag == tag))
            .unwrap_or(false),
        "re-Add must resurrect the mapping (non-sticky)"
    );
}
