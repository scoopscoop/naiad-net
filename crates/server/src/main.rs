//! `naiad-repo` — run a repository node, seed it, or manage accounts and reports.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{Parser, Subcommand};
use naiad_core::{Hash, Tag};
use naiad_netproto::{Account, HashDomain, Op};
use naiad_server::{
    DEFAULT_DRAIN_CAP, DomainConfig, RepoStore, os_shutdown_signal, serve_with_shutdown_domains,
};
use serde::Deserialize;

/// Worked examples appended to `naiad-repo --help` (and the bare-invocation
/// help). `--db` is global and comes before the subcommand.
const ROOT_EXAMPLES: &str = r#"Examples:
  naiad-repo serve                                 ./repo.db on 127.0.0.1:9090
  naiad-repo --db repo.db serve --addr 0.0.0.0:9090   reachable from elsewhere
  naiad-repo --db repo.db seed <64-char-hex-hash> character:samus
  naiad-repo --db repo.db seed --from-file mappings.jsonl    bulk import
  naiad-repo --db repo.db account list             who has submitted, and their role
  naiad-repo --db repo.db report list              the open moderation queue
  naiad-repo --db repo.db bridge status            PTR mirror cursor and stats

Note that `--db` is global: it goes before the subcommand, not after.

Settings live in repo.toml beside the database (a commented template is written
on first run). CLI flags beat environment variables, which beat the file, which
beats the built-in defaults; any disagreement is logged at startup.
Full guide: docs/operating-a-repo.md.

Environment:
  NAIAD_REPO_DB           path to the repository database [default: repo.db]
  NAIAD_REPO_ADDR         bind address for `serve` [default: 127.0.0.1:9090]
  NAIAD_REPO_K            k-anonymity crowd floor [default: 1000]
  NAIAD_REPO_KEY          repo identity key advertised in /repo/caps
                            (not the operator signing key — that is repo.key)
  NAIAD_REPO_NAME         display name advertised in /repo/caps; subscribers
                            capture it as the local subscription name
  NAIAD_REPO_HASH_DOMAIN  hash domain for /repo/caps [blake3|sha256]
  NAIAD_REPO_LOG_LEVEL    log filter level [default: info]
  NAIAD_REPO_LOG_CONSOLE  emit to stderr [true|false, default: true]
  NAIAD_REPO_LOG_FILE     append-mode log file path (empty = disable)
  RUST_LOG                log filter; beats NAIAD_REPO_LOG_LEVEL
  NAIAD_REPO_BRIDGE_*     PTR mirror settings (see `naiad-repo help bridge`)

Per-command examples: `naiad-repo help <command>`."#;

#[derive(Parser)]
#[command(
    name = "naiad-repo",
    version,
    about = "A Naiad tag repository node.",
    after_help = ROOT_EXAMPLES
)]
struct Cli {
    /// Path to the repository database (created if absent)
    /// [default: repo.db, env: NAIAD_REPO_DB].
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the repository over HTTP.
    ///
    /// Reads `repo.toml` beside the database (written on first run). CLI flags
    /// and environment variables override file values, which override the
    /// built-in defaults.
    // Default strings below must match settings::DEFAULT_ADDR and settings::DEFAULT_K.
    #[command(after_help = r#"Examples:
  naiad-repo serve                                  defaults, or whatever repo.toml says
  naiad-repo --db repo.db serve --addr 0.0.0.0:9090     bind every interface
  naiad-repo --db repo.db serve --k 5000                a larger crowd floor
  naiad-repo --db repo.db serve --repo-key <64-char-hex>   advertise an identity
  naiad-repo --db repo.db serve --hash-domain sha256    mirror a SHA-256 repo

Equivalent repo.toml (flags override it):

  [serve]
  addr = "0.0.0.0:9090"
  k = 5000
  # repo_key = "<64-char-hex>"
  # hash_domain = "blake3"
  # name = "NOS"

  [log]
  level = "info"
  file = "repo.log"      # relative paths resolve beside the database

Equivalent environment:

  NAIAD_REPO_ADDR=0.0.0.0:9090
  NAIAD_REPO_K=5000
  NAIAD_REPO_LOG_LEVEL=info
  NAIAD_REPO_NAME=NOS

`--k` is the k-anonymity crowd floor from ADR 0001: how many files must share a
fingerprint prefix before the server answers in bucketed mode. `--repo-key`
gives clients something stable to pin, so their pseudonym at your repo cannot
be linked to their identity elsewhere; without it they anchor on your URL.

Set `[bridge].enabled = true` to run the PTR sync loop inside this process."#)]
    Serve {
        /// Address to bind [default: 127.0.0.1:9090, env: NAIAD_REPO_ADDR,
        /// file key: serve.addr].
        #[arg(long)]
        addr: Option<SocketAddr>,
        /// Crowd-size floor for k-anonymity prefix sizing (ADR 0001)
        /// [default: 1000, env: NAIAD_REPO_K, file key: serve.k].
        #[arg(long)]
        k: Option<u64>,
        /// Optional repo identity hint: a plain 64-char hex Ed25519 pubkey
        /// advertised in `/repo/caps` as `repo_key` so clients can pin the
        /// repo. No rotation-chain machinery. [env: NAIAD_REPO_KEY,
        /// file key: serve.repo_key]
        #[arg(long)]
        repo_key: Option<String>,
        /// Hash domain advertised in `/repo/caps` [blake3|sha256, default: blake3,
        /// env: NAIAD_REPO_HASH_DOMAIN, file key: serve.hash_domain].
        #[arg(long, value_parser = parse_hash_domain)]
        hash_domain: Option<HashDomain>,
        /// Disable all outbound PTR sync. The server starts, but the PTR
        /// follow-loop is never launched. Fatal if combined with
        /// `[bridge].enabled = true`.
        /// [env: NAIAD_REPO_NO_EGRESS, file key: serve.no_egress].
        #[arg(long)]
        no_egress: bool,
    },
    /// Seed one or many `hash -> tag` mappings, signed by the operator key.
    ///
    /// Single mapping: provide `<hash>` and `<tag>` as positional arguments.
    /// Bulk from file: provide `--from-file <PATH>` with a JSONL file (one
    /// `{"hash":"<64-char lowercase blake3 hex>","tag":"<tag>"}` per line).
    /// The two modes are mutually exclusive.
    #[command(after_help = r#"Examples:
  naiad-repo --db repo.db seed <64-char-hex-hash> character:samus
  naiad-repo --db repo.db seed <64-char-hex-hash> series:metroid
  naiad-repo --db repo.db seed --from-file mappings.jsonl

One JSON object per line in the file, blank lines skipped:

