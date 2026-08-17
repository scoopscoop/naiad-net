//! HTTP-level authentication tests for the `x-naiad-key` / `x-naiad-ts` /
//! `x-naiad-sig` request-auth scheme. Exercises the `authenticate()` helper
//! inside the submit handler via `tower::ServiceExt::oneshot`.
//!
//! All assertions are on HTTP status codes (401 Unauthorized for auth failures).
//! Unit-level verify_auth tests live in `crates/netproto/src/auth.rs`.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use naiad_core::{Tag, hash_bytes};
use naiad_netproto::{
    AUTH_FRESHNESS_SECS, Account, HDR_AUTH_KEY, HDR_AUTH_SIG, HDR_AUTH_TS, HashDomain, Op,
    REPO_REPORTS, REPO_SUBMIT,
};
use naiad_server::{RepoStore, app};
use tower::ServiceExt;

use common::unix_now;

/// Make a fresh store and app.
fn fresh_app() -> (Arc<Mutex<RepoStore>>, axum::Router) {
    let store = Arc::new(Mutex::new(RepoStore::open_in_memory().unwrap()));
    let router = app(store.clone(), 1000);
    (store, router)
}

/// Build the JSON body of a signed Submission for use in auth tests.
fn make_sub_body(acct: &Account) -> Vec<u8> {
    let h = hash_bytes(b"auth_test");
    let sub = acct.sign(Op::Add, &h, &Tag::parse("a:x").unwrap());
    serde_json::to_vec(&sub).unwrap()
}

/// POST /repo/submit with an explicit ts and sig (for freshness edge-case tests).
async fn submit_with_explicit_ts(
    router: axum::Router,
    acct: &Account,
    ts: i64,
    body: Vec<u8>,
) -> StatusCode {
    let sig = acct.sign_auth("POST", REPO_SUBMIT, None, ts, &body);
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

// ── Freshness bounds ──────────────────────────────────────────────────────────

/// A timestamp at "now" is accepted (returns non-401).
#[tokio::test]
async fn fresh_timestamp_is_accepted() {
    let (_, router) = fresh_app();
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let ts = unix_now();
    let status = submit_with_explicit_ts(router, &acct, ts, body).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "fresh timestamp must not produce 401"
    );
}

/// A timestamp 1 second inside the boundary (now−299) is accepted.
/// Using now−299 rather than now−300 avoids a race where a wall-clock
/// second passes between signing and the server's freshness check, which
/// would push |diff| to 301 and cause a spurious 401.
#[tokio::test]
async fn timestamp_inside_boundary_is_accepted() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let ts = unix_now() - (AUTH_FRESHNESS_SECS - 1);
    let (_, router) = fresh_app();
    let status = submit_with_explicit_ts(router, &acct, ts, body).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "timestamp 1 s inside the window must not produce 401"
    );
}

/// A timestamp >300 s in the past is rejected with 401.
#[tokio::test]
async fn stale_past_timestamp_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let stale_ts = unix_now() - AUTH_FRESHNESS_SECS - 10;
    let (_, router) = fresh_app();
    assert_eq!(
        submit_with_explicit_ts(router, &acct, stale_ts, body).await,
        StatusCode::UNAUTHORIZED,
        "stale past timestamp must yield 401"
    );
}

/// A timestamp >300 s in the future is rejected with 401.
#[tokio::test]
async fn stale_future_timestamp_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let future_ts = unix_now() + AUTH_FRESHNESS_SECS + 10;
    let (_, router) = fresh_app();
    assert_eq!(
        submit_with_explicit_ts(router, &acct, future_ts, body).await,
        StatusCode::UNAUTHORIZED,
        "stale future timestamp must yield 401"
    );
}

/// `i64::MIN` timestamp must not panic — the server must return 401.
#[tokio::test]
async fn i64_min_timestamp_no_panic() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    assert_eq!(
        submit_with_explicit_ts(router, &acct, i64::MIN, body).await,
        StatusCode::UNAUTHORIZED,
        "i64::MIN timestamp must be stale and yield 401, not panic"
    );
}

