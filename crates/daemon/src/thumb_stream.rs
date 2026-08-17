use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use naiad_core::Hash;
use tokio::sync::{Mutex, mpsc};

use crate::AppState;
use crate::server::{present_location, thumb_bytes};

pub(crate) const FRAME_HEADER_LEN: usize = 36;
pub(crate) const MAX_JOBS: usize = 512;
pub(crate) const OUTBOUND_CAPACITY: usize = 64;

#[derive(Debug)]
struct JobState {
    wanted: bool,
}

type Jobs = Arc<Mutex<HashMap<Hash, JobState>>>;

struct Completed {
    hash: Hash,
    jpeg: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThumbCommand {
    Want(Hash),
    Cancel(Hash),
}

pub(crate) fn parse_command(text: &str) -> Result<ThumbCommand, &'static str> {
    let (verb, raw) = text.split_once(' ').ok_or("missing command separator")?;
    if raw.len() != 64 || raw.bytes().any(|b| b.is_ascii_uppercase() || b == b' ') {
        return Err("hash must be 64 lowercase hex characters");
    }
    let hash: Hash = raw.parse().map_err(|_| "invalid hash")?;
    match verb {
        "want" => Ok(ThumbCommand::Want(hash)),
        "cancel" => Ok(ThumbCommand::Cancel(hash)),
        _ => Err("unsupported command"),
    }
}

// Extracted so the overflow path is testable without allocating > 4 GiB.
fn body_len_u32(len: usize) -> Option<u32> {
    u32::try_from(len).ok()
}

pub(crate) fn encode_frame(hash: Hash, jpeg: Option<&[u8]>) -> Vec<u8> {
    let body = jpeg.unwrap_or_default();
    let len = match body_len_u32(body.len()) {
        Some(n) => n,
        None => {
            tracing::warn!(
                target: "thumb",
                body_len = body.len(),
                "thumbnail body exceeds u32 frame length; emitting failure frame"
            );
            // Zero-length frame is the per-item failure contract; jpeg=None
            // gives an empty body whose length always fits in u32.
            return encode_frame(hash, None);
        }
    };
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
    frame.extend_from_slice(hash.as_bytes());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

pub(crate) async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| connection(socket, state))
        .into_response()
}

async fn connection(mut socket: WebSocket, state: AppState) {
    let jobs: Jobs = Arc::new(Mutex::new(HashMap::new()));
    let (outbound_tx, mut outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let mut invalid_warnings = 0_u8;

    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => match parse_command(&text) {
                        Ok(ThumbCommand::Want(hash)) => {
                            if record_want(&jobs, hash).await {
                                tokio::spawn(produce(
                                    hash,
                                    jobs.clone(),
                                    state.clone(),
                                    outbound_tx.clone(),
                                ));
                            }
                        }
                        Ok(ThumbCommand::Cancel(hash)) => {
                            record_cancel(&jobs, hash).await;
                        }
                        Err(error) => {
                            warn_invalid_message(&mut invalid_warnings, error, text.len());
                        }
                    },
                    Some(Ok(Message::Binary(bytes))) => {
                        warn_invalid_message(
                            &mut invalid_warnings,
                            "client binary messages are unsupported",
                            bytes.len(),
                        );
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if let Err(error) = socket.send(Message::Pong(payload)).await {
                            tracing::warn!(target: "thumb", %error, "thumbnail stream Pong send failed");
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        tracing::debug!(target: "thumb", "thumbnail stream closed by client");
                        break;
                    }
                    Some(Err(error)) => {
                        tracing::warn!(target: "thumb", %error, "thumbnail stream receive failed");
                        break;
                    }
                    None => {
                        tracing::debug!(target: "thumb", "thumbnail stream client disconnected");
                        break;
                    }
                }
            }
            completed = outbound_rx.recv() => {
                let Some(completed) = completed else {
                    break;
                };
                let frame = encode_frame(completed.hash, completed.jpeg.as_deref());
                if let Err(error) = socket.send(Message::Binary(frame.into())).await {
                    tracing::warn!(target: "thumb", %error, "thumbnail stream result send failed");
                    break;
                }
            }
        }
    }

    for job in jobs.lock().await.values_mut() {
        job.wanted = false;
    }
}