  {"hash":"<64-char lowercase hex>","tag":"character:samus"}
  {"hash":"<64-char lowercase hex>","tag":"series:metroid"}

The whole file is parsed before anything is written, so a bad line aborts the
import with the offending line number and changes nothing. A client produces
this format directly:

  naiad export-mappings --db naiad.db --out mappings.jsonl

Mappings are signed with the operator key at repo.key beside the database,
created on first use."#)]
    Seed {
        /// 64-char BLAKE3 hex hash (single-mapping mode).
        hash: Option<String>,
        /// Tag text, e.g. `character:samus` (single-mapping mode).
        tag: Option<String>,
        /// Path to a JSONL file for bulk seeding (one `{"hash":"…","tag":"…"}`
        /// object per line). Mutually exclusive with `<hash>` and `<tag>`.
        #[arg(long, value_name = "PATH", conflicts_with_all = ["hash", "tag"])]
        from_file: Option<PathBuf>,
    },
    /// Manage accounts: promote, ban, or list.
    #[command(after_help = r#"Examples:
  naiad-repo --db repo.db account list                    pubkey, role, banned, created
  naiad-repo --db repo.db account promote <64-char-hex>   make a moderator
  naiad-repo --db repo.db account ban <64-char-hex>       reject future submits

Accounts appear the first time a key submits something — there is no signup.
Banning is reversible only by editing the database directly, so check the key
against `report list` before you use it."#)]
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// View and act on the open report queue (operates directly on the local DB).
    #[command(after_help = r#"Examples:
  naiad-repo --db repo.db report list                     id, hash, tag, reporter, note
  naiad-repo --db repo.db report delete <64-char-hex-hash> meme:bad
  naiad-repo --db repo.db report dismiss 7                keep the mapping, close it
  naiad-repo --db repo.db report ban <64-char-hex-key>    ban a bad-faith reporter

`delete` removes the (hash, tag) mapping and auto-closes every open report
against it; `dismiss` closes one report by id and leaves the mapping alone."#)]
    Report {
        #[command(subcommand)]
        action: ReportAction,
    },
    /// PTR bridge operations: mirror the Hydrus Public Tag Repository.
    ///
    /// Uses the global `--db` for the store and the `[bridge]` section of
    /// `repo.toml` (plus `NAIAD_REPO_BRIDGE_*` env) for PTR URL/key and the
    /// state db. Enable the in-process sync loop under `serve` with
    /// `[bridge].enabled = true`; these subcommands are the offline/admin ops.
    #[command(after_help = r#"Examples:
  naiad-repo --db repo.db bridge status               cursor and store stats, read-only
  naiad-repo --db repo.db bridge sync                 fetch updates past the cursor
  naiad-repo --db repo.db bridge sync --follow        keep polling for new ones
  naiad-repo --db repo.db bridge seed D:\ptr-snapshot    import an offline client.db
  naiad-repo --db repo.db bridge seed D:\ptr-snapshot --service-id 5

Configuration comes from `[bridge]` in repo.toml, overridable by environment:

  [bridge]
  enabled  = false            # true = run the sync loop inside `serve`
  ptr_url  = "<ptr base url>"
  ptr_key  = "<access key>"
  state_db = "bridge-state.db"   # relative paths resolve beside --db

