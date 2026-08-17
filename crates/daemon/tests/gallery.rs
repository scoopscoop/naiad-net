//! Gallery integration tests. HTTP cases drive the axum router in process via
//! `tower::ServiceExt::oneshot`; WebSocket cases bind a real loopback socket.

use std::net::SocketAddr;

use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use naiad_daemon::{AppState, app};
use naiad_db::Db;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

/// A real, decodable PNG, generated via the `image` crate.
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 64])
    });
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

/// Build an in-memory library with two PNGs imported from a temp folder, plus a
/// temp thumbnails dir. Returns the state and the guards that must stay alive
/// (the temp dirs own the real files the server reads).
fn fixture() -> (AppState, TempDir, TempDir, Vec<u8>) {
    let pic_a = png_bytes(40, 20);
    let files =
        naiad_test_support::fixture_dir(&[("a.png", &pic_a), ("b.png", &png_bytes(10, 30))]);
    let db = Db::open_in_memory().unwrap();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64);
    (state, files, thumbs, pic_a)
}

/// Run a single GET against a fresh router and return (status, body bytes).
async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    let resp = app(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

async fn spawn_gallery_server(state: AppState) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (addr, server)
}

async fn post_json(state: &AppState, uri: &str, body: serde_json::Value) -> StatusCode {
    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn search_returns_matching_files_as_json() {
    let (state, _files, _thumbs, _a) = fixture();
    // No query -> all files.
    let (status, body) = get(&state, "/api/search?q=").await;
    assert_eq!(status, StatusCode::OK);
    let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = items.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Each item exposes hash, name, size.
    let names: Vec<&str> = arr.iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"a.png"));
    assert!(names.contains(&"b.png"));
    assert_eq!(arr[0]["hash"].as_str().unwrap().len(), 64);
    assert!(arr[0]["size"].is_number());
    assert!(arr[0]["imported_at"].is_number());
    assert!(arr[0].get("created_at").is_some());
    assert!(arr[0].get("modified_at").is_some());
    assert!(arr[0].get("mime").is_some());
}

#[tokio::test]
async fn malformed_query_is_bad_request() {
    let (state, _files, _thumbs, _a) = fixture();
    // A `*` in the namespace is invalid -> parse error -> 400. (Leading/interior
    // `*` in the subtag is a supported wildcard, so `*bad` is now valid.)
    let (status, _) = get(&state, "/api/search?q=ch*r:bad").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn namespaces_lists_ranked_nonempty_namespaces() {
    let (state, _files, _thumbs, _a) = fixture();
    let hash_a = hash_of(&state, "a.png").await;
    let hash_b = hash_of(&state, "b.png").await;
    assert_eq!(
        post_json(
            &state,
            "/api/tags/add",
            serde_json::json!({ "file": hash_a, "tags": ["artist:mika", "artist:other", "samus"] }),
        )
        .await,
        StatusCode::OK,
    );
    assert_eq!(
        post_json(
            &state,
            "/api/tags/add",
            serde_json::json!({ "file": hash_b, "tags": ["series:metroid"] }),
        )
        .await,
        StatusCode::OK,
    );

    let (status, body) = get(&state, "/api/namespaces").await;
    assert_eq!(status, StatusCode::OK);
    let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        rows,
        serde_json::json!([
            { "namespace": "artist", "tag_count": 2 },
            { "namespace": "series", "tag_count": 1 },
        ]),
    );
}

/// Fetch the hash of a fixture file via the search endpoint (avoids touching
/// the moved DB).
async fn hash_of(state: &AppState, name: &str) -> String {
    let (_, body) = get(state, "/api/search?q=").await;
    let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
    items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == name)
        .unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn hash_of_a(state: &AppState) -> String {
    hash_of(state, "a.png").await
}