async fn record_want(jobs: &Jobs, hash: Hash) -> bool {
    let mut jobs = jobs.lock().await;
    if let Some(job) = jobs.get_mut(&hash) {
        job.wanted = true;
        false
    } else if jobs.len() >= MAX_JOBS {
        let hash_hex = hash.to_hex();
        tracing::warn!(
            target: "thumb",
            hash = &hash_hex[..12],
            active_jobs = jobs.len(),
            max_jobs = MAX_JOBS,
            "thumbnail stream want rejected at job limit"
        );
        false
    } else {
        jobs.insert(hash, JobState { wanted: true });
        true
    }
}

async fn record_cancel(jobs: &Jobs, hash: Hash) {
    if let Some(job) = jobs.lock().await.get_mut(&hash) {
        job.wanted = false;
        let hash_hex = hash.to_hex();
        tracing::debug!(
            target: "thumb",
            hash = &hash_hex[..12],
            "thumbnail stream job cancelled"
        );
    }
}

fn warn_invalid_message(warnings: &mut u8, error: &str, message_len: usize) {
    if *warnings >= 5 {
        return;
    }
    *warnings += 1;
    tracing::warn!(
        target: "thumb",
        error,
        message_len,
        "invalid thumbnail stream message"
    );
}

async fn continue_if_wanted(jobs: &Jobs, hash: Hash) -> bool {
    let mut jobs = jobs.lock().await;
    match jobs.get(&hash) {
        Some(job) if job.wanted => true,
        Some(_) => {
            jobs.remove(&hash);
            let hash_hex = hash.to_hex();
            tracing::debug!(
                target: "thumb",
                hash = &hash_hex[..12],
                "cancelled thumbnail stream job skipped before decode"
            );
            false
        }
        None => false,
    }
}

async fn finish_if_wanted(jobs: &Jobs, hash: Hash) -> bool {
    let wanted = jobs
        .lock()
        .await
        .remove(&hash)
        .is_some_and(|job| job.wanted);
    if !wanted {
        let hash_hex = hash.to_hex();
        tracing::debug!(
            target: "thumb",
            hash = &hash_hex[..12],
            "stale thumbnail stream completion suppressed"
        );
    }
    wanted
}