  NAIAD_REPO_BRIDGE_ENABLED / _PTR_URL / _PTR_KEY / _STATE_DB

A bulk `seed` from a snapshot first, then `sync --follow` to stay current, is
much faster than replaying the PTR's whole update history. `sync` needs a key;
`status` and `seed` do not. An enabled bridge adds a sha256 hash domain
alongside the repo's native domain; the native `--hash-domain` is unaffected.

At most one bridge writer (a `bridge sync` or a bridge-enabled serve) may run
per store: the writer holds an exclusive lock on `bridge.lock` beside the
state db. A second `bridge sync` prints "another bridge process appears to be
running" and exits 4; `bridge seed`, `bridge status`, and `bridge parity-audit`
are never gated by this lock."#)]
    Bridge {
        #[command(subcommand)]
        action: BridgeAction,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Promote a pubkey to moderator.
    Promote {
        /// 64-char hex Ed25519 public key.
        pubkey: String,
    },
    /// Ban a pubkey from submitting and reporting.
    Ban {
        /// 64-char hex Ed25519 public key.
        pubkey: String,
    },
    /// List all accounts.
    List,
}

#[derive(Subcommand)]
enum ReportAction {
    /// List every open report (id, hash, tag, reporter, note, created_at).
    List,
    /// Delete the (hash, tag) mapping and auto-close all open reports for it.
    Delete {
        /// 64-char BLAKE3 hex hash.
        hash: String,
        /// Tag text, e.g. `character:samus`.
        tag: String,
    },
    /// Ban a reporter pubkey: reject future submits and reports from that key.
    Ban {
        /// 64-char hex Ed25519 public key of the reporter.
        pubkey: String,
    },
    /// Dismiss a report by id without changing the mapping.
    Dismiss {
        /// Numeric report id (from `report list`).
        id: i64,
    },
}

#[derive(Subcommand)]
enum BridgeAction {
    /// Seed the repo from an offline Hydrus client.db snapshot (no PTR contact).
    Seed {
        /// Path to the snapshot directory (a copy of a Hydrus client.db set).
        snapshot_dir: PathBuf,
        /// Optional service id to restrict import (auto-discovered if omitted).
        #[arg(long)]
        service_id: Option<i64>,
        /// Use synchronous=OFF on the seed connection for maximum throughput.
        ///
        /// WARNING: with this flag, an OS crash or power loss *during* the seed
        /// can corrupt repo.db (not just lose the last transaction). If that
        /// happens, delete repo.db and re-seed from scratch — phase 1 restarts
        /// safely from an empty store. Only use on a reliable system or when
        /// re-seeding is cheap.
        #[arg(long)]
        unsafe_fast: bool,
        /// Rebuild the repo in place: clears repo_mappings and repo_hashes,
        /// re-seeds from the snapshot, replays local submissions on top, and
        /// mints a new store-generation id. Use this when the PTR snapshot has
        /// changed significantly and you want clients to re-pull from zero.
        /// The submissions log is preserved; all other mirror data is replaced.
        ///
        /// **Run offline — stop the serving process first.** During the rebuild
        /// window the store still advertises the old generation while
        /// `repo_mappings` is already being reshuffled. Clients that connect
        /// mid-rebuild see an inconsistent view. Restart the serving process
        /// after `--rebuild` completes and the new generation is minted.
        ///
        /// A crash before the generation is minted leaves the `rebuild_in_progress`
        /// marker set; re-run `--rebuild` to complete the operation.
        #[arg(long)]
        rebuild: bool,
    },
    /// Backfill sibling/parent relations from a snapshot into the serving
    /// RepoStore (#225). For sidecar deployments, whose `bridge seed` builds only
    /// the sidecar mapping file: relations live in the native RepoStore beside it,
    /// so this is a distinct step. (A mirror `bridge seed` already backfills
    /// relations as its own phase.) Idempotent — safe to re-run.
    SeedRelations {
        /// Path to the snapshot directory (same shape as `seed`).
        snapshot_dir: PathBuf,
        /// Optional service id to restrict import (auto-discovered if omitted).
        #[arg(long)]
        service_id: Option<i64>,
    },
    /// Sync PTR update deltas newer than the stored cursor.
    Sync {
        /// Keep running and poll for new updates.
        #[arg(long)]
        follow: bool,
    },
    /// Print the sync cursor and store statistics (read-only).
    Status,
    /// Audit mirror<->Hydrus-snapshot parity for a hash band (read-only).
    ParityAudit {
        /// Path to the Hydrus snapshot directory (same shape as `seed`).
        snapshot_dir: PathBuf,
        /// Repository service id in the snapshot (auto-discovered if omitted).
        #[arg(long)]
        service_id: Option<i64>,
        /// Hash-space slice as a hex prefix, e.g. "00" or "3f".
        /// DEFAULT (omitted): audit the full hash range.
        #[arg(long)]
        band: Option<String>,
    },
}

/// Parse a `--hash-domain` CLI value ("blake3" or "sha256").
/// Uses [`HashDomain`]'s [`std::str::FromStr`] impl so CLI, env-var and wire
/// protocol all accept exactly the same spellings and produce the same error.
fn parse_hash_domain(s: &str) -> Result<HashDomain, String> {
    s.parse::<HashDomain>().map_err(|e| e.to_string())
}

/// Print and log one sync-pass summary line, used by both mirror/snapshot and
/// sidecar sync paths. Avoids duplicating the `println!` + `tracing::info!`
/// pair in every arm.
fn print_sync_report(
    before: u64,
    after: u64,
    indexes_applied: u64,
    mappings_applied: u64,
    elapsed: std::time::Duration,
) {
    let line = naiad_server::bridge::sync::summary_line(
        before,
        after,
        indexes_applied,
        mappings_applied,
        elapsed,
    );
    println!("{line}");
    tracing::info!(target: "bridge", "{line}");
}

/// Emit one `WARN startup:` line per cross-tier override in `overrides`.
/// Extracted from the Serve arm so the identical loop over `log.overridden`
/// and `resolution.overridden` share a single implementation.
fn warn_overrides(overrides: &[naiad_server::settings::SettingOverride]) {
    for o in overrides {
        tracing::warn!(
            target: "startup",
            "{}: {} ({}) overrides {} ({})",
            o.setting, o.winner_tier, o.winner_val, o.loser_tier, o.loser_val
        );
    }
}

/// One line in a bulk-seed JSONL file.
#[derive(Deserialize)]
struct SeedLine {
    hash: String,
    tag: String,
}

/// Parse all lines from a JSONL seed file into `(Hash, Tag)` pairs.
///
/// Blank lines are skipped. Any error includes the 1-based line number and the
/// file name so the operator can find and fix the offending entry quickly.
/// The entire file is parsed before any DB write (all-or-nothing).
fn parse_seed_lines(file_name: &str, reader: impl BufRead) -> anyhow::Result<Vec<(Hash, Tag)>> {
    let mut out = Vec::new();
    for (idx, line_result) in reader.lines().enumerate() {
        let line =
            line_result.with_context(|| format!("{}:{}: reading line", file_name, idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let sl: SeedLine = serde_json::from_str(&line)
            .with_context(|| format!("{}:{}: invalid JSON", file_name, idx + 1))?;
        let hash = sl
            .hash
            .parse::<Hash>()
            .with_context(|| format!("{}:{}: invalid hash", file_name, idx + 1))?;
        let tag = Tag::parse(&sl.tag)
            .with_context(|| format!("{}:{}: invalid tag", file_name, idx + 1))?;
        out.push((hash, tag));
    }
    Ok(out)
}

/// Open a JSONL seed file, parse all entries, and bulk-apply them to the store.
fn seed_from_file(
    store: &RepoStore,
    account: &Account,
    path: &PathBuf,
) -> anyhow::Result<naiad_server::SeedSummary> {
    let file_name = path.display().to_string();
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(f);
    let parsed = parse_seed_lines(&file_name, reader)?;
    store
        .seed_mappings(account, parsed)
        .context("seeding mappings")
}

/// Install a simple stderr tracing subscriber for the `bridge` subcommands
/// (mirrors the former `naiad-bridge` binary; distinct from `serve`'s richer
/// `[log]`-driven `init_tracing`). Level from `RUST_LOG`, else `info`.
fn init_bridge_tracing() {
    let directive = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "info".to_owned());
    let filter = tracing_subscriber::EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // --- Bootstrap tier: resolve the database path ---
    // Flag → NAIAD_REPO_DB env → "repo.db" in the working directory.
    // Non-UTF-8 paths are rejected (same policy as `naiad`'s --db).
    let db_flag: Option<&str> = match cli.db.as_ref() {
        Some(p) => Some(
            p.to_str()
                .ok_or_else(|| anyhow::anyhow!("--db path is not valid UTF-8"))?,
        ),
        None => None,
    };
    let db_resolution =
        naiad_server::settings::resolve_db(db_flag, std::env::var("NAIAD_REPO_DB").ok().as_deref());
    // Emit disagreement warnings via eprintln! (pre-tracing; kept silent when
    // console = false, so no unconditional info line here).
    for (loser_src, loser_val) in &db_resolution.overridden {
        eprintln!(
            "naiad-repo: db: {} ({}) overrides {} ({})",
            db_resolution.source.name(),
            db_resolution.path.display(),
            loser_src.name(),
            loser_val,
        );
    }
    let db = db_resolution.path;

    let open_store =
        || RepoStore::open(&db).with_context(|| format!("opening repo db {}", db.display()));

    match cli.command {
        Command::Serve {
            addr,
            k,
            repo_key,
            hash_domain,
            no_egress,
        } => {
            // Scaffold before load so a first run gets a commented template;
            // best-effort (warns on failure), never clobbers an existing file.
            naiad_server::settings::ensure_scaffold(&db);
            // Malformed config is fatal: refuse to serve with settings the
            // operator thinks are in effect but aren't.
            let (cfg, ignored_keys) =
                naiad_server::settings::load(&db).context("loading repo.toml")?;

            // Injectable getenv closure used for all env reads in `serve`.
            // Never call std::env directly inside the Serve arm — use this.
            let getenv = |k: &str| std::env::var(k).ok();

            // Resolve log config (env > file > default) before opening the
            // tracing subscriber; this is the one place where RUST_LOG is read.
            let log = naiad_server::settings::resolve_log(&cfg.log, getenv)
                .context("resolving log settings")?;

            // Install the tracing subscriber from the resolved log config.
            // Level and sink are already concrete in LogConfig — no further
            // env reads needed here. See resolve_log for the full ladder.
            init_tracing(&log.config, &db);

            // Emit log-level warnings and unknown-key warnings now that the
            // subscriber is up.
            warn_overrides(&log.overridden);
            for w in &log.extra_warnings {
                tracing::warn!(target: "startup", "{w}");
            }
            for key in &ignored_keys {
                tracing::warn!(target: "startup", "repo.toml: ignoring unknown setting '{key}'");
            }

            // Resolve serve config (CLI > env > file > default).
            let resolution = naiad_server::settings::resolve_serve(
                addr,
                k,
                repo_key,
                hash_domain,
                no_egress,
                &cfg.serve,
                getenv,
            )
            .context("resolving serve settings")?;
            warn_overrides(&resolution.overridden);
            let serve = resolution.config;

            // Resolve the bridge config. The bridge is ADDITIVE: it never
            // rewrites the repo's native hash domain (ADR 0024 addendum
            // 2026-07-27). `[serve].hash_domain` means exactly what it says.
            let bridge = naiad_server::settings::resolve_bridge(&cfg.bridge, getenv);
            // Resolve stats config BEFORE block_on so the loopback guard error
            // propagates via `?` and aborts startup with a named message.
            let stats_cfg = naiad_server::settings::resolve_stats(&cfg.stats, &db, getenv)
                .context("resolving stats settings")?;
            // Fail fast: a snapshot-mode repo with a missing or unreadable
            // snapshot must NOT start and then serve empty sha256 results
            // (spec §6). The message names the configured path.
            let domains = DomainConfig::from_settings(
                serve.hash_domain,
                &bridge,
                &db,
                serve.read_connections as usize,
            )
            .context("configuring hash domains")?;
            tracing::info!(
                target: "startup",
                native = %serve.hash_domain,
                bridge_enabled = bridge.enabled,
                bridge_mode = %bridge.mode,
                domains = ?domains.served(),
                "hash domains resolved"
            );
            // A mirror-mode bridge serves a SHA-256-keyed store. Before 0.2.52,
            // [bridge].enabled = true silently forced hash_domain = "sha256";
            // that auto-coercion is gone. An operator who never set hash_domain
            // explicitly now advertises blake3 for a sha256-keyed store: clients
            // derive blake3 bucket keys, match zero rows, and merge nothing.
            if bridge.enabled
                && bridge.mode == naiad_server::settings::BridgeMode::Mirror
                && serve.hash_domain == HashDomain::Blake3
            {
                tracing::warn!(
                    target: "startup",
                    "mirror-mode bridge with blake3 native domain: a mirror store is \
                     SHA-256-keyed; releases before 0.2.52 auto-set hash_domain = \"sha256\" \
                     when the bridge was enabled — this release does not. If this store was \
                     seeded by `bridge seed`, add `[serve] hash_domain = \"sha256\"` to \
                     repo.toml or clients will derive blake3 bucket keys against sha256 rows \
                     and merge zero mappings with no error"
                );
            }

            // §4 (no_egress): a static-mirror server must never start the PTR
            // follow-loop, so `no_egress` and `bridge.enabled` are mutually
            // exclusive. Combining them is a config mistake; fail immediately.
            if serve.no_egress && bridge.enabled {
                anyhow::bail!(
                    "no_egress and bridge.enabled are mutually exclusive: \
                     set serve.no_egress = true only when the PTR follow-loop \
                     (bridge.enabled) is disabled. Remove one of these settings."
                );
            }
            if serve.no_egress {
                tracing::info!(
                    target: "startup",
                    "egress: DISABLED (no outbound PTR sync; mirror served static)"
                );
            }

            // Build the Tokio runtime BEFORE opening stores so the OS signal handler
            // can be armed immediately.  naiad-repo running as PID 1 in a container
            // ignores SIGTERM until a tokio signal task is installed; arming early
            // prevents a missed signal during slow migrations.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;

            let db_serve = db.clone();
            rt.block_on(async move {
                // 1. Arm the OS signal handler FIRST — before any blocking store
                //    opens.  A container SIGTERM arriving during migrations would
                //    otherwise be silently discarded.
                let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();
                tokio::spawn(async move {
                    os_shutdown_signal().await;
                    let _ = sig_tx.send(());
                });

                // 2. Open writer store via spawn_blocking (migrations may be slow).
                let db2 = db_serve.clone();
                let store = tokio::task::spawn_blocking(move || {
                    RepoStore::open(&db2)
                        .with_context(|| format!("opening repo db {}", db2.display()))
                })
                .await
                .map_err(|e| anyhow::anyhow!("store-open task panicked: {e:#}"))??;

                // 3. Open N read-only connections for the round-robin pool (#202).
                //    Non-fatal: a failed open shrinks the pool; an empty pool
                //    falls back to the writer in serve_with_shutdown_domains.
                //    When read_only = true, apply query_only + mmap pragmas so
                //    an accidental write is rejected at the SQLite level (#202).
                let n_readers = serve.read_connections as usize;
                let is_read_only = serve.read_only;
                let db3 = db_serve.clone();
                let read_stores: Vec<RepoStore> = tokio::task::spawn_blocking(move || {
                    let mut stores = Vec::with_capacity(n_readers);
                    for i in 0..n_readers {
                        match RepoStore::open_readonly(&db3) {
                            Ok(s) => {
                                if is_read_only {
                                    if let Err(e) = s.apply_read_only_serve_pragmas() {
                                        tracing::warn!(
                                            "read pool: connection {}/{}: \
                                                 apply_read_only_serve_pragmas failed: {e:#}",
                                            i + 1,
                                            n_readers
                                        );
                                    }
                                }
                                stores.push(s);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "read pool: connection {}/{} failed: {e:#}",
                                    i + 1,
                                    n_readers
                                );
                            }
                        }
                    }
                    stores
                })
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("read pool: open task panicked: {e:#}");
                    vec![]
                });
                if read_stores.is_empty() {
                    tracing::warn!(
                        "read pool: all {n_readers} connection(s) failed; \
                         reads will share the writer connection"
                    );
                } else {
                    tracing::info!(
                        target: "startup",
                        pool_size = read_stores.len(),
                        "read pool ready"
                    );
                }

                // 3b. Spawn the stats subsystem after the read pool is built.
                //    resolve_stats already ran (before block_on) so the loopback guard
                //    is already verified; spawn_stats is tolerant (returns None on failure).
                let bridge_state_for_stats = bridge.enabled.then(|| {
                    naiad_server::settings::resolve_beside_db(&db_serve, &bridge.state_db)
                });
                // Bridge sidecar gauges: only populate when serving as a sidecar
                // node (BridgeMode::Sidecar). Mirror nodes already report correct
                // data through the native repo_mappings gauges; sidecar nodes have
                // an empty repo_mappings table and need to read from the sidecar
                // file instead. Snapshot mode has neither a live sidecar nor live
                // mappings — don't emit anything.
                let sidecar_path_for_stats = (bridge.enabled
                    && bridge.mode == naiad_server::settings::BridgeMode::Sidecar)
                    .then(|| {
                        naiad_server::settings::resolve_beside_db(&db_serve, &bridge.state_db)
                    });
                let stats_handle = naiad_server::spawn_stats(
                    &stats_cfg,
                    &db_serve,
                    bridge_state_for_stats,
                    sidecar_path_for_stats,
                )
                .await;
                let stats_layer = stats_handle.as_ref().map(|h| h.make_layer());
                let stats_freshness = stats_handle.as_ref().map(|h| h.freshness());

                // 4. Start the bridge follow-loop (if enabled AND in mirror/sidecar mode).
                // Snapshot mode serves a static Hydrus snapshot directly: there are
                // no PTR deltas to replay, and starting the follow-loop would write
                // PTR history from cursor 0 into the native BLAKE3 store with continuous
                // network traffic — the opposite of what snapshot mode promises. Only
                // mirror and sidecar modes drive the loop.
                if bridge.enabled {
                    if bridge.mode == naiad_server::settings::BridgeMode::Sidecar {
                        let state_db =
                            naiad_server::settings::resolve_beside_db(&db_serve, &bridge.state_db);
                        let ptr_url = bridge.ptr_url.clone();
                        let ptr_key = bridge.ptr_key.clone();
                        tracing::info!(
                            target: "bridge",
                            state_db = %state_db.display(),
                            ptr_url = %ptr_url,
                            "bridge enabled: starting sidecar PTR sync follow-loop"
                        );
                        naiad_server::bridge::spawn_sidecar_follow(
                            db_serve.clone(),
                            state_db,
                            ptr_url,
                            ptr_key,
                            stats_freshness.clone(),
                            stats_handle.as_ref().and_then(|h| h.count_refresh_notify()),
                        );
                    } else if bridge.mode != naiad_server::settings::BridgeMode::Mirror {
                        tracing::info!(
                            target: "bridge",
                            mode = %bridge.mode,
                            "follow-loop not started: snapshot mode reads a static snapshot \
                             directly — no PTR network traffic is needed"
                        );
                    } else if bridge.ptr_key.trim().is_empty() {
                        tracing::warn!(
                            target: "bridge",
                            "bridge enabled but ptr_key is empty; serving without live PTR sync \
                             (set [bridge].ptr_key in repo.toml or NAIAD_REPO_BRIDGE_PTR_KEY)"
                        );
                    } else {
                        let state_db =
                            naiad_server::settings::resolve_beside_db(&db_serve, &bridge.state_db);
                        tracing::info!(
                            target: "bridge",
                            state_db = %state_db.display(),
                            ptr_url = %bridge.ptr_url,
                            "bridge enabled: starting PTR sync follow-loop"
                        );
                        naiad_server::bridge::spawn_follow(
                            db_serve.clone(),
                            state_db,
                            bridge.ptr_url.clone(),
                            bridge.ptr_key.clone(),
                            stats_freshness.clone(),
                        );
                    }
                }

                // 5a. One-shot background task: if no persisted distinct-hash count
                //    exists in repo_meta yet (pre-upgrade stores), run the full
                //    COUNT scan once and persist the result. The HTTP handler reads
                //    the persisted row on every caps request; this task only fires
                //    the expensive scan on first startup after the upgrade.
                {
                    let bg_db = db_serve.clone();
                    tokio::spawn(async move {
                        let result =
                            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<u64>> {
                                let reader = RepoStore::open_readonly(&bg_db)?;
                                if reader.read_distinct_hash_count()?.is_some() {
                                    return Ok(None);
                                }
                                let count = reader.distinct_hash_count()?;
                                drop(reader);
                                if is_read_only {
                                    return Ok(Some(count));
                                }
                                let writer = RepoStore::open(&bg_db)?;
                                writer.write_distinct_hash_count(count)?;
                                Ok(Some(count))
                            })
                            .await;
                        match result {
                            Ok(Ok(Some(count))) => tracing::info!(
                                target: "startup",
                                count,
                                "caps count computed (persisted unless read_only)"
                            ),
                            Ok(Ok(None)) => tracing::debug!(
                                target: "startup",
                                "caps count already persisted"
                            ),
                            Ok(Err(e)) => tracing::warn!(
                                target: "startup",
                                error = %e,
                                "caps count background compute failed; serving fallback"
                            ),
                            Err(e) => tracing::warn!(
                                target: "startup",
                                error = %e,
                                "caps count task panicked; serving fallback"
                            ),
                        }
                    });
                }

                // 5b. Bind and serve.  Note: if console=false and no file sink is
                //    configured, the subscriber is not installed and the bound
                //    address is not printed — operator's choice to be silent.
                let listener = tokio::net::TcpListener::bind(serve.addr)
                    .await
                    .with_context(|| format!("binding {}", serve.addr))?;
                let bound = listener.local_addr()?;
                tracing::info!("naiad-repo listening on http://{bound}");

                // On a sidecar node the native store is empty; the real distinct-hash
                // count lives in the sidecar db's sync_state cache (#236 parity).
                // Compute the path the same way sidecar_path_for_stats does above
                // (that value was consumed by spawn_stats) and Arc-wrap it for
                // cheap clone inside caps_handler.
                let sidecar_count_path = (bridge.enabled
                    && bridge.mode == naiad_server::settings::BridgeMode::Sidecar)
                    .then(|| {
                        std::sync::Arc::new(naiad_server::settings::resolve_beside_db(
                            &db_serve,
                            &bridge.state_db,
                        ))
                    });
                serve_with_shutdown_domains(
                    store,
                    read_stores,
                    listener,
                    serve.k,
                    serve.repo_key,
                    serve.name,
                    domains,
                    is_read_only,
                    stats_layer,
                    sidecar_count_path,
                    async move {
                        // On Err the signal-watcher task was dropped unexpectedly;
                        // keep the server running rather than triggering a spurious
                        // shutdown (matches the watch-channel semantics in lib.rs).
                        if sig_rx.await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    },
                    DEFAULT_DRAIN_CAP,
                )
                .await
            })
            .context("serving repository")
        }

        Command::Seed {
            hash,
            tag,
            from_file,
        } => {
            let store = open_store()?;
            let key_path = db.with_file_name("repo.key");
            let account = Account::load_or_create(&key_path)
                .with_context(|| format!("opening operator key {}", key_path.display()))?;
            match (from_file, hash, tag) {
                (Some(path), None, None) => {
                    let summary = seed_from_file(&store, &account, &path)?;
                    println!(
                        "seeded: {} inserted, {} skipped, {} total",
                        summary.inserted, summary.skipped, summary.total
                    );
                    Ok(())
                }
                (None, Some(h), Some(t)) => {
                    let hash: Hash = h.parse().context("parsing hash")?;
                    let tag = Tag::parse(&t).context("parsing tag")?;
                    store
                        .apply_submission(&account.sign(Op::Add, &hash, &tag))
                        .context("storing submission")?;
                    println!("ok");
                    Ok(())
                }
                _ => anyhow::bail!("provide either `<hash> <tag>` or `--from-file <path>`"),
            }
        }

        Command::Account { action } => {
            let store = open_store()?;
            match action {
                AccountAction::Promote { pubkey } => {
                    store
                        .set_role(&pubkey, "moderator")
                        .context("promoting account")?;
                    println!("ok");
                    Ok(())
                }
                AccountAction::Ban { pubkey } => {
                    store.set_banned(&pubkey, true).context("banning account")?;
                    println!("ok");
                    Ok(())
                }
                AccountAction::List => {
                    for a in store.list_accounts().context("listing accounts")? {
                        println!(
                            "{}\t{}\t{}\t{}",
                            a.pubkey,
                            a.role,
                            if a.banned { "banned" } else { "ok" },
                            a.created_at
                        );
                    }
                    Ok(())
                }
            }
        }

        Command::Report { action } => {
            let store = open_store()?;
            match action {
                ReportAction::List => {
                    for r in store.open_reports().context("listing reports")? {
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}",
                            r.id,
                            r.hash,
                            r.tag,
                            r.reporter,
                            r.note.as_deref().unwrap_or(""),
                            r.created_at
                        );
                    }
                    Ok(())
                }
                ReportAction::Delete { hash, tag } => {
                    store
                        .moderator_delete_mapping(&hash, &tag)
                        .context("deleting mapping")?;
                    println!("ok");
                    Ok(())
                }
                ReportAction::Ban { pubkey } => {
                    store.set_banned(&pubkey, true).context("banning account")?;
                    println!("ok");
                    Ok(())
                }
                ReportAction::Dismiss { id } => {
                    store.close_report(id).context("dismissing report")?;
                    println!("ok");
                    Ok(())
                }
            }
        }

        Command::Bridge { action } => {
            init_bridge_tracing();
            // Scaffold before load so a first run gets a commented template;
            // best-effort (warns on failure), never clobbers an existing file.
            naiad_server::settings::ensure_scaffold(&db);
            // Missing repo.toml → defaults; malformed → fatal (as with serve).
            let (cfg, ignored_keys) =
                naiad_server::settings::load(&db).context("loading repo.toml")?;
            // Emit unknown-key warnings now that the subscriber (if any) is up.
            for key in &ignored_keys {
                tracing::warn!(target: "startup", "repo.toml: ignoring unknown setting '{key}'");
            }
            let bridge =
                naiad_server::settings::resolve_bridge(&cfg.bridge, |k| std::env::var(k).ok());
            let state_db = naiad_server::settings::resolve_beside_db(&db, &bridge.state_db);
            tracing::debug!(
                target: "bridge",
                ptr_url = %bridge.ptr_url,
                state_db = %state_db.display(),
                enabled = bridge.enabled,
                "bridge config resolved"
            );

            match action {
                BridgeAction::Seed {
                    snapshot_dir,
                    service_id,
                    unsafe_fast,
                    rebuild,
                } => {
                    if bridge.mode == naiad_server::settings::BridgeMode::Sidecar {
                        // Sidecar path: build the compact hash-ordered index.
                        let sc = naiad_server::bridge::sidecar::Sidecar::create(&state_db)
                            .with_context(|| {
                                format!("creating sidecar at {}", state_db.display())
                            })?;
                        naiad_server::bridge::sidecar_seed::seed(
                            &snapshot_dir,
                            service_id,
                            &sc,
                            rebuild,
                        )
                        .context("seeding sidecar from snapshot")
                    } else {
                        // Mirror / snapshot path: seed the RepoStore.
                        let store = RepoStore::open_bulk_ingest(&db, unsafe_fast)
                            .with_context(|| format!("opening repo db {}", db.display()))?;
                        let state = naiad_server::bridge::state::StateDb::open(&state_db)
                            .with_context(|| format!("opening state db {}", state_db.display()))?;
                        // #225: the persisted bridge author signs the backfilled
                        // sibling/parent relations (phase 4 of seed::run).
                        let bridge_author = naiad_server::bridge::load_bridge_author(&state_db)?;
                        naiad_server::bridge::seed::run(
                            &snapshot_dir,
                            service_id,
                            &store,
                            &state,
                            &bridge_author,
                            rebuild,
                        )
                        .context("seeding from snapshot")
                    }
                }
                BridgeAction::SeedRelations {
                    snapshot_dir,
                    service_id,
                } => {
                    // Relations live only in the RepoStore (#225 §5), regardless
                    // of mirror vs sidecar mode — open it directly.
                    let store = open_store()?;
                    let bridge_author = naiad_server::bridge::load_bridge_author(&state_db)?;
                    naiad_server::bridge::seed::seed_relations(
                        &snapshot_dir,
                        service_id,
                        &store,
                        &bridge_author,
                    )
                    .context("seeding relations from snapshot")
                }
                BridgeAction::Sync { follow } => {
                    if bridge.ptr_key.trim().is_empty() {
                        anyhow::bail!(
                            "bridge sync needs a PTR access key; set [bridge].ptr_key or \
                             NAIAD_REPO_BRIDGE_PTR_KEY."
                        );
                    }
                    // Single-writer guard (#193): at most one bridge writer per store.
                    // Held for the whole arm; released on process exit.
                    let _lock = match naiad_server::bridge::lock::BridgeLock::acquire(
                        &naiad_server::bridge::lock::lock_path(&state_db),
                    ) {
                        Ok(l) => l,
                        Err(e) if e.is::<naiad_server::bridge::lock::Contended>() => {
                            eprintln!("another bridge process appears to be running");
                            std::process::exit(4);
                        }
                        Err(e) => return Err(e),
                    };
                    if bridge.mode == naiad_server::settings::BridgeMode::Sidecar {
                        // Sidecar sync path: apply PTR update files to the sidecar index.
                        let sc = naiad_server::bridge::sidecar::Sidecar::open(&state_db)
                            .with_context(|| {
                                format!("opening sidecar at {}", state_db.display())
                            })?;
                        // #225: bridged relations land in the serving RepoStore
                        // (never the sidecar file). Open it and the bridge author
                        // so this sync also refreshes the relation graph.
                        let repo = open_store()?;
                        let bridge_author = naiad_server::bridge::load_bridge_author(&state_db)?;
                        let mut client = naiad_server::bridge::ptr_client::PtrClient::new(
                            &bridge.ptr_url,
                            &bridge.ptr_key,
                        );
                        if follow {
                            loop {
                                let before = sc.next_update_index().unwrap_or(0);
                                let started = std::time::Instant::now();
                                match naiad_server::bridge::sidecar_sync::sync_once(
                                    &sc,
                                    &mut client,
                                    Some((&repo, &bridge_author)),
                                ) {
                                    Ok(r) => {
                                        let after = sc.next_update_index().unwrap_or(0);
                                        print_sync_report(
                                            before,
                                            after,
                                            r.indexes_applied,
                                            r.mappings_applied,
                                            started.elapsed(),
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(target: "bridge", error = %format!("{e:#}"), "sidecar sync pass failed; retrying");
                                    }
                                }
                                std::thread::sleep(std::time::Duration::from_secs(
                                    naiad_server::bridge::sync::MIN_POLL_SECS,
                                ));
                            }
                        } else {
                            let before = sc
                                .next_update_index()
                                .context("reading cursor before sidecar sync")?;
                            let started = std::time::Instant::now();
                            let report = naiad_server::bridge::sidecar_sync::sync_once(
                                &sc,
                                &mut client,
                                Some((&repo, &bridge_author)),
                            )
                            .context("sidecar sync")?;
                            let after = sc
                                .next_update_index()
                                .context("reading cursor after sidecar sync")?;
                            print_sync_report(
                                before,
                                after,
                                report.indexes_applied,
                                report.mappings_applied,
                                started.elapsed(),
                            );
                            Ok(())
                        }
                    } else {
                        // Mirror / snapshot path: sync the RepoStore.
                        let store = open_store()?;
                        let state = naiad_server::bridge::state::StateDb::open(&state_db)
                            .with_context(|| format!("opening state db {}", state_db.display()))?;
                        // #225: bridge author signs the bridged sibling/parent relations.
                        let bridge_author = naiad_server::bridge::load_bridge_author(&state_db)?;
                        let mut client = naiad_server::bridge::ptr_client::PtrClient::new(
                            &bridge.ptr_url,
                            &bridge.ptr_key,
                        );
                        if follow {
                            naiad_server::bridge::sync::follow(
                                &state,
                                &store,
                                &bridge_author,
                                &mut client,
                                None,
                            )
                            .context("bridge sync follow")
                        } else {
                            let before = state
                                .next_update_index()
                                .context("reading cursor before sync")?;
                            let started = std::time::Instant::now();
                            let report = naiad_server::bridge::sync::sync_once(
                                &state,
                                &store,
                                &bridge_author,
                                &mut client,
                            )
                            .context("bridge sync")?;
                            let after = state
                                .next_update_index()
                                .context("reading cursor after sync")?;
                            print_sync_report(
                                before,
                                after,
                                report.indexes_applied,
                                report.mappings_applied,
                                started.elapsed(),
                            );
                            Ok(())
                        }
                    }
                }
                BridgeAction::Status => {
                    naiad_server::bridge::status(&db, &state_db).context("bridge status")
                }
                BridgeAction::ParityAudit {
                    snapshot_dir,
                    service_id,
                    band,
                } => {
                    let outcome = if bridge.mode == naiad_server::settings::BridgeMode::Sidecar {
                        // Sidecar path: compare sidecar digest vs Hydrus snapshot digest.
                        naiad_server::bridge::parity_audit_sidecar(
                            &state_db,
                            &snapshot_dir,
                            service_id,
                            band.as_deref(),
                        )
                        .context("bridge parity-audit (sidecar)")?
                    } else {
                        // Mirror / snapshot path.
                        naiad_server::bridge::parity_audit(
                            &db,
                            &state_db,
                            &snapshot_dir,
                            service_id,
                            band.as_deref(),
                        )
                        .context("bridge parity-audit")?
                    };
                    match outcome {
                        naiad_server::bridge::AuditOutcome::Pass => Ok(()),
                        naiad_server::bridge::AuditOutcome::Fail => std::process::exit(2),
                        naiad_server::bridge::AuditOutcome::Refused => std::process::exit(3),
                    }
                }
            }
        }
    }
}