/// Block (bounded) until the generation queue has at least `n` parked waiters.
async fn settled_waiters(queue: &std::sync::Arc<naiad_daemon::LifoPermits>, n: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while queue.waiting() < n {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("thumb request never reached the generation queue");
}

#[tokio::test]
async fn thumb_stream_multiplexes_multiple_cached_results() {
    let (state, _files, _thumbs, _a) = fixture();
    let hash_a = hash_of(&state, "a.png").await;
    let hash_b = hash_of(&state, "b.png").await;
    // Warm both cache entries through the preserved HTTP path.
    assert_eq!(
        get(&state, &format!("/thumb/{hash_a}")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(&state, &format!("/thumb/{hash_b}")).await.0,
        StatusCode::OK
    );

    let (addr, server) = spawn_gallery_server(state).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}{}", naiad_api::THUMB_STREAM))
            .await
            .unwrap();
    ws.send(Message::Text(format!("want {hash_a}").into()))
        .await
        .unwrap();
    ws.send(Message::Text(format!("want {hash_b}").into()))
        .await
        .unwrap();

    let mut received = std::collections::HashSet::new();
    for _ in 0..2 {
        let Message::Binary(frame) = ws.next().await.unwrap().unwrap() else {
            panic!("expected binary thumbnail frame");
        };
        assert!(frame.len() > 36);
        let mut raw = [0_u8; 32];
        raw.copy_from_slice(&frame[..32]);
        received.insert(naiad_core::Hash::from_bytes(raw).to_hex());
        assert_eq!(
            u32::from_be_bytes(frame[32..36].try_into().unwrap()) as usize,
            frame.len() - 36
        );
    }
    assert_eq!(received, std::collections::HashSet::from([hash_a, hash_b]));
    server.abort();
}

#[tokio::test]
async fn thumb_stream_ignores_client_binary_message_and_stays_synchronized() {
    let (state, _files, _thumbs, _a) = fixture();
    let hash = hash_of_a(&state).await;
    assert_eq!(
        get(&state, &format!("/thumb/{hash}")).await.0,
        StatusCode::OK
    );

    let (addr, server) = spawn_gallery_server(state).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}{}", naiad_api::THUMB_STREAM))
            .await
            .unwrap();
    ws.send(Message::Binary(vec![0xde, 0xad, 0xbe, 0xef].into()))
        .await
        .unwrap();
    let barrier = vec![0x62, 0x69, 0x6e];
    ws.send(Message::Ping(barrier.clone().into()))
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        match ws.next().await {
            Some(Ok(Message::Pong(payload))) if payload == barrier => {}
            Some(Ok(Message::Pong(payload))) => {
                panic!("received Pong with unexpected payload {payload:?}")
            }
            Some(Ok(message)) => {
                panic!("unexpected message before binary barrier: {message:?}")
            }
            Some(Err(error)) => panic!("WebSocket failed after client binary message: {error}"),
            None => panic!("WebSocket closed after client binary message"),
        }
    })
    .await
    .expect("client-binary Ping/Pong barrier timed out");

    ws.send(Message::Text(format!("want {hash}").into()))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(frame))) => break frame,
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WebSocket failed before valid result: {error}"),
                None => panic!("WebSocket closed before valid result"),
            }
        }
    })
    .await
    .expect("valid want did not complete after client binary message");
    assert!(frame.len() > 36);
    assert_eq!(
        u32::from_be_bytes(frame[32..36].try_into().unwrap()) as usize,
        frame.len() - 36
    );
    let mut raw = [0_u8; 32];
    raw.copy_from_slice(&frame[..32]);
    assert_eq!(naiad_core::Hash::from_bytes(raw).to_hex(), hash);
    server.abort();
}