async fn produce(hash: Hash, jobs: Jobs, state: AppState, outbound: mpsc::Sender<Completed>) {
    let hash_hex = hash.to_hex();
    if let Some(jpeg) = state
        .thumb_store
        .get_async(&hash_hex, state.thumb_size)
        .await
    {
        if finish_if_wanted(&jobs, hash).await {
            let _ = outbound
                .send(Completed {
                    hash,
                    jpeg: Some(jpeg),
                })
                .await;
        }
        return;
    }

    let permit = state.thumb_permits.acquire().await;
    if !continue_if_wanted(&jobs, hash).await {
        return;
    }

    let path = match present_location(&state, hash).await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                target: "thumb",
                hash = &hash_hex[..12],
                "thumbnail stream location lookup failed: {}",
                error.1
            );
            drop(permit);
            if finish_if_wanted(&jobs, hash).await {
                let _ = outbound.send(Completed { hash, jpeg: None }).await;
            }
            return;
        }
    };

    if !continue_if_wanted(&jobs, hash).await {
        return;
    }

    let store = state.thumb_store.clone();
    let size = state.thumb_size;
    let hash_for_decode = hash_hex.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        thumb_bytes(&path, &store, size, &hash_for_decode)
    })
    .await;
    let jpeg = match result {
        Ok(Ok(jpeg)) => Some(jpeg),
        Ok(Err(error)) => {
            tracing::warn!(
                target: "thumb",
                hash = &hash_hex[..12],
                "thumbnail generation failed: {error:#}"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                target: "thumb",
                hash = &hash_hex[..12],
                "thumbnail task panicked: {error}"
            );
            None
        }
    };

    if finish_if_wanted(&jobs, hash).await {
        let _ = outbound.send(Completed { hash, jpeg }).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

    struct LogWriter(LogBuffer);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
        type Writer = LogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            LogWriter(self.clone())
        }
    }

    impl LogBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        fn subscriber(&self) -> impl tracing::Subscriber {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(self.clone())
                .finish()
        }
    }

    fn log_line<'a>(logs: &'a str, message: &str) -> &'a str {
        logs.lines()
            .find(|line| line.contains(message))
            .unwrap_or_else(|| panic!("missing log {message:?} in {logs:?}"))
    }

    const HASH_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn parses_exact_want_and_cancel_commands() {
        let hash: Hash = HASH_HEX.parse().unwrap();
        assert_eq!(
            parse_command(&format!("want {HASH_HEX}")),
            Ok(ThumbCommand::Want(hash))
        );
        assert_eq!(
            parse_command(&format!("cancel {HASH_HEX}")),
            Ok(ThumbCommand::Cancel(hash))
        );
    }

    #[test]
    fn rejects_noncanonical_or_malformed_commands() {
        for bad in [
            "want",
            "want  abc",
            "want abc",
            "want 00010203 extra",
            "Want 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "want 000102030405060708090A0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "drop 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ] {
            assert!(parse_command(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn body_len_u32_accepts_valid_sizes_and_rejects_overflow() {
        assert_eq!(body_len_u32(0), Some(0));
        assert_eq!(body_len_u32(u32::MAX as usize), Some(u32::MAX));
        // Only exercisable on 64-bit targets where usize can exceed u32::MAX.
        #[cfg(target_pointer_width = "64")]
        assert!(body_len_u32(u32::MAX as usize + 1).is_none());
    }

    #[test]
    fn success_frame_is_raw_hash_big_endian_length_then_jpeg() {
        let hash: Hash = HASH_HEX.parse().unwrap();
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let frame = encode_frame(hash, Some(&jpeg));
        assert_eq!(&frame[..32], hash.as_bytes());
        assert_eq!(&frame[32..36], &[0, 0, 0, 4]);
        assert_eq!(&frame[36..], &jpeg);
    }

    #[test]
    fn failure_frame_has_zero_length_and_no_body() {
        let hash: Hash = HASH_HEX.parse().unwrap();
        let frame = encode_frame(hash, None);
        assert_eq!(frame.len(), 36);
        assert_eq!(&frame[32..], &[0, 0, 0, 0]);
    }

    fn numbered_hash(number: u16) -> Hash {
        let mut raw = [0_u8; 32];
        raw[..2].copy_from_slice(&number.to_be_bytes());
        Hash::from_bytes(raw)
    }

    #[tokio::test]
    async fn job_map_is_bounded_and_duplicate_want_reuses_entry() {
        let jobs: Jobs = Arc::new(Mutex::new(HashMap::new()));
        for number in 0..MAX_JOBS {
            assert!(record_want(&jobs, numbered_hash(number as u16)).await);
        }
        assert_eq!(jobs.lock().await.len(), MAX_JOBS);

        assert!(!record_want(&jobs, numbered_hash(0)).await);
        assert_eq!(jobs.lock().await.len(), MAX_JOBS);

        assert!(!record_want(&jobs, numbered_hash(MAX_JOBS as u16)).await);
        assert_eq!(jobs.lock().await.len(), MAX_JOBS);
    }

    #[tokio::test]
    async fn cancel_then_rewant_toggles_the_same_job_entry() {
        let jobs: Jobs = Arc::new(Mutex::new(HashMap::new()));
        let hash = numbered_hash(7);
        assert!(record_want(&jobs, hash).await);

        record_cancel(&jobs, hash).await;
        {
            let jobs = jobs.lock().await;
            assert_eq!(jobs.len(), 1);
            assert!(!jobs.get(&hash).unwrap().wanted);
        }

        assert!(!record_want(&jobs, hash).await);
        let jobs = jobs.lock().await;
        assert_eq!(jobs.len(), 1);
        assert!(jobs.get(&hash).unwrap().wanted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn job_lifecycle_logs_rejection_at_warn_and_cancellation_at_debug() {
        let jobs: Jobs = Arc::new(Mutex::new(HashMap::new()));
        for number in 0..MAX_JOBS {
            jobs.lock()
                .await
                .insert(numbered_hash(number as u16), JobState { wanted: true });
        }

        let buffer = LogBuffer::default();
        let subscriber = buffer.subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        assert!(!record_want(&jobs, numbered_hash(MAX_JOBS as u16)).await);

        let cancelled = numbered_hash(7);
        record_cancel(&jobs, cancelled).await;
        assert!(!continue_if_wanted(&jobs, cancelled).await);

        let stale = numbered_hash(8);
        record_cancel(&jobs, stale).await;
        assert!(!finish_if_wanted(&jobs, stale).await);
        drop(guard);

        let logs = buffer.contents();
        let rejected = log_line(&logs, "thumbnail stream want rejected at job limit");
        assert!(rejected.contains(" WARN "), "{rejected}");
        assert!(rejected.contains("active_jobs=512"), "{rejected}");
        assert!(rejected.contains("max_jobs=512"), "{rejected}");

        for message in [
            "thumbnail stream job cancelled",
            "cancelled thumbnail stream job skipped before decode",
            "stale thumbnail stream completion suppressed",
        ] {
            let line = log_line(&logs, message);
            assert!(line.contains("DEBUG"), "{line}");
        }
    }

    #[test]
    fn invalid_message_warnings_are_capped_at_five_with_context() {
        let buffer = LogBuffer::default();
        let subscriber = buffer.subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let mut warnings = 0;

        for _ in 0..6 {
            warn_invalid_message(&mut warnings, "unsupported test message", 73);
        }
        drop(guard);

        let logs = buffer.contents();
        assert_eq!(logs.matches("invalid thumbnail stream message").count(), 5);
        for line in logs
            .lines()
            .filter(|line| line.contains("invalid thumbnail stream message"))
        {
            assert!(line.contains("unsupported test message"), "{line}");
            assert!(line.contains("message_len=73"), "{line}");
        }
    }

    #[tokio::test]
    async fn outbound_capacity_is_64_and_blocked_completion_releases_generation_permit() {
        assert_eq!(OUTBOUND_CAPACITY, 64);

        let image = image::RgbImage::from_pixel(16, 16, image::Rgb([31, 47, 63]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let files = tempfile::tempdir().unwrap();
        std::fs::write(files.path().join("backpressure.png"), &png).unwrap();
        let (db, _db_dir) = naiad_test_support::temp_db();
        crate::import_path(&db, files.path(), |_| {}).unwrap();
        let thumbs = tempfile::tempdir().unwrap();
        let state = AppState::new(
            db,
            crate::thumb_store::ThumbStore::open(&thumbs.path().join("thumbs.db")).unwrap(),
            64,
        )
        .with_thumb_concurrency(1);
        let permits = state.thumb_permits();
        let hash = naiad_core::hash_bytes(&png);
        let hash_hex = hash.to_hex();
        let store = state.thumb_store.clone();
        let thumb_size = state.thumb_size;
        let jobs: Jobs = Arc::new(Mutex::new(HashMap::from([(
            hash,
            JobState { wanted: true },
        )])));
        let (outbound, mut completed) = mpsc::channel(OUTBOUND_CAPACITY);

        for number in 0..OUTBOUND_CAPACITY {
            outbound
                .try_send(Completed {
                    hash: numbered_hash(number as u16),
                    jpeg: None,
                })
                .unwrap();
        }
        assert_eq!(outbound.capacity(), 0);
        assert!(matches!(
            outbound.try_send(Completed {
                hash: numbered_hash(OUTBOUND_CAPACITY as u16),
                jpeg: None,
            }),
            Err(mpsc::error::TrySendError::Full(_))
        ));

        let producer = tokio::spawn(produce(hash, jobs, state, outbound));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.get_async(&hash_hex, thumb_size).await.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("thumbnail generation never completed");

        let reacquired = tokio::time::timeout(std::time::Duration::from_secs(2), permits.acquire())
            .await
            .expect("blocked outbound completion retained the generation permit");
        assert!(
            !producer.is_finished(),
            "producer completed despite the outbound channel remaining full"
        );

        completed.recv().await.unwrap();
        drop(reacquired);
        producer.await.unwrap();

        let mut delivered = false;
        while let Ok(item) = completed.try_recv() {
            delivered |=
                item.hash == hash && item.jpeg.as_ref().is_some_and(|jpeg| !jpeg.is_empty());
        }
        assert!(
            delivered,
            "generated completion was not delivered after capacity freed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_location_warns_before_zero_length_completion() {
        let thumbs = tempfile::tempdir().unwrap();
        let state = AppState::new(
            naiad_db::Db::open_in_memory().unwrap(),
            crate::thumb_store::ThumbStore::open(&thumbs.path().join("thumbs.db")).unwrap(),
            64,
        );
        let hash = numbered_hash(42);
        let jobs: Jobs = Arc::new(Mutex::new(HashMap::from([(
            hash,
            JobState { wanted: true },
        )])));
        let (outbound, mut completed) = mpsc::channel(1);

        let buffer = LogBuffer::default();
        let subscriber = buffer.subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        produce(hash, jobs, state, outbound).await;
        let completed = completed.recv().await.unwrap();
        drop(guard);

        assert_eq!(completed.hash, hash);
        assert!(completed.jpeg.is_none());
        let logs = buffer.contents();
        let missing = log_line(&logs, "thumbnail stream location lookup failed");
        assert!(missing.contains(" WARN "), "{missing}");
        assert!(missing.contains(&hash.to_hex()[..12]), "{missing}");
    }
}