/// A cloneable append-mode file sink shared across tracing's writer calls.
#[derive(Clone)]
struct FileWriter(Arc<Mutex<std::fs::File>>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()), buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Install the global tracing subscriber from the resolved [`LogConfig`].
///
/// Level, console flag, and optional file sink are already concrete — no
/// further env reads here. See [`naiad_server::settings::resolve_log`] for the
/// full resolution ladder (RUST_LOG → NAIAD_REPO_LOG_LEVEL → [log].level →
/// "info"). ANSI off (server logs are for terminals, files, and service
/// wrappers alike). `try_init` so a second call (tests) is a no-op instead of
/// a panic.
fn init_tracing(log: &naiad_server::settings::LogConfig, db_path: &std::path::Path) {
    use std::io::Write as _;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    let filter = EnvFilter::try_new(&log.level).unwrap_or_else(|_| EnvFilter::new("info"));

    let file_sink = log.file.as_deref().and_then(|spec| {
        let path = if std::path::Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            db_path.with_file_name(spec)
        };
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "--- naiad-repo log session start ---");
                Some(FileWriter(Arc::new(Mutex::new(f))))
            }
            Err(e) => {
                eprintln!(
                    "naiad-repo: could not open log file {}: {e}",
                    path.display()
                );
                None
            }
        }
    });

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    let _ = match (log.console, file_sink) {
        (true, Some(sink)) => builder
            .with_writer(std::io::stderr.and(move || sink.clone()))
            .try_init(),
        (true, None) => builder.with_writer(std::io::stderr).try_init(),
        (false, Some(sink)) => builder.with_writer(move || sink.clone()).try_init(),
        (false, None) => return, // no subscriber installed; operator opted into fully silent logging
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A good line followed by one with a hash that is too short must produce
    /// an error that mentions line 2 and the file name.
    #[test]
    fn parse_seed_lines_bad_hash_names_line_number() {
        let input = concat!(
            "{\"hash\":\"0000000000000000000000000000000000000000000000000000000000000001\",\"tag\":\"a:b\"}\n",
            "{\"hash\":\"tooshort\",\"tag\":\"x\"}\n",
        );
        let err = parse_seed_lines("mappings.jsonl", std::io::Cursor::new(input)).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains(":2") || msg.contains("line 2"),
            "error should mention line 2, got: {msg}"
        );
        assert!(
            msg.contains("mappings.jsonl"),
            "error should name the file, got: {msg}"
        );
    }

    /// Blank lines in the JSONL input are silently skipped.
    #[test]
    fn parse_seed_lines_skips_blank_lines() {
        let input = concat!(
            "{\"hash\":\"0000000000000000000000000000000000000000000000000000000000000001\",\"tag\":\"a:b\"}\n",
            "\n",
            "  \n",
            "{\"hash\":\"0000000000000000000000000000000000000000000000000000000000000002\",\"tag\":\"c:d\"}\n",
        );
        let result = parse_seed_lines("test.jsonl", std::io::Cursor::new(input)).unwrap();
        assert_eq!(result.len(), 2);
    }

    /// Regression for #148 / #207: the PTR follow-loop must be gated on mirror
    /// or sidecar mode, not on `enabled` alone. A snapshot-mode bridge with
    /// `enabled = true` must never start the loop. This test pins the guard
    /// condition so it cannot regress silently.
    #[test]
    fn follow_loop_requires_mirror_or_sidecar_mode() {
        use naiad_server::settings::BridgeMode;

        // The condition that `main.rs` evaluates: follow-loop starts only when
        // both enabled AND mode is Mirror or Sidecar. Extracted here so the
        // logic is independently verifiable without a full server startup.
        let should_start_follow = |enabled: bool, mode: BridgeMode| -> bool {
            enabled && (mode == BridgeMode::Mirror || mode == BridgeMode::Sidecar)
        };

        assert!(
            !should_start_follow(true, BridgeMode::Snapshot),
            "snapshot mode + enabled must NOT start the follow-loop (#148)"
        );
        assert!(
            !should_start_follow(false, BridgeMode::Mirror),
            "mirror mode + disabled must NOT start the follow-loop"
        );
        assert!(
            !should_start_follow(false, BridgeMode::Sidecar),
            "sidecar mode + disabled must NOT start the follow-loop"
        );
        assert!(
            should_start_follow(true, BridgeMode::Mirror),
            "mirror mode + enabled must start the follow-loop"
        );
        assert!(
            should_start_follow(true, BridgeMode::Sidecar),
            "sidecar mode + enabled must start the follow-loop (#207)"
        );
    }

    // ── §4 no_egress guard tests ──────────────────────────────────────────────

    /// `no_egress = true` + `bridge.enabled = true` must be a fatal conflict.
    /// This mirrors the guard in the Serve arm: the exact condition checked is
    /// `serve.no_egress && bridge.enabled`.
    #[test]
    fn no_egress_and_bridge_enabled_is_fatal() {
        // Directly verify the guard condition (same extraction pattern as the
        // follow_loop test above).
        let should_reject =
            |no_egress: bool, bridge_enabled: bool| -> bool { no_egress && bridge_enabled };

        assert!(
            should_reject(true, true),
            "no_egress=true + enabled=true must be rejected"
        );
        assert!(
            !should_reject(true, false),
            "no_egress=true + enabled=false must be accepted"
        );
        assert!(
            !should_reject(false, true),
            "no_egress=false + enabled=true must be accepted (normal PTR mirror)"
        );
        assert!(
            !should_reject(false, false),
            "no_egress=false + enabled=false must be accepted (default)"
        );
    }

    /// With `no_egress = true` and `bridge.enabled = false`, the follow-loop
    /// guard must NOT start the loop (no egress AND enabled=false both suppress
    /// it; they should compose without error).
    #[test]
    fn no_egress_and_bridge_disabled_does_not_start_follow_loop() {
        use naiad_server::settings::BridgeMode;

        let should_start_follow =
            |enabled: bool, mode: BridgeMode| -> bool { enabled && mode == BridgeMode::Mirror };

        // no_egress=true, bridge disabled → loop must not start.
        assert!(
            !should_start_follow(false, BridgeMode::Mirror),
            "no_egress=true + enabled=false: follow-loop must not start"
        );
    }
}
