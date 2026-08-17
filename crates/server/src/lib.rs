//! `naiad-server` — a tag repository node (simple client/server model,
//! spec §3, ADR 0021). Storage is in `store`; the HTTP surface is in `http`.

pub mod bridge;
pub mod domain;
mod http;
pub mod settings;
mod stats;
mod store;

pub use domain::{DomainConfig, Sha256Backend, SidecarBackend, SnapshotBackend};
pub use http::{app, app_domains, app_domains_budget, app_split, app_with_bucket_budget};
pub use naiad_netproto::HashDomain;
pub use store::{AccountRow, BulkApplyStats, InternCaches, RepoStore, Result, SeedSummary, advise};

// Public stats API: used by the `naiad-repo` binary (main.rs).
// The `stats` module itself is private; these are its externally-visible surface.
pub use stats::{StatsHandle, spawn_stats};

// Re-exports for stats integration tests. The stats module is private;
// these are the minimal surface needed by `tests/stats_http.rs`.
#[doc(hidden)]
pub use stats::http::{StatsHttpState, app as stats_http_app};
#[doc(hidden)]
pub use stats::store::StatsDb;

use stats::middleware::StatsLayer;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Hard drain-cap used by [`serve`]: maximum time in-flight requests are
/// allowed to finish after a shutdown signal before the server forces exit.
pub const DEFAULT_DRAIN_CAP: Duration = Duration::from_secs(8);

/// Bind `addr` and serve the repository until an OS shutdown signal is received.
///
/// On Unix, resolves on SIGTERM **or** Ctrl-C (whichever fires first).
/// On other platforms, resolves on Ctrl-C only.  After the signal fires,
/// in-flight requests drain for up to 8 s; if the drain has not finished by
/// then the function returns anyway.
///
/// # Errors
/// Returns an error if the address cannot be bound or the server fails.
pub async fn serve(
    store: RepoStore,
    read_store: Option<RepoStore>,
    addr: SocketAddr,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    hash_domain: HashDomain,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!("naiad-repo listening on http://{bound}");
    serve_with_shutdown(
        store,
        read_store,
        listener,
        k,
        repo_key,
        name,
        hash_domain,
        os_shutdown_signal(),
        DEFAULT_DRAIN_CAP,
    )
    .await
}

/// Single-file internal helper: convert `Option<RepoStore>` to `Vec<RepoStore>`
/// for passing to `serve_with_shutdown_domains`. A `None` becomes an empty vec
/// (the domain function then falls back to a 1-element pool over the writer);
/// `Some(s)` becomes a 1-element vec.
fn read_store_to_vec(rs: Option<RepoStore>) -> Vec<RepoStore> {
    rs.into_iter().collect()
}