/// `i64::MAX` timestamp must not panic — the server must return 401.
#[tokio::test]
async fn i64_max_timestamp_no_panic() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    assert_eq!(
        submit_with_explicit_ts(router, &acct, i64::MAX, body).await,
        StatusCode::UNAUTHORIZED,
        "i64::MAX timestamp must be stale and yield 401, not panic"
    );
}

// ── Signature / key failures ──────────────────────────────────────────────────

/// A valid signature by signer but the wrong key in the header → 401.
#[tokio::test]
async fn wrong_key_in_header_is_rejected() {
    let signer = Account::generate();
    let other = Account::generate();
    let body = make_sub_body(&signer);
    let ts = unix_now();
    // Signer's sig, but OTHER's pubkey in the header.
    let sig = signer.sign_auth("POST", REPO_SUBMIT, None, ts, &body);
    let (_, router) = fresh_app();
    let status = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, other.public_hex()) // wrong key
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong key must yield 401");
}

/// The sig was computed over the original body; the server receives a tampered
/// body → 401 (body hash mismatch in canonical bytes).
#[tokio::test]
async fn tampered_body_is_rejected() {
    let acct = Account::generate();
    let original_body = make_sub_body(&acct);
    let ts = unix_now();
    // Sign over original body.
    let sig = acct.sign_auth("POST", REPO_SUBMIT, None, ts, &original_body);
    // Send a different body.
    let tampered_body =
        b"{\"version\":6,\"op\":\"add\",\"hash\":\"aa\",\"tag\":\"x:y\",\"author\":\"bb\",\"signature\":\"cc\"}"
            .to_vec();
    let (_, router) = fresh_app();
    let status = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(tampered_body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "tampered body must yield 401"
    );
}

/// The sig was computed for `/repo/submit`; it is replayed on a different path
/// (`/repo/moderate`) with the same method and body → 401.
/// This isolates path binding in canonical bytes.
#[tokio::test]
async fn replayed_sig_on_different_path_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let ts = unix_now();
    // Sign for /repo/submit.
    let sig = acct.sign_auth("POST", REPO_SUBMIT, None, ts, &body);
    // Replay on /repo/moderate — same method, same body, different path.
    let (_, router) = fresh_app();
    let status = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/repo/moderate")
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "sig replayed on different path must yield 401"
    );
}

/// Method-only replay: sign "POST /repo/reports" with empty body, then send
/// the request as "GET /repo/reports" with the same path and same (empty) body.
/// The GET /repo/reports handler calls authenticate("GET", ...) internally;
/// the sig was produced over "POST" → canonical-bytes mismatch → 401.
/// Path and body are identical — only the method binding is under test.
#[tokio::test]
async fn replayed_sig_on_different_method_is_rejected() {
    let acct = Account::generate();
    let ts = unix_now();
    // Sign for POST /repo/reports with empty body.
    let sig = acct.sign_auth("POST", REPO_REPORTS, None, ts, b"");
    let (_, router) = fresh_app();
    // Send as GET /repo/reports — same path, same empty body, only method differs.
    let status = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(REPO_REPORTS)
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "sig signed for POST but request sent as GET must yield 401 (method is bound in canonical bytes)"
    );
}

// ── Missing headers ───────────────────────────────────────────────────────────

/// All three auth headers missing → 401.
#[tokio::test]
async fn missing_all_auth_headers_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing auth headers must yield 401"
    );
}

/// Only the key header present (ts and sig absent) → 401.
#[tokio::test]
async fn missing_ts_and_sig_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                // HDR_AUTH_TS and HDR_AUTH_SIG absent → ts_str = "" → parse fails → 401
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing ts/sig must yield 401"
    );
}

/// Non-integer timestamp → 401.
#[tokio::test]
async fn non_integer_timestamp_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(REPO_SUBMIT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, "not-a-number")
                .header(
                    HDR_AUTH_SIG,
                    "aa".repeat(64), // 128-char placeholder
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "non-integer ts must yield 401"
    );
}