#[tokio::test]
async fn thumb_stream_cancelled_queued_hash_never_generates() {
    let (state, _files, _thumbs, _a) = fixture();
    let state = state.with_thumb_concurrency(1);
    let hash = hash_of(&state, "a.png").await;
    let store = state.thumb_store().clone();
    let queue = state.thumb_permits();
    let held = queue.acquire().await;
    let (addr, server) = spawn_gallery_server(state).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}{}", naiad_api::THUMB_STREAM))
            .await
            .unwrap();
    ws.send(Message::Text(format!("want {hash}").into()))
        .await
        .unwrap();
    settled_waiters(&queue, 1).await;
    ws.send(Message::Text(format!("cancel {hash}").into()))
        .await
        .unwrap();
    // Ping/Pong is the command-order barrier: the server cannot return this
    // Pong until it has consumed the preceding cancel.
    // Race-free only because the sole permit is held: the outbound channel is empty and the select! loop is reading the socket exclusively (TCP order prevails).
    ws.send(Message::Ping(vec![0x11, 0x7c].into()))
        .await
        .unwrap();
    loop {
        if matches!(ws.next().await.unwrap().unwrap(), Message::Pong(_)) {
            break;
        }
    }
    drop(held);
    let _reacquired = tokio::time::timeout(Duration::from_secs(2), queue.acquire())
        .await
        .expect("cancelled producer did not release its generation permit");
    assert!(
        store.get(&hash, 64).is_none(),
        "cancelled queued thumbnail was generated"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws.next())
            .await
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn cold_fling_cancels_backlog_and_serves_visible_set() {
    let files = tempfile::tempdir().unwrap();
    for index in 0..32_u32 {
        std::fs::write(
            files.path().join(format!("{index:02}.png")),
            png_bytes(8 + index, 8),
        )
        .unwrap();
    }
    let (db, _db_dir) = naiad_test_support::temp_db();
    naiad_daemon::import_path(&db, files.path(), |_| {}).unwrap();
    let thumbs = tempfile::tempdir().unwrap();
    let state = AppState::new(db, naiad_test_support::test_thumb_store(&thumbs), 64)
        .with_thumb_concurrency(1);

    let (_, body) = get(&state, "/api/search?q=").await;
    let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let by_name: std::collections::HashMap<_, _> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["name"].as_str().unwrap().to_owned(),
                item["hash"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let hashes: Vec<String> = (0..32)
        .map(|index| by_name[&format!("{index:02}.png")].clone())
        .collect();

    let queue = state.thumb_permits();
    let store = state.thumb_store().clone();
    let held = queue.acquire().await;
    let (addr, server) = spawn_gallery_server(state).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}{}", naiad_api::THUMB_STREAM))
            .await
            .unwrap();

    for hash in &hashes[..28] {
        ws.send(Message::Text(format!("want {hash}").into()))
            .await
            .unwrap();
    }
    settled_waiters(&queue, 28).await;

    for hash in &hashes[..28] {
        ws.send(Message::Text(format!("cancel {hash}").into()))
            .await
            .unwrap();
    }
    let barrier_payload = vec![0x11, 0x17];
    // Ping/Pong barrier — race-free only because the sole permit is held: the outbound channel is empty and the select! loop is reading the socket exclusively (TCP order prevails).
    ws.send(Message::Ping(barrier_payload.clone().into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Pong(payload))) if payload == barrier_payload => break,
                Some(Ok(Message::Pong(payload))) => {
                    panic!("received Pong with unexpected payload {payload:?}")
                }
                Some(Ok(Message::Binary(_))) => {
                    panic!("thumbnail arrived before cancellation barrier")
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WebSocket failed before cancellation barrier: {error}"),
                None => panic!("WebSocket closed before cancellation barrier"),
            }
        }
    })
    .await
    .expect("cancel Ping/Pong barrier timed out");

    for hash in &hashes[28..] {
        ws.send(Message::Text(format!("want {hash}").into()))
            .await
            .unwrap();
    }
    settled_waiters(&queue, 32).await;
    let receive_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    drop(held);

    let visible: std::collections::HashSet<_> = hashes[28..].iter().cloned().collect();
    let mut received = std::collections::HashSet::new();
    while let Ok(message) = tokio::time::timeout_at(receive_deadline, ws.next()).await {
        let Some(message) = message else {
            panic!("thumbnail stream closed during the five-second receive interval");
        };
        let Message::Binary(frame) = message.unwrap() else {
            continue;
        };
        assert!(
            received.len() < 4,
            "received a fifth binary thumbnail frame"
        );
        assert!(frame.len() > 36, "visible thumbnail frame was empty");
        assert_eq!(
            u32::from_be_bytes(frame[32..36].try_into().unwrap()) as usize,
            frame.len() - 36
        );
        let mut raw = [0_u8; 32];
        raw.copy_from_slice(&frame[..32]);
        let hash = naiad_core::Hash::from_bytes(raw).to_hex();
        assert!(visible.contains(&hash), "received cancelled hash {hash}");
        assert!(received.insert(hash), "received a duplicate visible frame");
    }
    assert_eq!(received, visible);
    tokio::time::timeout(Duration::from_secs(2), async {
        while queue.waiting() != 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("cancelled thumbnail backlog did not drain");

    for hash in &hashes[..28] {
        assert!(
            store.get(hash, 64).is_none(),
            "cancelled thumbnail cache exists for {hash}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn thumb_stream_cancel_during_active_decode_suppresses_delivery() {
    let (state, files, _thumbs, _a) = fixture();
    // Keep decode active long enough to order a cancellation after admission.
    std::fs::write(files.path().join("a.png"), png_bytes(3072, 3072)).unwrap();
    let state = state.with_thumb_concurrency(1);
    let hash = hash_of(&state, "a.png").await;
    let store = state.thumb_store().clone();
    let queue = state.thumb_permits();
    let held = queue.acquire().await;

    let (addr, server) = spawn_gallery_server(state).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}{}", naiad_api::THUMB_STREAM))
            .await
            .unwrap();
    ws.send(Message::Text(format!("want {hash}").into()))
        .await
        .unwrap();
    settled_waiters(&queue, 1).await;

    // Hand the sole permit directly to the producer, then prove it owns the
    // lane by parking a reacquire behind it.
    drop(held);
    let probe_queue = queue.clone();
    let reacquire = tokio::spawn(async move { probe_queue.acquire().await });
    settled_waiters(&queue, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        store.get(&hash, 64).is_none(),
        "active-decode fixture completed too quickly"
    );

    ws.send(Message::Text(format!("cancel {hash}").into()))
        .await
        .unwrap();
    // Ping/Pong barrier — race-free only because the sole permit is held: the outbound channel is empty and the select! loop is reading the socket exclusively (TCP order prevails).
    ws.send(Message::Ping(vec![0x4d, 0x91].into()))
        .await
        .unwrap();
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Pong(_) => break,
            Message::Binary(_) => panic!("thumbnail arrived before cancellation barrier"),
            _ => {}
        }
    }

    // Store entry proves decode ran to completion after admission. Permit
    // reacquisition proves the blocking job and its decode permit are done.
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.get_async(&hash, 64).await.is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("active decode did not produce its allowed cache artifact");
    let _reacquired = tokio::time::timeout(Duration::from_secs(5), reacquire)
        .await
        .expect("active decode did not release its generation permit")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), ws.next())
            .await
            .is_err(),
        "cancelled active decode delivered a thumbnail frame"
    );
    server.abort();
}