/// Serve over an already-bound listener, shutting down when `shutdown`
/// resolves, serving every domain in `domains`.
///
/// Exposed as `pub` for programmatic embedders that need to supply a custom
/// shutdown trigger (tests, container runtimes, etc.).
///
/// `shutdown` is polled on a spawned task (hence `Send + 'static`).  If that
/// task exits by panicking — i.e. the watch sender is dropped unexpectedly —
/// the graceful-drain future will never resolve, so the server stays up rather
/// than silently exiting; operators must then use SIGKILL to force exit (tokio
/// installs its unix signal handler process-wide and never removes it, so a
/// second SIGTERM/SIGINT is also swallowed for the life of the process).
///
/// After the shutdown signal fires, in-flight requests drain for up to
/// `drain_cap`; if the drain has not finished by then the function returns
/// anyway, logging a warning.  The `drain_cap` parameter exists for
/// testability: tests inject short durations; `serve` uses 8 s.
///
/// `read_stores` is the pre-opened pool of read-only connections (#202). An
/// empty vec falls back to a 1-element pool over the writer; a non-empty vec
/// is wrapped in a [`ReadPool`] whose connections are assigned round-robin.
///
/// `read_only`: when `true`, write handlers return 403 (#202).
///
/// # Errors
/// Returns an error if `axum::serve` fails during the serving phase.
//
// Plumbing entry point: each argument maps to one piece of server config and
// is forwarded into the router; a config struct would add indirection without
// reducing the real parameter count.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_shutdown_domains(
    store: RepoStore,
    read_stores: Vec<RepoStore>,
    listener: tokio::net::TcpListener,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    domains: DomainConfig,
    read_only: bool,
    stats_layer: Option<StatsLayer>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain_cap: Duration,
) -> anyhow::Result<()> {
    let shared = Arc::new(Mutex::new(store));
    // Keep a second Arc so we can checkpoint after the axum router drops its clone.
    let shared_for_checkpoint = Arc::clone(&shared);
    // Build the read pool (#202): wrap each read store in Arc<Mutex<>>, then
    // assemble the pool. An empty vec falls back to a 1-element pool over the
    // writer so every call site is identical regardless of read_connections.
    let read_pool = if read_stores.is_empty() {
        Arc::new(http::ReadPool::new(vec![Arc::clone(&shared)]))
    } else {
        Arc::new(http::ReadPool::new(
            read_stores
                .into_iter()
                .map(|s| Arc::new(Mutex::new(s)))
                .collect(),
        ))
    };

    // A watch channel lets both the graceful-drain future and the cap timer
    // independently observe the shutdown signal without racing each other.
    let (tx, rx_serve) = tokio::sync::watch::channel(false);
    let rx_cap = rx_serve.clone();

    // Spawn a lightweight task that fires the watch when the shutdown signal
    // lands.  We keep the JoinHandle so we can abort it on exit, preventing a
    // resource leak when serve_with_shutdown returns before the signal fires
    // (error path or drain-cap expiry with a persistent OS signal future).
    let signal_task = tokio::spawn(async move {
        shutdown.await;
        tracing::info!("shutdown signal received; draining connections");
        let _ = tx.send(true);
    });

    // The graceful drain future: axum stops accepting new connections when this
    // resolves, then waits for in-flight handlers to complete.
    // On Err the watch sender was dropped (signal task panicked); stay up rather
    // than spuriously triggering a shutdown.
    // Note: is_err() consumes the Result, dropping the watch::Ref before the
    // pending().await so the future remains Send.
    let graceful_fut = async move {
        let mut rx = rx_serve;
        if rx.wait_for(|v| *v).await.is_err() {
            tracing::error!("shutdown-signal task exited unexpectedly; staying up indefinitely");
            std::future::pending::<()>().await;
        }
    };

    // The hard cap future: waits for the same signal then sleeps for drain_cap.
    // On Err (sender dropped) behave the same way — never resolve.
    let cap_fut = async move {
        let mut rx = rx_cap;
        if rx.wait_for(|v| *v).await.is_err() {
            std::future::pending::<()>().await;
            return;
        }
        tokio::time::sleep(drain_cap).await;
        tracing::warn!(
            "graceful drain cap exceeded ({:.1}s); forcing exit",
            drain_cap.as_secs_f32()
        );
    };

    // Race: biased toward the graceful arm so that if both resolve simultaneously
    // we propagate the serve result rather than silently swallowing it.
    let serve_result = tokio::select! {
        biased;
        result = async {
            axum::serve(
                listener,
                http::app_domains_with_pool(shared, read_pool, k, repo_key, name, domains, read_only, stats_layer)
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(graceful_fut)
            .await
        } => Some(result),
        _ = cap_fut => None,
    };

    // Checkpoint the WAL on the writer store now that connections are drained.
    // In the graceful path the router (and its read-only Arc) has been dropped,
    // so no readers hold a snapshot and TRUNCATE succeeds.  In the cap path a
    // wedged handler might still hold the mutex; try_lock avoids blocking exit.
    // Checkpoint the WAL on the writer store now that connections are drained.
    // In the graceful path the router (and its read-only Arc) has been dropped,
    // so no readers hold a snapshot and TRUNCATE succeeds.  In the cap path a
    // wedged handler might still hold the mutex; try_lock avoids blocking exit.
    match shared_for_checkpoint.try_lock() {
        Ok(store) => match store.checkpoint_wal() {
            Ok(false) => tracing::info!("WAL checkpoint complete"),
            Ok(true) => tracing::warn!(
                "WAL checkpoint busy: some frames could not be copied; \
                 -wal/-shm may persist until the next clean shutdown"
            ),
            Err(e) => tracing::warn!("WAL checkpoint failed: {e:#}"),
        },
        Err(_) => tracing::warn!("WAL checkpoint skipped: writer store is locked"),
    }

    // Abort the signal task to release its OS-signal subscription (or oneshot
    // receiver), whether we exited gracefully or hit the cap.
    signal_task.abort();

    if let Some(result) = serve_result {
        result?;
    }
    Ok(())
}

/// Serve over an already-bound listener, shutting down when `shutdown`
/// resolves. Single-domain form; see [`serve_with_shutdown_domains`].
///
/// # Errors
/// Returns an error if the server fails.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_shutdown(
    store: RepoStore,
    read_store: Option<RepoStore>,
    listener: tokio::net::TcpListener,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    hash_domain: HashDomain,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain_cap: Duration,
) -> anyhow::Result<()> {
    serve_with_shutdown_domains(
        store,
        read_store_to_vec(read_store),
        listener,
        k,
        repo_key,
        name,
        DomainConfig::native_only(hash_domain),
        false,
        None,
        shutdown,
        drain_cap,
    )
    .await
}