// ── Hash-domain binding (#161) ────────────────────────────────────────────────

/// Submit `body` to `uri`, signing the auth frame with `sign_domain`.
///
/// `sign_domain` is deliberately independent of whatever `uri` carries so a test
/// can express the mismatch an on-path attacker creates: the signature says one
/// thing, the query string says another.
async fn submit_signing_domain(
    router: axum::Router,
    acct: &Account,
    sign_domain: Option<HashDomain>,
    uri: &str,
    body: Vec<u8>,
) -> StatusCode {
    let ts = unix_now();
    let sig = acct.sign_auth("POST", REPO_SUBMIT, sign_domain, ts, &body);
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(HDR_AUTH_KEY, acct.public_hex())
                .header(HDR_AUTH_TS, ts.to_string())
                .header(HDR_AUTH_SIG, sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The defect in #161: before v7 the canonical frame covered `uri.path()` only,
/// so an on-path attacker could append `?domain=` to a validly-signed submit and
/// steer it at a different hash domain with the signature still verifying.
/// Appending the *native* domain is the case that stayed a 200 pre-fix (a
/// non-native one hit the domain gate), so it is the one that proves the frame
/// now binds the parameter.
#[tokio::test]
async fn appended_domain_param_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status =
        submit_signing_domain(router, &acct, None, "/repo/submit?domain=blake3", body).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "?domain= appended to a request signed without one must yield 401"
    );
}

/// Control for the above: the identical signature on the bare path succeeds, so
/// the 401 is attributable to the appended parameter and nothing else.
#[tokio::test]
async fn unqualified_submit_still_succeeds() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = submit_signing_domain(router, &acct, None, REPO_SUBMIT, body).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a submit with no ?domain= must still apply"
    );
}

/// The mirror attack: strip a `?domain=` the signer did send.
#[tokio::test]
async fn stripped_domain_param_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status =
        submit_signing_domain(router, &acct, Some(HashDomain::Blake3), REPO_SUBMIT, body).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "stripping a signed ?domain= must yield 401"
    );
}

/// A `?domain=` that matches the signature is honoured — the binding rejects
/// mismatches, it does not reject the parameter itself.
#[tokio::test]
async fn matching_domain_param_is_accepted() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = submit_signing_domain(
        router,
        &acct,
        Some(HashDomain::Blake3),
        "/repo/submit?domain=blake3",
        body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a ?domain= matching the signed frame must apply"
    );
}

/// Case and surrounding whitespace canonicalize the same way on both sides, so
/// `?domain=%20BLAKE3%20` verifies against a frame signed with `Some(Blake3)`.
/// Without this, the server's lenient `resolve_domain` and the auth frame would
/// disagree and a legitimate client would get a mystifying 401.
#[tokio::test]
async fn domain_param_canonicalizes_before_binding() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = submit_signing_domain(
        router,
        &acct,
        Some(HashDomain::Blake3),
        "/repo/submit?domain=%20BLAKE3%20",
        body,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "domain= must canonicalize identically for resolution and for the auth frame"
    );
}

/// A blank `domain=` canonicalizes to "no domain requested" on both sides, the
/// same way `resolve_domain` treats it — so it verifies against a frame signed
/// with `None`.
#[tokio::test]
async fn blank_domain_param_binds_as_absent() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = submit_signing_domain(router, &acct, None, "/repo/submit?domain=", body).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a blank domain= must bind as absent, matching resolve_domain"
    );
}

/// `DomainParam` is `deny_unknown_fields`: an unsigned query parameter is a 400,
/// not a silent no-op. Only `domain` is covered by the canonical frame, so any
/// other parameter reaching a handler would be attacker-controlled — failing the
/// request is what forces a future one to be folded into the frame deliberately.
#[tokio::test]
async fn unknown_query_param_is_rejected() {
    let acct = Account::generate();
    let body = make_sub_body(&acct);
    let (_, router) = fresh_app();
    let status = submit_signing_domain(router, &acct, None, "/repo/submit?bogus=1", body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unknown query parameter must be rejected, not ignored"
    );
}