/// Characterisation test for the connection teardown loop.
///
/// When a client closes the WebSocket cleanly the server's `connection()`
/// function exits its `select!` loop via the `Message::Close` branch and
/// immediately marks every in-flight job unwanted (the teardown loop on
/// lines 132-134 of thumb_stream.rs). The produce task's post-admission
/// `continue_if_wanted` checkpoint then returns false and skips generation,
/// so no cache file is created for the hash.
///
/// Mutation check: commenting out the teardown loop causes this test to FAIL
/// because the produce task sees `wanted = true` and generates the thumbnail.
#[tokio::test]
async fn thumb_stream_client_close_marks_jobs_unwanted() {
    let (state, _files, _thumbs, _a) = fixture();
    let state = state.with_thumb_concurrency(1);
    let hash = hash_of(&state, "a.png").await;
    let store = state.thumb_store().clone();
    let queue = state.thumb_permits();
    let held = queue.acquire().await;
    let (addr, server) = spawn_gallery_server(state).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}{}", naiad_api::THUMB_STREAM))
            .await
            .unwrap();
    ws.send(Message::Text(format!("want {hash}").into()))
        .await
        .unwrap();
    settled_waiters(&queue, 1).await;

    // Close the WebSocket cleanly — exercises Message::Close and the teardown
    // loop that marks every job unwanted before the connection task exits.
    ws.send(Message::Close(None)).await.unwrap();
    // Drain until the stream closes. The server sends its Close response only
    // after the teardown loop completes (it is queued in tungstenite and
    // flushed when the socket is dropped), so receiving it guarantees
    // job.wanted is already false.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;

    // Release the permit so the parked produce task can acquire it.
    drop(held);

    // Re-acquiring confirms the produce task ran and returned the permit.
    let _reacquired = tokio::time::timeout(Duration::from_secs(2), queue.acquire())
        .await
        .expect("teardown-marked producer did not release its generation permit");

    // Teardown set job.wanted = false before the permit was released, so
    // continue_if_wanted returned false and generation was skipped.
    assert!(
        store.get(&hash, 64).is_none(),
        "teardown should have marked the job unwanted, preventing cache creation"
    );
    server.abort();
}