/// Resolves when the process receives a shutdown signal.
///
/// On Unix: the first of SIGTERM or Ctrl-C.
/// On other platforms: Ctrl-C only.
///
/// Exposed as `pub` so the `naiad-repo` binary can install the signal handler
/// early (before slow store opens) and pass the resolved future to
/// [`serve_with_shutdown`].
pub async fn os_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = sigterm.recv() => {}
                    _ = async { let _ = tokio::signal::ctrl_c().await; } => {}
                }
            }
            Err(e) => {
                tracing::error!(
                    "failed to install SIGTERM handler ({e}); falling back to Ctrl-C only"
                );
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Synchronous wrapper over [`serve`]: builds a runtime and blocks.
///
/// # Errors
/// Returns an error if the runtime cannot be built or [`serve`] fails.
pub fn run(
    store: RepoStore,
    read_store: Option<RepoStore>,
    addr: SocketAddr,
    k: u64,
    repo_key: Option<String>,
    name: Option<String>,
    hash_domain: HashDomain,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(
        store,
        read_store,
        addr,
        k,
        repo_key,
        name,
        hash_domain,
    ))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `serve_with_shutdown` completes promptly after the injected shutdown
    /// future resolves.  Wrapped in a 2-second timeout to catch hangs.
    #[tokio::test]
    async fn serve_with_shutdown_completes_after_signal() {
        let store = RepoStore::open_in_memory().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        // Use a oneshot as the injected shutdown trigger.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let serve = tokio::spawn(serve_with_shutdown(
            store,
            None,
            listener,
            1000,
            None,
            None,
            HashDomain::Blake3,
            async move {
                let _ = shutdown_rx.await;
            },
            // Short drain cap: ensures the test completes well within the
            // tokio::time::timeout below even with a slightly slow CI runner.
            Duration::from_millis(200),
        ));

        // Give the server a moment to start accepting before we trigger it.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Fire the shutdown.
        let _ = shutdown_tx.send(());

        // Should complete well within 2 seconds.
        tokio::time::timeout(Duration::from_secs(2), serve)
            .await
            .expect("serve_with_shutdown timed out — graceful shutdown is stuck")
            .expect("spawned task panicked")
            .expect("serve_with_shutdown returned an error");
    }

    /// An in-flight request that arrived before the shutdown signal must be
    /// allowed to complete during the graceful drain window.
    #[tokio::test]
    async fn drain_lets_in_flight_request_complete() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let store = RepoStore::open_in_memory().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();

        let serve = tokio::spawn(serve_with_shutdown(
            store,
            None,
            listener,
            1000,
            None,
            None,
            HashDomain::Blake3,
            async move {
                let _ = sig_rx.await;
            },
            Duration::from_millis(500),
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Open a connection and send a complete request.  We then wait
        // briefly so the server's hyper task has received the headers and
        // dispatched the handler before we fire the shutdown signal.
        // Without this gap the signal can arrive before hyper has read
        // anything on the connection, causing an immediate reset rather
        // than a graceful drain.
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        conn.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        // Let hyper receive and dispatch the handler before signalling shutdown.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Fire shutdown — the in-flight handler must complete.
        let _ = sig_tx.send(());

        // Read the full response — the drain must not drop the connection mid-reply.
        let mut resp = Vec::new();
        conn.read_to_end(&mut resp).await.unwrap();
        let resp_str = std::str::from_utf8(&resp).unwrap();
        assert!(
            resp_str.starts_with("HTTP/1.1 200"),
            "in-flight request must complete with 200 OK during drain; got: {resp_str:.200}"
        );

        tokio::time::timeout(Duration::from_secs(2), serve)
            .await
            .expect("serve did not complete within 2 s after drain")
            .unwrap()
            .unwrap();
    }

    /// After the server exits, the listening port must be closed so that new
    /// TCP connections are refused.
    #[tokio::test]
    async fn no_new_connections_after_shutdown() {
        let store = RepoStore::open_in_memory().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();

        let serve = tokio::spawn(serve_with_shutdown(
            store,
            None,
            listener,
            1000,
            None,
            None,
            HashDomain::Blake3,
            async move {
                let _ = sig_rx.await;
            },
            Duration::from_millis(200),
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = sig_tx.send(());

        // Wait for the server to fully stop.
        tokio::time::timeout(Duration::from_secs(2), serve)
            .await
            .expect("serve did not stop within 2 s")
            .unwrap()
            .unwrap();

        // After the server stops, the port must be released.
        let result = tokio::net::TcpStream::connect(addr).await;
        assert!(
            result.is_err(),
            "new connections must fail after server stops"
        );
    }

    /// A wedged client must not hold up exit beyond `drain_cap`.
    ///
    /// The scenario: send complete HTTP headers announcing a body that never
    /// arrives.  axum's `Bytes` extractor blocks reading the body, so the
    /// handler IS dispatched (headers complete → hyper calls the service) but
    /// cannot complete (body never finishes → extractor never returns).  This
    /// keeps the connection in-flight through the drain window.
    ///
    /// An incomplete-header scenario (no terminal `\r\n`) does NOT exercise the
    /// cap path on all platforms: hyper closes connections with no dispatched
    /// handler immediately during graceful shutdown.  The body-withholding
    /// approach reliably exercises the cap path everywhere.
    #[tokio::test]
    async fn wedged_client_bounded_by_drain_cap() {
        use tokio::io::AsyncWriteExt;

        let store = RepoStore::open_in_memory().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();

        const DRAIN_CAP_MS: u64 = 300;

        let serve = tokio::spawn(serve_with_shutdown(
            store,
            None,
            listener,
            1000,
            None,
            None,
            HashDomain::Blake3,
            async move {
                let _ = sig_rx.await;
            },
            Duration::from_millis(DRAIN_CAP_MS),
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Send complete HTTP headers with a Content-Length of 1024 but no body.
        // hyper dispatches the handler (headers are complete), but axum's Bytes
        // extractor blocks trying to read 1024 bytes that never arrive.  The
        // handler is in-flight for the entire graceful drain window.
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        conn.write_all(
            b"POST /repo/submit HTTP/1.1\r\n\
              Host: localhost\r\n\
              Content-Type: application/json\r\n\
              Content-Length: 1024\r\n\
              \r\n",
        )
        .await
        .unwrap();

        // Give hyper time to receive headers and dispatch the handler.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Measure from the moment shutdown fires.
        let shutdown_at = std::time::Instant::now();

        // Fire shutdown — the in-flight handler keeps axum's drain alive.
        let _ = sig_tx.send(());

        // The server must exit within drain_cap + a generous buffer, not hang.
        tokio::time::timeout(Duration::from_millis(DRAIN_CAP_MS + 1000), serve)
            .await
            .expect(
                "serve did not complete within drain_cap + buffer — wedged client held exit open",
            )
            .unwrap()
            .unwrap();

        // Must have waited AT LEAST drain_cap — proves the cap path fired rather
        // than the server returning instantly on an early connection drop.
        let elapsed_ms = shutdown_at.elapsed().as_millis() as u64;
        assert!(
            elapsed_ms >= DRAIN_CAP_MS,
            "serve returned in {elapsed_ms}ms < drain_cap ({DRAIN_CAP_MS}ms): \
             cap path was not exercised"
        );

        drop(conn);
    }

    /// A clean shutdown must checkpoint the WAL so that `repo.db-wal` is
    /// truncated to zero bytes.
    ///
    /// The key fixture: *the test itself* holds an open read-only connection
    /// that is NOT handed to the server.  This reader keeps the WAL file alive
    /// after all server connections close (preventing OS-level deletion), and
    /// because it holds an implicit WAL snapshot it prevents the default
    /// passive checkpoint that fires on writer-close from truncating the file.
    /// Without our explicit `PRAGMA wal_checkpoint(TRUNCATE)` call the WAL
    /// file would still exist and contain non-zero bytes.  With it, the file
    /// exists but is empty.
    #[tokio::test]
    async fn clean_shutdown_checkpoints_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("repo.db");
        let wal_path = dir.path().join("repo.db-wal");

        // Writer: runs migrations, populating the WAL.
        let store = RepoStore::open(&db_path).unwrap();

        // Witness reader: held by the TEST across the shutdown.  It keeps the
        // WAL file present after the server's connections close and ensures the
        // passive checkpoint on writer-close cannot truncate the file.
        // NOT passed to serve_with_shutdown — the server gets its own copy.
        let _witness_reader = RepoStore::open_readonly(&db_path).unwrap();

        // Server read-only: normal production configuration.
        let server_read = RepoStore::open_readonly(&db_path).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();

        let serve = tokio::spawn(serve_with_shutdown(
            store,
            Some(server_read),
            listener,
            1000,
            None,
            None,
            HashDomain::Blake3,
            async move {
                let _ = sig_rx.await;
            },
            Duration::from_millis(200),
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Signal a clean shutdown (no requests in flight).
        let _ = sig_tx.send(());

        tokio::time::timeout(Duration::from_secs(2), serve)
            .await
            .expect("serve did not complete within 2 s")
            .unwrap()
            .unwrap();

        // The witness reader keeps the WAL file alive.  PRAGMA TRUNCATE must
        // have set it to zero bytes.  If checkpoint_wal() were removed, the
        // passive checkpoint that fires on writer-close does NOT truncate, so
        // the file would be non-zero.
        let wal_meta = std::fs::metadata(&wal_path)
            .expect("WAL file must still exist — witness reader holds it open");
        assert_eq!(
            wal_meta.len(),
            0,
            "WAL must be truncated to zero by clean shutdown; \
             non-zero means checkpoint_wal() was not called or returned busy"
        );

        // Schema survives the checkpoint.
        drop(_witness_reader);
        RepoStore::open(&db_path).expect("fresh open after checkpoint must succeed");
    }
}