#[tokio::test]
async fn thumb_generates_caches_and_serves() {
    let (state, _files, _thumbs, _a) = fixture();
    let hash = hash_of_a(&state).await;

    // First request generates the thumbnail.
    let (status, body) = get(&state, &format!("/thumb/{hash}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..2], &[0xFF, 0xD8]); // JPEG SOI

    // It was stored in the SQLite thumbnail cache.
    assert!(
        state.thumb_store().get(&hash, 64).is_some(),
        "thumbnail should be cached in the store"
    );

    // Second request serves the cached bytes (identical content).
    let (status2, body2) = get(&state, &format!("/thumb/{hash}")).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body, body2);
}

#[tokio::test]
async fn tag_completion_returns_while_thumbnail_lane_is_saturated() {
    let (state, _files, _thumbs, _a) = fixture();
    let state = state.with_thumb_concurrency(0);
    let hash = hash_of_a(&state).await;

    let pending_thumb_state = state.clone();
    let pending_thumb = tokio::spawn(async move {
        app(pending_thumb_state)
            .oneshot(
                Request::builder()
                    .uri(format!("/thumb/{hash}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let complete = tokio::time::timeout(
        Duration::from_millis(200),
        get(&state, "/api/tags/complete?q=artist:a"),
    )
    .await
    .expect("tag completion should not wait behind saturated thumbnails");
    assert_eq!(complete.0, StatusCode::OK);

    pending_thumb.abort();
}

#[tokio::test]
async fn cached_thumb_serves_without_generation_permit() {
    let (state, _files, _thumbs, _a) = fixture();
    let hash = hash_of_a(&state).await;

    // Generate and cache the thumbnail with the normal permit budget.
    let (status, body) = get(&state, &format!("/thumb/{hash}")).await;
    assert_eq!(status, StatusCode::OK);

    // With the generation lane fully saturated (zero permits), the cached
    // thumbnail must still be served: cache hits may not queue for a permit.
    let state = state.with_thumb_concurrency(0);
    let (status2, body2) = tokio::time::timeout(
        Duration::from_millis(500),
        get(&state, &format!("/thumb/{hash}")),
    )
    .await
    .expect("cached thumbnail should not wait for a generation permit");
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);
}

#[tokio::test]
async fn saturated_queue_admits_newest_thumb_request_first() {
    let (state, _files, _thumbs, _a) = fixture();
    let state = state.with_thumb_concurrency(1);
    let hash_a = hash_of(&state, "a.png").await;
    let hash_b = hash_of(&state, "b.png").await;

    // Occupy the only generation slot so both requests must queue.
    let queue = state.thumb_permits();
    let held = queue.acquire().await;

    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    // "stale" arrives first (a tile flung past), "visible" arrives second.
    for (label, hash) in [("stale", hash_a), ("visible", hash_b)] {
        let state = state.clone();
        let done = done_tx.clone();
        let parked = queue.waiting();
        tokio::spawn(async move {
            let (status, _body) = get(&state, &format!("/thumb/{hash}")).await;
            done.send((label, status)).unwrap();
        });
        settled_waiters(&queue, parked + 1).await;
    }

    drop(held);
    // Newest-first: the later arrival is served before the backlog...
    let (first, status) = tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
        .await
        .expect("queued thumb request never completed")
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, "visible");
    // ...and the backlog still completes with leftover capacity.
    let (second, status) = tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
        .await
        .expect("stale thumb request never completed")
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second, "stale");
}

#[tokio::test]
async fn dropped_queued_thumb_request_does_not_stall_the_queue() {
    let (state, _files, _thumbs, _a) = fixture();
    let state = state.with_thumb_concurrency(1);
    let hash_a = hash_of(&state, "a.png").await;
    let hash_b = hash_of(&state, "b.png").await;

    let queue = state.thumb_permits();
    let held = queue.acquire().await;

    // A queued request whose client goes away mid-wait.
    let doomed_state = state.clone();
    let doomed = tokio::spawn(async move {
        get(&doomed_state, &format!("/thumb/{hash_a}")).await;
    });
    settled_waiters(&queue, 1).await;
    doomed.abort();
    let _ = doomed.await;

    // A live request queued on top of the abandoned entry.
    let live_state = state.clone();
    let live = tokio::spawn(async move { get(&live_state, &format!("/thumb/{hash_b}")).await });
    settled_waiters(&queue, 2).await;

    drop(held);
    // The live request is served despite the dead entry ahead of it in the
    // hand-off path...
    let (status, _body) = tokio::time::timeout(Duration::from_secs(5), live)
        .await
        .expect("live thumb request starved behind a dropped one")
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    // ...and the abandoned waiter's slot returns to the pool.
    tokio::time::timeout(Duration::from_secs(2), queue.acquire())
        .await
        .expect("generation permit leaked through the dropped request");
}

#[tokio::test]
async fn thumb_unknown_hash_is_not_found() {
    let (state, _files, _thumbs, _a) = fixture();
    let unknown = "0".repeat(64);
    let (status, _) = get(&state, &format!("/thumb/{unknown}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn file_serves_original_bytes() {
    let (state, _files, _thumbs, pic_a) = fixture();
    let hash = hash_of_a(&state).await;
    let (status, body) = get(&state, &format!("/file/{hash}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, pic_a); // exact original bytes
}

#[tokio::test]
async fn file_unknown_hash_is_not_found() {
    let (state, _files, _thumbs, _a) = fixture();
    let unknown = "0".repeat(64);
    let (status, _) = get(&state, &format!("/file/{unknown}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn index_serves_embedded_ui() {
    let (state, _files, _thumbs, _a) = fixture();
    let (status, body) = get(&state, "/").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    // The default `/` serves the bundled Svelte UI (real build or the build.rs
    // placeholder) — both mount into `<div id="app">`.
    assert!(html.contains("id=\"app\""));
}
