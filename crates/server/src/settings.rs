//! Server configuration — `repo.toml`, `NAIAD_REPO_*` environment variables,
//! and CLI flags — resolved per ADR 0022 / ADR 0025.
//!
//! Precedence (first-set-wins): CLI flag → environment variable → `repo.toml`
//! → built-in default. The file lives beside the repository database
//! (`db.with_file_name("repo.toml")`), mirroring the client's
//! `naiad.toml`-beside-`naiad.db` rule. A malformed file is a fatal startup
//! error (unlike the client's keep-last-good: at process start there is no
//! "last good"). Unknown keys and sections are tolerated
//! (no `deny_unknown_fields`). See [`resolve_db`], [`resolve_serve`], and
//! [`resolve_log`] for the full resolution ladders.

use std::fmt::Display;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use naiad_netproto::HashDomain;
use tracing_subscriber::EnvFilter;

/// Built-in default bind address (matches the pre-config CLI default).
/// Referenced verbatim in the `serve --help` doc comments in `main.rs` — keep in sync.
pub const DEFAULT_ADDR: &str = "127.0.0.1:9090";
/// Built-in default k-anonymity crowd-size floor (ADR 0001).
/// Referenced verbatim in the `serve --help` doc comments in `main.rs` — keep in sync.
pub const DEFAULT_K: u64 = 1000;
/// Built-in default for the repository database filename.
/// Referenced verbatim in the `--db` help in `main.rs` — keep in sync.
pub const DEFAULT_DB: &str = "repo.db";
/// Built-in default log level.
/// Referenced verbatim in the `serve --help` env table in `main.rs` — keep in sync.
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// Built-in default for console (stderr) logging.
/// Referenced verbatim in the `serve --help` env table in `main.rs` — keep in sync.
pub const DEFAULT_CONSOLE: bool = true;
/// Default PTR base URL (the public Hydrus PTR).
pub const DEFAULT_PTR_URL: &str = "https://ptr.hydrus.network:45871";
/// Default PTR access key: the built-in public read key (from the former
/// `naiad-bridge` config).
pub const DEFAULT_PTR_KEY: &str =
    "4a285629721ca442541ef2c15ea17d1f7f7578b0c3f4f5f2a05f8f0ab297786f";
/// Default bridge state DB name; relative resolves beside the repo db.
pub const DEFAULT_STATE_DB: &str = "bridge-state.db";

/// Default server-set precision ceiling for SHA-256-domain queries, in prefix
/// bits. 256 = exact-hash queries allowed, which is sound for snapshot mode:
/// the k-anonymity dance exists to protect a client from an *untrusted*
/// operator, and a self-hosted bridge has no such adversary (design §Backend 1,
/// "Precision"). Lower it to enforce coarseness on a shared deployment.
pub const DEFAULT_BRIDGE_MAX_QUERY_BITS: u32 = 256;

/// Default minimum prefix bits required for SHA-256-domain bucket queries.
/// Mirrors [`crate::domain::SNAPSHOT_MIN_QUERY_BITS`] — kept here as the
/// single source of truth for the settings default so we do not import a
/// domain constant into the settings module.
pub const DEFAULT_BRIDGE_MIN_QUERY_BITS: u32 = 8;

/// Default number of read-only SQLite connections in the round-robin pool (#202).
/// Each pooled connection serves a distinct concurrent handler under SQLite WAL.
/// Clamped to [1, 64] at resolution time.
pub const DEFAULT_READ_CONNECTIONS: u32 = 4;

/// Default serve-only mode: false (writes accepted). When true, write endpoints
/// return 403 and pooled read connections receive `PRAGMA query_only = ON`.
/// Env: `NAIAD_REPO_READ_ONLY`. File key: `[serve].read_only`.
pub const DEFAULT_READ_ONLY: bool = false;

/// Default stats-subsystem enablement. Env: `NAIAD_REPO_STATS_ENABLED`.
pub const DEFAULT_STATS_ENABLED: bool = true;
/// Default stats listener bind address (loopback only). Env: `NAIAD_REPO_STATS_LISTEN`.
pub const DEFAULT_STATS_LISTEN: &str = "127.0.0.1:9092";
/// Default: refuse a non-loopback stats bind. Env: `NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK`.
pub const DEFAULT_STATS_ALLOW_NON_LOOPBACK: bool = false;
/// Default stats DB filename; a relative path resolves beside the repo db.
pub const DEFAULT_STATS_DB: &str = "stats.db";

/// Which backend serves a repo's added SHA-256 domain (design §Decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeMode {
    /// The eager materialized store fed by `bridge seed` / `bridge sync`.
    /// The default so an upgraded mirror deployment behaves exactly as before.
    #[default]
    Mirror,
    /// Query a static Hydrus `client.db` snapshot directly: no seed, no store.
    Snapshot,
    /// Compact hash-ordered sidecar index (ADR 0028) — the PTR backend.
    Sidecar,
}

impl Display for BridgeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BridgeMode::Mirror => "mirror",
            BridgeMode::Snapshot => "snapshot",
            BridgeMode::Sidecar => "sidecar",
        })
    }
}

/// Parse a bridge-mode string: `"mirror"` / `"snapshot"` / `"sidecar"`, trimmed
/// and case-insensitive. Anything else → `None`.
#[must_use]
fn parse_bridge_mode_str(s: &str) -> Option<BridgeMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mirror" => Some(BridgeMode::Mirror),
        "snapshot" => Some(BridgeMode::Snapshot),
        "sidecar" => Some(BridgeMode::Sidecar),
        _ => None,
    }
}

/// First-run template, written beside the db when `repo.toml` is absent and
/// shipped verbatim in the portable zip as the sample config
/// (`scripts/repo.toml.sample` — a unit test keeps the two in sync).
/// All keys ship commented so an untouched file never triggers a cross-tier
/// warning when an environment variable is set. Parsing the template yields
/// `RepoSettings::default()`.
pub const SCAFFOLD: &str = "\
# naiad-repo settings -- read once when `naiad-repo serve` starts.
# CLI flags and NAIAD_REPO_* environment variables override these values.
# Edit and restart to apply. This file lives beside the repository database (repo.db).

[serve]
# Address to bind. Use 0.0.0.0:9090 to accept connections from other machines.
# Env: NAIAD_REPO_ADDR
# addr = \"127.0.0.1:9090\"
# Crowd-size floor for k-anonymity prefix sizing (ADR 0001).
# Env: NAIAD_REPO_K
# k = 1000
# Optional: 64-char hex Ed25519 pubkey advertised in /repo/caps as the repo's public identity.
# (Not the operator signing key -- that stays in repo.key beside the database.)
# Env: NAIAD_REPO_KEY
# repo_key = \"\"
# Optional: human-readable repo display name advertised in /repo/caps.
# Clients use it as the repo's local name when subscribing.
# Env: NAIAD_REPO_NAME
# name = \"\"
# Hash domain this repo natively serves, advertised in /repo/caps.
# Default: blake3 (native naiad identity). [bridge].enabled = true ADDS a
# sha256 domain alongside this one -- it never replaces it.
# Env: NAIAD_REPO_HASH_DOMAIN
# hash_domain = \"blake3\"
# Fail-closed no-egress assertion: when true, refuse to start the PTR
# follow-loop (fatal if [bridge].enabled = true is also set) and emit a
# startup line confirming no outbound PTR sync. Use on a static mirror
# container where the follow-loop must never open. Default: false.
# Env: NAIAD_REPO_NO_EGRESS  CLI: --no-egress
# no_egress = false
# Number of read-only SQLite connections in the round-robin pool (#202).
# Parallel caps/buckets/snapshot reads each borrow a distinct connection so
# they run concurrently under SQLite WAL instead of serialising. Clamp [1, 64].
# Env: NAIAD_REPO_READ_CONNECTIONS
# read_connections = 4
# Serve-only mode: when true, write endpoints (POST /repo/submit, /repo/report,
# POST /repo/moderate, POST /repo/relations/submit) return 403 Forbidden, and
# pooled read connections receive PRAGMA query_only = ON plus a 1 GiB mmap
# window for read throughput. The writer store is still opened (WAL checkpoint
# on clean shutdown requires it). A separate bridge sync process writing the
# same live store is safe: WAL readers see committed changes on the next
# transaction. Does NOT set SQLite immutable=1 (unsafe under nightly sync,
# design §10.1). Default: false.
# Env: NAIAD_REPO_READ_ONLY
# read_only = false

# [bridge]
# Serve an ADDITIONAL sha256 hash domain from Hydrus data (ADR 0024).
# Env mirrors: NAIAD_REPO_BRIDGE_*.
# enabled = false
# Which sha256 backend: \"mirror\" (eager store fed by `bridge seed`/`sync`)
# or \"snapshot\" (query a static Hydrus client.db set directly; no seed,
# no store, freshness = snapshot age).
# mode = \"mirror\"
# Snapshot mode only: directory holding client.db, client.master.db and
# client.mappings.db. Missing or unreadable is a fatal startup error.
# snapshot_dir = \"/srv/ptr-snapshot\"
# Snapshot mode only: Hydrus tag-service id inside the snapshot.
# Omit to auto-discover.
# snapshot_service_id = 9
# Precision ceiling for sha256-domain queries, in prefix bits. Lower it to
# enforce k-anonymity coarseness on a shared deployment. The effective ceiling
# 256 is enforced by the server's domain policy regardless of what is set here.
# Values below 8 are raised to 8 at startup (with a warning).
# max_query_bits = 256
# Minimum prefix bits required for sha256-domain bucket queries (default 8).
# Raise this on large snapshots (e.g. the PTR, ~200M hashes) where a coarse
# query would trigger a doomed multi-GB scan before hitting the response budget
# or timeout. With min_query_bits = 16, requests below 16 bits return a fast 400
# instead. At 16 bits on the PTR the k-anonymity crowd is still ~3000 files per
# bucket, so privacy is not harmed. Small repos should keep the default of 8.
# Bench: 8 bits on the PTR never completed (killed at ~2 h); 16 bits succeeds
# at ~6 MB per 4-bucket request on spinning disk.
# Env: NAIAD_REPO_BRIDGE_MIN_QUERY_BITS
# min_query_bits = 8
# Mirror mode only: PTR connection and sync state.
# ptr_url = \"https://ptr.hydrus.network:45871\"
# ptr_key = \"<64-char hex access key; default is the public PTR read key>\"
# state_db = \"bridge-state.db\"

# [stats]
# Built-in statistics subsystem: records request/system/store/sync metrics into
# a separate stats.db and serves a self-contained dashboard on a loopback-only
# second port (reach it with: ssh -L 9092:localhost:9092 <host>).
# Env mirrors: NAIAD_REPO_STATS_*.
# enabled = true
# Bind address for the stats listener. MUST be a loopback address (127.0.0.0/8
# or ::1) unless allow_non_loopback = true. A non-loopback bind without that
# flag is a FATAL startup error -- the stats port is never meant to be public.
# Env: NAIAD_REPO_STATS_LISTEN
# listen = \"127.0.0.1:9092\"
# Escape hatch: permit a non-loopback listen address. Leave false unless you
# deliberately front the stats port behind your own access control.
# Env: NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK
# allow_non_loopback = false
# stats.db path. A relative path resolves beside the repository database.
# Delete the file to reset all history (rollback is trivial).
# Env: NAIAD_REPO_STATS_DB
# db_path = \"stats.db\"

[log]
# Log filter: trace | debug | info | warn | error. RUST_LOG beats NAIAD_REPO_LOG_LEVEL beats this.
# Env: NAIAD_REPO_LOG_LEVEL
# level = \"info\"
# Emit log lines to stderr. Default: true.
# Env: NAIAD_REPO_LOG_CONSOLE
# console = true
# Uncomment to also write a log file. A relative path is resolved beside the
# database. Appends across runs.
# Env: NAIAD_REPO_LOG_FILE
# file = \"repo.log\"
";

/// All file-resident server settings. Every field defaults, so a missing key,
/// section, or file yields the defaults.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct RepoSettings {
    #[serde(default)]
    pub serve: ServeSettings,
    #[serde(default)]
    pub log: LogSettings,
    #[serde(default)]
    pub bridge: BridgeSettings,
    #[serde(default)]
    pub stats: StatsSettings,
}

/// `[serve]` — file-level values for the `serve` subcommand's options. All
/// `Option` so the merge in [`resolve_serve`] can tell "set in file" from
/// "absent" (absent falls through to the built-in default).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct ServeSettings {
    /// Bind address. Absent = `DEFAULT_ADDR`.
    #[serde(default)]
    pub addr: Option<SocketAddr>,
    /// k-anonymity crowd-size floor. Absent = `DEFAULT_K`.
    #[serde(default)]
    pub k: Option<u64>,
    /// Repo identity hint advertised in `/repo/caps`. Absent = none.
    #[serde(default)]
    pub repo_key: Option<String>,
    /// Hash domain advertised in `/repo/caps`. Absent = `Blake3`.
    #[serde(default)]
    pub hash_domain: Option<HashDomain>,
    /// Fail-closed assertion that this process makes no outbound PTR
    /// connections (design §4, #190). When `true`: the PTR follow-loop is
    /// refused at startup (a fatal error if `[bridge].enabled = true` is also
    /// set), and a startup log line confirms the no-egress state.
    /// Absent = `false` (existing behaviour unchanged).
    /// Env: `NAIAD_REPO_NO_EGRESS`. CLI: `--no-egress`.
    #[serde(default)]
    pub no_egress: Option<bool>,
    /// Number of read-only SQLite connections in the round-robin pool (#202).
    /// Absent = [`DEFAULT_READ_CONNECTIONS`] (4). Clamped to [1, 64] at
    /// resolution time. Env: `NAIAD_REPO_READ_CONNECTIONS`.
    #[serde(default)]
    pub read_connections: Option<u32>,
    /// Serve-only mode (#202): when `true`, write endpoints return 403 and
    /// pooled read connections receive `PRAGMA query_only = ON` plus a 1 GiB
    /// mmap window. The writer store remains open for WAL checkpoint on clean
    /// shutdown. Absent = `false` (all writes accepted).
    /// Env: `NAIAD_REPO_READ_ONLY`.
    #[serde(default)]
    pub read_only: Option<bool>,
    /// Optional display name advertised in `/repo/caps`. Absent = none.
    #[serde(default)]
    pub name: Option<String>,
}

/// `[log]` — same keys as the client's `naiad.toml` `[log]`.
/// All fields are `Option` so "absent from file" is distinguishable from
/// "explicitly set to the default" — the same change ADR 0023 made to the
/// daemon's `LogSettings`. Defaults now live in [`resolve_log`], not here.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct LogSettings {
    /// Log filter: bare level or full per-target directives. Absent = resolved
    /// from env or `DEFAULT_LOG_LEVEL` in [`resolve_log`].
    #[serde(default)]
    pub level: Option<String>,
    /// Emit to stderr. Absent = resolved from env or `DEFAULT_CONSOLE`.
    #[serde(default)]
    pub console: Option<bool>,
    /// Additional append-mode file sink; relative paths resolve beside the
    /// database. Absent = no file.
    #[serde(default)]
    pub file: Option<String>,
}

/// `[bridge]` — bridge settings. An absent section yields a disabled bridge
/// with built-in defaults. Every field defaults so partial sections are fine.
/// The store DB is the global `--db` (no `db` key); `addr`/`k`/`hash_domain`
/// stay under `[serve]`. `mode` selects the SHA-256 backend.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct BridgeSettings {
    /// Serve an added SHA-256 domain from Hydrus data. Default `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Which SHA-256 backend answers the added domain. Default
    /// [`BridgeMode::Mirror`].
    #[serde(default)]
    pub mode: BridgeMode,
    /// Snapshot mode only: directory holding `client.db`, `client.master.db`
    /// and `client.mappings.db`. Required when `mode = "snapshot"`; a missing
    /// or unreadable directory is a fatal startup error (spec §6).
    #[serde(default)]
    pub snapshot_dir: Option<String>,
    /// Snapshot mode only: Hydrus tag-service id inside the snapshot. Absent =
    /// auto-discover (mirrors `bridge seed --service-id`).
    #[serde(default)]
    pub snapshot_service_id: Option<i64>,
    /// Configured precision ceiling for SHA-256-domain queries, in prefix bits.
    /// Default [`DEFAULT_BRIDGE_MAX_QUERY_BITS`]. The raw value is stored as
    /// configured; the effective ceiling 256 is enforced by the server's domain
    /// policy, not here.
    #[serde(default = "default_bridge_max_query_bits")]
    pub max_query_bits: u32,
    /// Minimum prefix bits required for SHA-256-domain bucket queries.
    /// Default [`DEFAULT_BRIDGE_MIN_QUERY_BITS`] (8). Raise on large snapshots
    /// (e.g. PTR) so coarse-prefix queries return a fast 400 rather than a
    /// doomed multi-GB scan. The effective floor is clamped into
    /// `[SNAPSHOT_MIN_QUERY_BITS, max_query_bits]` by
    /// [`crate::domain::DomainConfig::from_settings`].
    #[serde(default = "default_bridge_min_query_bits")]
    pub min_query_bits: u32,
    /// Mirror mode only: PTR base URL. Default [`DEFAULT_PTR_URL`].
    #[serde(default = "default_ptr_url")]
    pub ptr_url: String,
    /// Mirror mode only: PTR access key (64-char hex). Default
    /// [`DEFAULT_PTR_KEY`].
    #[serde(default = "default_ptr_key")]
    pub ptr_key: String,
    /// Mirror mode only: bridge state DB path; a relative path resolves beside
    /// the repo db. Default [`DEFAULT_STATE_DB`].
    #[serde(default = "default_state_db")]
    pub state_db: String,
}

fn default_ptr_url() -> String {
    DEFAULT_PTR_URL.to_owned()
}
fn default_ptr_key() -> String {
    DEFAULT_PTR_KEY.to_owned()
}
fn default_state_db() -> String {
    DEFAULT_STATE_DB.to_owned()
}
fn default_bridge_max_query_bits() -> u32 {
    DEFAULT_BRIDGE_MAX_QUERY_BITS
}
fn default_bridge_min_query_bits() -> u32 {
    DEFAULT_BRIDGE_MIN_QUERY_BITS
}

impl Default for BridgeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: BridgeMode::default(),
            snapshot_dir: None,
            snapshot_service_id: None,
            max_query_bits: default_bridge_max_query_bits(),
            min_query_bits: default_bridge_min_query_bits(),
            ptr_url: default_ptr_url(),
            ptr_key: default_ptr_key(),
            state_db: default_state_db(),
        }
    }
}

/// `[stats]` — built-in statistics subsystem. Absent section = defaults
/// (enabled, loopback 9092). All `Option` so `resolve_stats` can tell
/// "set in file" from "absent".
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct StatsSettings {
    /// Enable the stats listener, samplers, and request middleware. Absent = `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Bind address for the loopback stats listener. Absent = `127.0.0.1:9092`.
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    /// Escape hatch: allow a non-loopback bind. Absent = `false`.
    #[serde(default)]
    pub allow_non_loopback: Option<bool>,
    /// `stats.db` path; a relative path resolves beside the repo db. Absent = `"stats.db"`.
    #[serde(default)]
    pub db_path: Option<String>,
}

/// The settings-file path beside a database file: `<db dir>/repo.toml`.
/// Mirrors the client's `settings_path_for` (`naiad.toml` beside `naiad.db`).
#[must_use]
pub fn settings_path_for(db_path: &Path) -> PathBuf {
    db_path.with_file_name("repo.toml")
}

/// Load `repo.toml` beside `db_path`. Missing file → defaults. Malformed file
/// or unreadable file → `Err` (the caller exits non-zero: fatal on bad config).
/// Unknown keys are collected and returned so the caller can warn about them.
pub fn load(db_path: &Path) -> anyhow::Result<(RepoSettings, Vec<String>)> {
    let path = settings_path_for(db_path);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let de = toml::Deserializer::new(&content);
            let mut ignored: Vec<String> = Vec::new();
            let settings: RepoSettings = serde_ignored::deserialize(de, |p| {
                ignored.push(p.to_string());
            })
            .with_context(|| format!("parsing {}", path.display()))?;
            Ok((settings, ignored))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((RepoSettings::default(), vec![])),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the commented first-run template if `repo.toml` is absent. Best
/// effort: a write failure (e.g. read-only directory) is warned to stderr and
/// otherwise ignored — serving matters more than templating. Never touches an
/// existing file. Uses a temp-file-then-rename to avoid a partial write being
/// mistaken for a valid config on a subsequent startup. Runs before tracing
/// is up, hence `eprintln!`.
pub fn ensure_scaffold(db_path: &Path) {
    let path = settings_path_for(db_path);
    if path.exists() {
        return;
    }
    // Write to a sibling temp file first so that a crash mid-write never
    // leaves a truncated repo.toml that load() would reject as malformed.
    let tmp = path.with_file_name("repo.toml.tmp");
    if let Err(e) = std::fs::write(&tmp, SCAFFOLD) {
        eprintln!(
            "naiad-repo: could not write config template {}: {e}",
            path.display()
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!(
            "naiad-repo: could not write config template {}: {e}",
            path.display()
        );
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // Intentional first-run UX: unlike the daemon's silent scaffold, the
    // server is headless and the operator needs to know a config file was
    // created so they can find and edit it.
    eprintln!("naiad-repo: wrote config template {}", path.display());
}

// ---------------------------------------------------------------------------
// Private env helpers (injectable getenv, never std::env directly)
// ---------------------------------------------------------------------------

/// Return the raw value of `key` from the environment, or `None` if absent.
/// An empty string is returned as `Some("")` — callers decide what empty means.
fn env_present(getenv: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    getenv(key)
}

/// Return the value of `key` from the environment, or `None` if absent or
/// blank (whitespace-only). Filters out values that would resolve to nothing.
fn env_nonblank(getenv: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    getenv(key).filter(|s| !s.trim().is_empty())
}

/// Parse a bool from an env-var value: `1|true|yes|on` → `true`,
/// `0|false|no|off` → `false`, anything else → `None`.
/// Comparison is trimmed and case-insensitive.
fn parse_env_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse a hash-domain string: `"blake3"` → `Blake3`, `"sha256"` → `Sha256`,
/// anything else → `None`. Trimmed and case-insensitive.
/// Used by both CLI (delegated from `parse_hash_domain` in `main.rs`) and env.
/// Delegates to `HashDomain::from_str` so CLI, env, and the wire protocol
/// always accept exactly the same spellings.
pub fn parse_hash_domain_str(s: &str) -> Option<HashDomain> {
    s.parse().ok()
}

/// Resolve a path value (from config or CLI) relative to the repo database.
///
/// If `value` is already absolute it is returned as-is. Otherwise it is
/// treated as a filename in the same directory as `db_path`, the rule used
/// for `[log].file`, `[bridge].state_db`, and `[bridge].snapshot_dir`.
pub fn resolve_beside_db(db_path: &Path, value: &str) -> PathBuf {
    let raw = Path::new(value);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        db_path.with_file_name(raw)
    }
}

// ---------------------------------------------------------------------------
// Database resolution (bootstrap tier)
// ---------------------------------------------------------------------------

/// Which tier determined the database path.
#[derive(Debug, Clone, PartialEq)]
pub enum DbSource {
    Flag,
    Env,
    /// Path came from the built-in default (`repo.db` in the working directory).
    /// Never appears in [`DbResolution::overridden`] — only explicitly-set tiers
    /// (Flag and Env) are reported when they lose to a higher-priority tier.
    Default,
}

impl DbSource {
    /// Human-readable tier name for override warnings.
    pub fn name(&self) -> &'static str {
        match self {
            DbSource::Flag => "--db flag",
            DbSource::Env => "NAIAD_REPO_DB env",
            DbSource::Default => "default (repo.db)",
        }
    }
}

/// Result of resolving the `--db` bootstrap tier.
#[derive(Debug, Clone, PartialEq)]
pub struct DbResolution {
    /// Resolved database path.
    pub path: PathBuf,
    /// Which tier provided the path.
    pub source: DbSource,
    /// Explicitly-set losing tiers that differed from the winner
    /// (tuples of `(losing source, losing value string)`).
    /// `DbSource::Default` is never in this list.
    pub overridden: Vec<(DbSource, String)>,
}

/// Resolve the database path: `--db flag` → `NAIAD_REPO_DB` env → `"repo.db"`.
/// Flag values are used as-is; env values are trimmed (leading/trailing
/// whitespace stripped) so a shell export typo doesn't open a path with a
/// literal leading space. Whitespace-only values are treated as absent.
/// `Default` never appears in `DbResolution::overridden`.
///
/// A separate resolver rather than reusing `naiad_bootstrap::resolve_db_path`:
/// that function hardcodes `naiad.db` and the exe-directory default;
/// generalising it would perturb the client callers. See ADR 0025 Alternatives.
pub fn resolve_db(cli_db: Option<&str>, env_db: Option<&str>) -> DbResolution {
    let flag = cli_db.filter(|s| !s.trim().is_empty());
    // Trim the env value: a leading-space env var must not open a leading-space path.
    let env = env_db.map(|s| s.trim()).filter(|s| !s.is_empty());

    match (flag, env) {
        (Some(f), Some(e)) => {
            let path = PathBuf::from(f);
            let mut overridden = Vec::new();
            if f != e {
                overridden.push((DbSource::Env, e.to_owned()));
            }
            DbResolution {
                path,
                source: DbSource::Flag,
                overridden,
            }
        }
        (Some(f), None) => DbResolution {
            path: PathBuf::from(f),
            source: DbSource::Flag,
            overridden: vec![],
        },
        (None, Some(e)) => DbResolution {
            path: PathBuf::from(e),
            source: DbSource::Env,
            overridden: vec![],
        },
        (None, None) => DbResolution {
            path: PathBuf::from(DEFAULT_DB),
            source: DbSource::Default,
            overridden: vec![],
        },
    }
}

// ---------------------------------------------------------------------------
// Serve resolution
// ---------------------------------------------------------------------------

/// Effective `serve` configuration after the CLI > env > file > default merge.
#[derive(Debug, Clone, PartialEq)]
pub struct ServeConfig {
    pub addr: SocketAddr,
    pub k: u64,
    pub repo_key: Option<String>,
    pub hash_domain: HashDomain,
    /// Fail-closed no-egress assertion (design §4, #190). When `true`, the
    /// server refuses to start the PTR follow-loop and logs a startup line
    /// confirming no outbound PTR sync will occur.
    pub no_egress: bool,
    /// Number of read-only SQLite connections in the round-robin pool (#202).
    /// Always in [1, 64].
    pub read_connections: u32,
    /// Serve-only mode (#202): when `true`, write endpoints return 403 and
    /// pooled read connections receive `PRAGMA query_only = ON` + 1 GiB mmap.
    pub read_only: bool,
    /// Optional display name advertised in `/repo/caps`. `None` when unset.
    pub name: Option<String>,
}

/// One cross-tier override warning: a setting present in both a higher and a
/// lower tier with differing values. Used for both serve and log resolutions.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingOverride {
    /// The setting name (e.g. `"serve.addr"` or `"log.level"`).
    pub setting: &'static str,
    /// Name of the tier whose value was used (the winner).
    pub winner_tier: &'static str,
    /// String representation of the winning value.
    pub winner_val: String,
    /// Name of the tier whose value was discarded (the loser).
    pub loser_tier: &'static str,
    /// String representation of the losing value.
    pub loser_val: String,
}

/// Resolution result for the `serve` configuration: effective settings plus
/// any cross-tier override warnings.
#[derive(Debug, Clone, PartialEq)]
pub struct ServeResolution {
    pub config: ServeConfig,
    /// The hash domain from the first tier that explicitly set it
    /// (`cli.or(env).or(file)`). `None` if no tier set it. Retained so a
    /// caller can tell an explicit `blake3` from the default `blake3` in
    /// diagnostics; it is no longer used to reject any configuration
    /// (ADR 0024 addendum 2026-07-27).
    pub explicit_hash_domain: Option<HashDomain>,
    /// Warnings to emit: settings where two explicitly-set tiers differed.
    pub overridden: Vec<SettingOverride>,
}

/// Newtype wrapper for `repo_key` values in the [`pick`] helper.
///
/// `Display` prints `(none)` for an empty string so cross-tier override
/// warnings show a human-readable sentinel instead of a blank field. An empty
/// `KeyVal` represents an explicit "no key" override; a non-empty one is the
/// key hex itself. WHY a newtype: `pick<T>` needs `T: Display` and the "(none)"
/// sentinel spelling must match the display, so the type carries the semantics.
#[derive(PartialEq)]
struct KeyVal(String);

impl Display for KeyVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("(none)")
        } else {
            f.write_str(&self.0)
        }
    }
}

/// Validate a trimmed log-level/filter string.
///
/// Hybrid rule (applied after trimming):
/// - **Bare word** (contains no `=`, `,`, or `::`): must be one of the six
///   known tracing levels, case-insensitively. Any other bare word (e.g.
///   `ifno`, `banana`) is a typo by construction for this setting and is
///   therefore invalid — an operator who wants target filtering writes
///   `target=level`.
/// - **Directive-shaped** (contains `=`, `,`, or `::`): validated with
///   `EnvFilter::try_new`; catches things like `naiad_server=deubg`.
fn is_valid_level(s: &str) -> bool {
    if !s.contains('=') && !s.contains(',') && !s.contains("::") {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "trace" | "debug" | "info" | "warn" | "error" | "off"
        )
    } else {
        EnvFilter::try_new(s).is_ok()
    }
}

/// Generic first-set-wins merge helper with cross-tier warning collection.
///
/// `flag` > `env` > `file`, all `Option`. The first `Some` value wins; every
/// lower tier that is `Some` and *differs* from the winner pushes a warning.
/// Example: `flag=A, env=B, file=C` → winner `A`, 2 warnings (env and file
/// both lost). All three tier names are caller-supplied so this function works
/// for both the serve ladder (`flag_tier = "CLI flag"`, `file_tier =
/// "repo.toml"`) and the log-level ladder (`flag_tier = "RUST_LOG env"`,
/// `env_tier = "NAIAD_REPO_LOG_LEVEL env"`, `file_tier = "repo.toml"`).
#[allow(clippy::too_many_arguments)] // 3-tier × (value + name) + setting + out; no clean grouping
fn pick<T: PartialEq + Display>(
    setting: &'static str,
    flag: Option<T>,
    env: Option<T>,
    file: Option<T>,
    flag_tier: &'static str,
    env_tier: &'static str,
    file_tier: &'static str,
    out: &mut Vec<SettingOverride>,
) -> Option<T> {
    if let Some(f) = flag {
        if let Some(ref e) = env {
            if *e != f {
                out.push(SettingOverride {
                    setting,
                    winner_tier: flag_tier,
                    winner_val: f.to_string(),
                    loser_tier: env_tier,
                    loser_val: e.to_string(),
                });
            }
        }
        if let Some(ref fi) = file {
            if *fi != f {
                out.push(SettingOverride {
                    setting,
                    winner_tier: flag_tier,
                    winner_val: f.to_string(),
                    loser_tier: file_tier,
                    loser_val: fi.to_string(),
                });
            }
        }
        Some(f)
    } else if let Some(e) = env {
        if let Some(ref fi) = file {
            if *fi != e {
                out.push(SettingOverride {
                    setting,
                    winner_tier: env_tier,
                    winner_val: e.to_string(),
                    loser_tier: file_tier,
                    loser_val: fi.to_string(),
                });
            }
        }
        Some(e)
    } else {
        file
    }
}

/// Merge explicit CLI flags, environment variables, file values, and built-in
/// defaults, per field. Returns the resolved configuration plus any cross-tier
/// override warnings.
///
/// `getenv` is injectable so tests never touch the real process environment
/// (mirrors the pattern [`resolve_bridge`] already uses). A malformed
/// `NAIAD_REPO_*` value that fails to parse is a fatal startup error: the
/// returned `Err` names the variable, the raw value, and the expected form.
///
/// `cli_no_egress` is `true` when the `--no-egress` flag was passed; `false`
/// when it was absent (the CLI flag can only assert `true`). The env var
/// `NAIAD_REPO_NO_EGRESS` and `[serve].no_egress` cover the `false` case.
///
/// # Errors
///
/// Returns `Err` if any `NAIAD_REPO_*` value cannot be parsed (`ADDR` as
/// `IP:port`, `K` as a non-negative integer, `HASH_DOMAIN` as `blake3` or
/// `sha256`, or `NO_EGRESS` with an invalid bool).
pub fn resolve_serve(
    cli_addr: Option<SocketAddr>,
    cli_k: Option<u64>,
    cli_repo_key: Option<String>,
    cli_hash_domain: Option<HashDomain>,
    cli_no_egress: bool,
    file: &ServeSettings,
    getenv: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<ServeResolution> {
    let mut overridden: Vec<SettingOverride> = Vec::new();

    // Parse env tiers (malformed = fatal, naming the variable).
    let env_addr: Option<SocketAddr> = match env_nonblank(&getenv, "NAIAD_REPO_ADDR") {
        Some(s) => Some(s.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "NAIAD_REPO_ADDR: invalid value {:?} (expected IP:port, e.g. 0.0.0.0:9090)",
                s
            )
        })?),
        None => None,
    };

    let env_k: Option<u64> = match env_nonblank(&getenv, "NAIAD_REPO_K") {
        Some(s) => Some(s.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "NAIAD_REPO_K: invalid value {:?} (expected non-negative integer)",
                s
            )
        })?),
        None => None,
    };

    let env_hash_domain: Option<HashDomain> = match env_nonblank(&getenv, "NAIAD_REPO_HASH_DOMAIN")
    {
        Some(s) => Some(parse_hash_domain_str(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "NAIAD_REPO_HASH_DOMAIN: invalid value {:?} (expected blake3 or sha256)",
                s
            )
        })?),
        None => None,
    };

    // NAIAD_REPO_READ_CONNECTIONS: malformed = fatal (like K). Clamp [1, 64]
    // after merge so file values out of range are silently clamped.
    let env_read_connections: Option<u32> =
        match env_nonblank(&getenv, "NAIAD_REPO_READ_CONNECTIONS") {
            Some(s) => Some(s.trim().parse().map_err(|_| {
                anyhow::anyhow!(
                    "NAIAD_REPO_READ_CONNECTIONS: invalid value {:?} (expected positive integer)",
                    s
                )
            })?),
            None => None,
        };

    // NAIAD_REPO_NO_EGRESS: parse as bool; malformed = fatal. A blank/absent env
    // value falls through to file.no_egress.
    let env_no_egress: Option<bool> = match env_present(&getenv, "NAIAD_REPO_NO_EGRESS") {
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(parse_env_bool(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "NAIAD_REPO_NO_EGRESS: invalid value {:?} \
                 (expected true/false, 1/0, yes/no, on/off)",
                s.trim()
            )
        })?),
        None => None,
    };

    // NAIAD_REPO_READ_ONLY: parse as bool; malformed = fatal (like no_egress).
    // A blank/absent env value falls through to file.read_only.
    let env_read_only: Option<bool> = match env_present(&getenv, "NAIAD_REPO_READ_ONLY") {
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(parse_env_bool(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "NAIAD_REPO_READ_ONLY: invalid value {:?} \
                 (expected true/false, 1/0, yes/no, on/off)",
                s.trim()
            )
        })?),
        None => None,
    };

    // NAIAD_REPO_KEY: trim before the emptiness test — whitespace-only = explicit "none".
    let env_key: Option<KeyVal> =
        env_present(&getenv, "NAIAD_REPO_KEY").map(|s| KeyVal(s.trim().to_owned()));

    // Resolve addr, k, hash_domain via the generic pick helper (section-qualified names).
    let addr = pick(
        "serve.addr",
        cli_addr,
        env_addr,
        file.addr,
        "CLI flag",
        "NAIAD_REPO_ADDR env",
        "repo.toml",
        &mut overridden,
    )
    .unwrap_or_else(|| DEFAULT_ADDR.parse().expect("DEFAULT_ADDR parses"));

    let k = pick(
        "serve.k",
        cli_k,
        env_k,
        file.k,
        "CLI flag",
        "NAIAD_REPO_K env",
        "repo.toml",
        &mut overridden,
    )
    .unwrap_or(DEFAULT_K);

    let explicit_hash_domain = pick(
        "serve.hash_domain",
        cli_hash_domain,
        env_hash_domain,
        file.hash_domain,
        "CLI flag",
        "NAIAD_REPO_HASH_DOMAIN env",
        "repo.toml",
        &mut overridden,
    );
    let hash_domain = explicit_hash_domain.unwrap_or_default();

    // repo_key via KeyVal newtype so Display prints "(none)" for empty strings.
    // Empty env (including whitespace-only after trim) = explicit "none", beats file.
    let cli_key: Option<KeyVal> = cli_repo_key.map(KeyVal);
    let file_key: Option<KeyVal> = file.repo_key.clone().map(KeyVal);

    let winner_key = pick(
        "serve.repo_key",
        cli_key,
        env_key,
        file_key,
        "CLI flag",
        "NAIAD_REPO_KEY env",
        "repo.toml",
        &mut overridden,
    );
    // Empty KeyVal = explicit "none" (no key advertised).
    let repo_key = winner_key.map(|kv| kv.0).filter(|s| !s.is_empty());

    // NAIAD_REPO_NAME: trim before the emptiness test — whitespace-only = explicit "none".
    let env_name: Option<KeyVal> =
        env_present(&getenv, "NAIAD_REPO_NAME").map(|s| KeyVal(s.trim().to_owned()));
    let file_name: Option<KeyVal> = file.name.clone().map(|s| KeyVal(s.trim().to_owned()));
    let winner_name = pick(
        "serve.name",
        None::<KeyVal>, // no CLI flag: deployment knob, like read_connections
        env_name,
        file_name,
        "CLI flag",
        "NAIAD_REPO_NAME env",
        "repo.toml",
        &mut overridden,
    );
    let name = winner_name.map(|kv| kv.0).filter(|s| !s.is_empty());

    // no_egress: CLI flag (`--no-egress`) is a presence-only assertion; it
    // maps to Some(true) when present and None when absent (the flag can
    // never express "false" — that is the default). Env and file cover the
    // full bool range.
    let cli_no_egress_opt: Option<bool> = if cli_no_egress { Some(true) } else { None };
    let no_egress = pick(
        "serve.no_egress",
        cli_no_egress_opt,
        env_no_egress,
        file.no_egress,
        "CLI flag",
        "NAIAD_REPO_NO_EGRESS env",
        "repo.toml",
        &mut overridden,
    )
    .unwrap_or(false);

    // read_connections: env > file > default(4), clamped [1, 64].
    // No CLI flag: the pool size is an infrastructure knob, not a per-run
    // override. Malformed env is fatal (like K).
    let read_connections = env_read_connections
        .or(file.read_connections)
        .unwrap_or(DEFAULT_READ_CONNECTIONS)
        .clamp(1, 64);

    // read_only: env > file > default(false). No CLI flag.
    let read_only = env_read_only
        .or(file.read_only)
        .unwrap_or(DEFAULT_READ_ONLY);

    Ok(ServeResolution {
        config: ServeConfig {
            addr,
            k,
            repo_key,
            hash_domain,
            no_egress,
            read_connections,
            read_only,
            name,
        },
        explicit_hash_domain,
        overridden,
    })
}

// ---------------------------------------------------------------------------
// Log resolution
// ---------------------------------------------------------------------------

/// Effective log configuration after the env > file > default merge.
/// Passed to `init_tracing`; level and console are already resolved to
/// concrete values.
#[derive(Debug, Clone, PartialEq)]
pub struct LogConfig {
    /// Resolved log filter directive (never empty).
    pub level: String,
    /// Whether to emit to stderr.
    pub console: bool,
    /// Optional append-mode file sink path.
    pub file: Option<String>,
}

/// Resolution result for log configuration: effective settings plus any
/// cross-tier override warnings.
#[derive(Debug, Clone, PartialEq)]
pub struct LogResolution {
    pub config: LogConfig,
    /// Warnings to emit: settings where two explicitly-set tiers differed.
    pub overridden: Vec<SettingOverride>,
    /// Non-fatal warnings that don't fit the cross-tier model — e.g. an
    /// unrecognised plain-level value in `RUST_LOG` or `repo.toml` that was
    /// ignored and fell through to the next tier. Emitted via
    /// `tracing::warn!(target: "startup", ...)` after `init_tracing`.
    pub extra_warnings: Vec<String>,
}

/// Merge log settings: env > file > built-in defaults. Level ladder is
/// `RUST_LOG` → `NAIAD_REPO_LOG_LEVEL` → `[log].level` → `"info"`.
/// Empty/blank values are absent at every rung; non-blank values are trimmed
/// before validation so `"debug "` and `"debug"` are equivalent.
///
/// `getenv` is injectable so tests never touch the real process environment.
/// Level values are validated with [`is_valid_level`] (hybrid rule: bare
/// words must be one of the six tracing levels; directive-shaped values are
/// checked via `EnvFilter::try_new`). A `NAIAD_REPO_LOG_CONSOLE` value that
/// cannot be parsed as a bool is a fatal startup error. A
/// `NAIAD_REPO_LOG_LEVEL` value that fails `is_valid_level` is also fatal.
/// `RUST_LOG` and `[log].level` with the same problem warn and fall through
/// to the next tier (both are documented as accepting full directives — a
/// typo there must not cause a fatal error in production deployments).
///
/// # Errors
///
/// Returns `Err` if `NAIAD_REPO_LOG_CONSOLE` cannot be parsed as a bool, or
/// if `NAIAD_REPO_LOG_LEVEL` fails level validation. Both errors name the
/// variable and the bad value.
pub fn resolve_log(
    file: &LogSettings,
    getenv: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<LogResolution> {
    let mut overridden: Vec<SettingOverride> = Vec::new();
    let mut extra_warnings: Vec<String> = Vec::new();

    // --- level ---
    // Tier ladder: RUST_LOG → NAIAD_REPO_LOG_LEVEL → [log].level → DEFAULT_LOG_LEVEL.
    // All values are trimmed before validation and use; whitespace-padded level strings
    // ("debug ") are equivalent to their trimmed form ("debug").
    let rust_log_raw = env_nonblank(&getenv, "RUST_LOG").map(|s| s.trim().to_owned());
    let env_level_raw = env_nonblank(&getenv, "NAIAD_REPO_LOG_LEVEL").map(|s| s.trim().to_owned());
    let file_level_raw = file.level.as_deref().and_then(|s| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        }
    });

    // NAIAD_REPO_LOG_LEVEL: fatal if the value fails the hybrid level check.
    if let Some(ref el) = env_level_raw {
        if !is_valid_level(el) {
            return Err(anyhow::anyhow!(
                "NAIAD_REPO_LOG_LEVEL: invalid value {:?} \
                 (bare values must be one of trace|debug|info|warn|error|off; \
                 for per-target filtering use 'target=level' form, \
                 e.g. 'naiad_server=debug,info')",
                el
            ));
        }
    }

    // RUST_LOG and [log].level: warn-and-continue when the value fails the
    // hybrid level check. Both tiers are documented as accepting full
    // directives; a typo there must not cause a fatal error in production.
    let rust_log = match rust_log_raw {
        Some(ref rl) if !is_valid_level(rl) => {
            extra_warnings.push(format!(
                "log.level: RUST_LOG env value {:?} is not a valid level or filter \
                 directive; bare values must be trace/debug/info/warn/error/off, \
                 or use 'target=level' form (e.g. 'naiad_server=debug') \
                 — ignoring RUST_LOG",
                rl
            ));
            None
        }
        other => other,
    };
    let file_level = match file_level_raw {
        Some(ref fl) if !is_valid_level(fl) => {
            extra_warnings.push(format!(
                "log.level: repo.toml value {:?} is not a valid level or filter \
                 directive; bare values must be trace/debug/info/warn/error/off, \
                 or use 'target=level' form (e.g. 'naiad_server=debug') \
                 — ignoring file value",
                fl
            ));
            None
        }
        other => other,
    };
    let env_level = env_level_raw;

    // Three-tier pick: RUST_LOG → NAIAD_REPO_LOG_LEVEL → [log].level → default.
    let level = pick(
        "log.level",
        rust_log,
        env_level,
        file_level,
        "RUST_LOG env",
        "NAIAD_REPO_LOG_LEVEL env",
        "repo.toml",
        &mut overridden,
    )
    .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned());

    // --- console ---
    // Tier ladder: NAIAD_REPO_LOG_CONSOLE (parse_env_bool) → file.console → DEFAULT_CONSOLE.
    let console = match env_nonblank(&getenv, "NAIAD_REPO_LOG_CONSOLE") {
        Some(s) => parse_env_bool(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "NAIAD_REPO_LOG_CONSOLE: invalid value {:?} \
                 (expected true/false, 1/0, yes/no, on/off)",
                s
            )
        })?,
        None => file.console.unwrap_or(DEFAULT_CONSOLE),
    };

    // --- log file ---
    // Tier ladder: NAIAD_REPO_LOG_FILE via env_present (trimmed; empty or
    // whitespace-only = explicit "none", beats file setting) → file.file → None.
    let log_file: Option<String> = match env_present(&getenv, "NAIAD_REPO_LOG_FILE") {
        Some(v) => {
            let v = v.trim().to_owned();
            if v.is_empty() { None } else { Some(v) }
        }
        None => file.file.clone(),
    };

    Ok(LogResolution {
        config: LogConfig {
            level,
            console,
            file: log_file,
        },
        overridden,
        extra_warnings,
    })
}

// ---------------------------------------------------------------------------
// Bridge resolution (unchanged semantics; refactored to use parse_env_bool)
// ---------------------------------------------------------------------------

/// Effective `[bridge]` configuration after the env > file > default merge.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeConfig {
    pub enabled: bool,
    pub mode: BridgeMode,
    pub snapshot_dir: Option<String>,
    pub snapshot_service_id: Option<i64>,
    pub max_query_bits: u32,
    pub min_query_bits: u32,
    pub ptr_url: String,
    pub ptr_key: String,
    pub state_db: String,
}

/// Resolve `[bridge]` with precedence env > file > default. `getenv` is
/// injectable so tests never touch the real environment (mirrors the pattern
/// the former `bridge/config.rs::resolve` used).
///
/// **Env keys (all prefixed `NAIAD_REPO_BRIDGE_`):**
/// - `ENABLED` — bool (`true`/`1`/`yes`/`on` or `false`/`0`/`no`/`off`); malformed → warn, fall back to file.
/// - `PTR_URL`, `PTR_KEY`, `STATE_DB` — use blank-means-unset semantics (`env_nonblank`): a blank
///   or whitespace-only value falls through to the file/default silently. A non-blank value still
///   overrides as before.
/// - `MODE`, `SNAPSHOT_DIR`, `SNAPSHOT_SERVICE_ID`, `MAX_QUERY_BITS` — use blank-means-unset
///   semantics (`env_nonblank`): a blank or whitespace-only value is treated as absent, falling
///   through to the file/default. A non-blank but malformed value (bad mode name, non-integer)
///   logs a `warn` and falls back to the file/default, matching the `ENABLED` behaviour.
#[must_use]
pub fn resolve_bridge(
    file: &BridgeSettings,
    getenv: impl Fn(&str) -> Option<String>,
) -> BridgeConfig {
    let enabled = match getenv("NAIAD_REPO_BRIDGE_ENABLED") {
        Some(s) => match parse_env_bool(&s) {
            Some(b) => b,
            None => {
                tracing::warn!(
                    target: "startup",
                    variable = "NAIAD_REPO_BRIDGE_ENABLED",
                    value = %s.trim(),
                    "invalid bool in env var; using file/default"
                );
                file.enabled
            }
        },
        None => file.enabled,
    };
    let ptr_url = env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_PTR_URL")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| file.ptr_url.clone());
    let ptr_key = env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_PTR_KEY")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| file.ptr_key.clone());
    let state_db = env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_STATE_DB")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| file.state_db.clone());
    // Remaining keys use `env_nonblank` (blank = unset). Malformed values warn
    // and fall back to the file/default, matching `enabled`.
    let mode = match env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_MODE") {
        Some(s) => match parse_bridge_mode_str(&s) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    target: "startup",
                    variable = "NAIAD_REPO_BRIDGE_MODE",
                    value = %s.trim(),
                    "invalid bridge mode in env var (expected mirror, snapshot, or sidecar); \
                     using file/default"
                );
                file.mode
            }
        },
        None => file.mode,
    };
    let snapshot_dir = env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_SNAPSHOT_DIR")
        .map(|s| s.trim().to_owned())
        .or_else(|| file.snapshot_dir.as_deref().map(|s| s.trim().to_owned()));
    let snapshot_service_id = match env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID") {
        Some(s) => match s.trim().parse::<i64>() {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(
                    target: "startup",
                    variable = "NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID",
                    value = %s.trim(),
                    "invalid integer in env var; using file/default"
                );
                file.snapshot_service_id
            }
        },
        None => file.snapshot_service_id,
    };
    let max_query_bits = match env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_MAX_QUERY_BITS") {
        Some(s) => match s.trim().parse::<u32>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    target: "startup",
                    variable = "NAIAD_REPO_BRIDGE_MAX_QUERY_BITS",
                    value = %s.trim(),
                    "invalid integer in env var; using file/default"
                );
                file.max_query_bits
            }
        },
        None => file.max_query_bits,
    };
    let min_query_bits = match env_nonblank(&getenv, "NAIAD_REPO_BRIDGE_MIN_QUERY_BITS") {
        Some(s) => match s.trim().parse::<u32>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    target: "startup",
                    variable = "NAIAD_REPO_BRIDGE_MIN_QUERY_BITS",
                    value = %s.trim(),
                    "invalid integer in env var; using file/default"
                );
                file.min_query_bits
            }
        },
        None => file.min_query_bits,
    };
    BridgeConfig {
        enabled,
        mode,
        snapshot_dir,
        snapshot_service_id,
        max_query_bits,
        min_query_bits,
        ptr_url,
        ptr_key,
        state_db,
    }
}

// ---------------------------------------------------------------------------
// Stats resolution
// ---------------------------------------------------------------------------

/// Effective `[stats]` configuration after the env > file > default merge.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsConfig {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub allow_non_loopback: bool,
    /// Resolved `stats.db` path (already `resolve_beside_db`-applied).
    pub db_path: PathBuf,
}

/// Resolve `[stats]` with precedence env > file > default. `getenv` is
/// injectable so tests never touch the real environment. A malformed
/// `NAIAD_REPO_STATS_*` value is a fatal `Err` naming the variable, matching
/// the `resolve_serve` pattern (malformed = fatal, not warn-and-fallback).
/// Enforces the loopback guard: a non-loopback `listen` without
/// `allow_non_loopback` is fatal (unless `enabled = false`, which skips the
/// guard entirely).
///
/// # Errors
/// Returns `Err` on a malformed `LISTEN`/`ENABLED`/`ALLOW_NON_LOOPBACK` value,
/// or when the loopback guard trips.
pub fn resolve_stats(
    file: &StatsSettings,
    db_path: &Path,
    getenv: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<StatsConfig> {
    let enabled = match env_present(&getenv, "NAIAD_REPO_STATS_ENABLED") {
        Some(s) if s.trim().is_empty() => file.enabled.unwrap_or(DEFAULT_STATS_ENABLED),
        Some(s) => parse_env_bool(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "NAIAD_REPO_STATS_ENABLED: invalid value {:?} \
                 (expected true/false, 1/0, yes/no, on/off)",
                s.trim()
            )
        })?,
        None => file.enabled.unwrap_or(DEFAULT_STATS_ENABLED),
    };

    let listen: SocketAddr = match env_nonblank(&getenv, "NAIAD_REPO_STATS_LISTEN") {
        Some(s) => s.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "NAIAD_REPO_STATS_LISTEN: invalid value {:?} (expected IP:port, e.g. 127.0.0.1:9092)",
                s
            )
        })?,
        None => file
            .listen
            .unwrap_or_else(|| DEFAULT_STATS_LISTEN.parse().expect("DEFAULT_STATS_LISTEN parses")),
    };

    let allow_non_loopback = match env_present(&getenv, "NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK") {
        Some(s) if s.trim().is_empty() => file
            .allow_non_loopback
            .unwrap_or(DEFAULT_STATS_ALLOW_NON_LOOPBACK),
        Some(s) => parse_env_bool(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK: invalid value {:?} \
                 (expected true/false, 1/0, yes/no, on/off)",
                s.trim()
            )
        })?,
        None => file
            .allow_non_loopback
            .unwrap_or(DEFAULT_STATS_ALLOW_NON_LOOPBACK),
    };

    let db_raw = env_nonblank(&getenv, "NAIAD_REPO_STATS_DB")
        .map(|s| s.trim().to_owned())
        .or_else(|| file.db_path.clone())
        .unwrap_or_else(|| DEFAULT_STATS_DB.to_owned());
    let resolved_db = resolve_beside_db(db_path, &db_raw);

    // Loopback guard: a stats port that is accidentally world-reachable is
    // exactly the failure this feature must not have. Skip entirely when disabled.
    if enabled && !listen.ip().is_loopback() && !allow_non_loopback {
        anyhow::bail!(
            "[stats].listen {listen} is not a loopback address; the stats port must bind \
             127.0.0.0/8 or ::1. Set [stats].allow_non_loopback = true \
             (NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK) only if you deliberately front it \
             behind your own access control."
        );
    }

    Ok(StatsConfig {
        enabled,
        listen,
        allow_non_loopback,
        db_path: resolved_db,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn settings_path_is_beside_db() {
        let p = settings_path_for(Path::new("/srv/naiad/repo.db"));
        assert_eq!(p, PathBuf::from("/srv/naiad/repo.toml"));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let d = dir();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("load");
        assert_eq!(s, RepoSettings::default());
        assert_eq!(s.serve.addr, None);
        assert_eq!(s.serve.k, None);
        assert_eq!(s.log.level, None);
        assert_eq!(s.log.console, None);
        assert_eq!(s.log.file, None);
        assert!(ignored.is_empty(), "default settings have no unknown keys");
    }

    #[test]
    fn full_file_parses() {
        let d = dir();
        std::fs::write(
            d.path().join("repo.toml"),
            "[serve]\naddr = \"0.0.0.0:8080\"\nk = 500\nrepo_key = \"ab\"\n\
             no_egress = true\n\
             [log]\nlevel = \"debug\"\nconsole = false\nfile = \"repo.log\"\n",
        )
        .unwrap();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("load");
        assert_eq!(s.serve.addr, Some("0.0.0.0:8080".parse().unwrap()));
        assert_eq!(s.serve.k, Some(500));
        assert_eq!(s.serve.repo_key.as_deref(), Some("ab"));
        assert_eq!(s.serve.no_egress, Some(true));
        assert_eq!(s.log.level.as_deref(), Some("debug"));
        assert_eq!(s.log.console, Some(false));
        assert_eq!(s.log.file.as_deref(), Some("repo.log"));
        assert!(ignored.is_empty(), "no unknown keys in full valid file");
    }

    #[test]
    fn partial_file_fills_defaults() {
        let d = dir();
        std::fs::write(d.path().join("repo.toml"), "[serve]\nk = 250\n").unwrap();
        let (s, _ignored) = load(&d.path().join("repo.db")).expect("load");
        assert_eq!(s.serve.k, Some(250));
        assert_eq!(s.serve.addr, None);
        assert_eq!(s.log, LogSettings::default());
    }

    #[test]
    fn unknown_keys_are_tolerated_and_reported() {
        let d = dir();
        std::fs::write(
            d.path().join("repo.toml"),
            "[serve]\nk = 7\n[future]\nshiny = true\n",
        )
        .unwrap();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("load");
        assert_eq!(s.serve.k, Some(7));
        assert!(
            !ignored.is_empty(),
            "unknown [future] section must be reported"
        );
    }

    #[test]
    fn malformed_file_is_fatal_and_names_path() {
        let d = dir();
        std::fs::write(d.path().join("repo.toml"), "[serve\nk = ").unwrap();
        let err = load(&d.path().join("repo.db")).expect_err("must fail");
        assert!(format!("{err:#}").contains("repo.toml"));
    }

    #[test]
    fn scaffold_writes_once_and_roundtrips_to_defaults() {
        let d = dir();
        let db = d.path().join("repo.db");
        ensure_scaffold(&db);
        let path = settings_path_for(&db);
        assert!(path.exists());
        // Template parses and is equivalent to the built-in defaults once merged.
        let (s, ignored) = load(&db).expect("template parses");
        let resolution = resolve_serve(None, None, None, None, false, &s.serve, |_| None)
            .expect("resolve_serve on scaffold");
        let cfg = resolution.config;
        assert_eq!(cfg.addr, DEFAULT_ADDR.parse().unwrap());
        assert_eq!(cfg.k, DEFAULT_K);
        assert_eq!(cfg.repo_key, None);
        assert_eq!(s.log, LogSettings::default());
        assert!(ignored.is_empty(), "scaffold produces no unknown keys");
        // Never clobbers: a hand-edited file survives a second call.
        std::fs::write(&path, "[serve]\nk = 42\n").unwrap();
        ensure_scaffold(&db);
        assert_eq!(load(&db).unwrap().0.serve.k, Some(42));
    }

    #[test]
    fn scaffold_parses_to_all_defaults() {
        let d = dir();
        std::fs::write(d.path().join("repo.toml"), SCAFFOLD).unwrap();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("scaffold parses");
        assert_eq!(
            s,
            RepoSettings::default(),
            "scaffold must parse to defaults"
        );
        assert!(ignored.is_empty(), "scaffold has no unknown keys");
    }

    // --- resolve_db tests ---

    #[test]
    fn resolve_db_flag_beats_env_beats_default() {
        // Flag wins over env and default.
        let r = resolve_db(Some("flag.db"), Some("env.db"));
        assert_eq!(r.path, PathBuf::from("flag.db"));
        assert_eq!(r.source, DbSource::Flag);
        assert_eq!(r.overridden.len(), 1);
        assert_eq!(r.overridden[0], (DbSource::Env, "env.db".to_string()));

        // Env wins when no flag.
        let r2 = resolve_db(None, Some("env.db"));
        assert_eq!(r2.path, PathBuf::from("env.db"));
        assert_eq!(r2.source, DbSource::Env);
        assert!(r2.overridden.is_empty());

        // Default when neither set.
        let r3 = resolve_db(None, None);
        assert_eq!(r3.path, PathBuf::from(DEFAULT_DB));
        assert_eq!(r3.source, DbSource::Default);
        assert!(r3.overridden.is_empty());
    }

    #[test]
    fn resolve_db_empty_values_are_absent() {
        // Empty flag treated as absent.
        let r = resolve_db(Some(""), Some("env.db"));
        assert_eq!(r.source, DbSource::Env);
        assert_eq!(r.path, PathBuf::from("env.db"));

        // Empty env treated as absent.
        let r2 = resolve_db(None, Some(""));
        assert_eq!(r2.source, DbSource::Default);
    }

    #[test]
    fn resolve_db_no_warning_when_tiers_agree_or_default() {
        // Same values → no warning.
        let r = resolve_db(Some("same.db"), Some("same.db"));
        assert!(r.overridden.is_empty(), "agreeing tiers must not warn");

        // Default is never in overridden.
        let r2 = resolve_db(Some("a.db"), None);
        assert!(r2.overridden.is_empty());
    }

    // --- resolve_serve tests (existing, updated for new signature) ---

    #[test]
    fn resolve_serve_precedence_cli_over_file_over_default() {
        let file = ServeSettings {
            addr: Some("10.0.0.1:1000".parse().unwrap()),
            k: Some(500),
            repo_key: Some("filekey".into()),
            hash_domain: None,
            ..ServeSettings::default()
        };
        // CLI beats file.
        let resolution = resolve_serve(
            Some("127.0.0.1:2000".parse().unwrap()),
            Some(9),
            Some("clikey".into()),
            None,
            false,
            &file,
            |_| None,
        )
        .expect("resolve");
        let cfg = &resolution.config;
        assert_eq!(cfg.addr, "127.0.0.1:2000".parse().unwrap());
        assert_eq!(cfg.k, 9);
        assert_eq!(cfg.repo_key.as_deref(), Some("clikey"));
        // File beats default.
        let resolution =
            resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        let cfg = &resolution.config;
        assert_eq!(cfg.addr, "10.0.0.1:1000".parse().unwrap());
        assert_eq!(cfg.k, 500);
        assert_eq!(cfg.repo_key.as_deref(), Some("filekey"));
        // Default when both absent.
        let resolution = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        let cfg = &resolution.config;
        assert_eq!(cfg.addr, DEFAULT_ADDR.parse().unwrap());
        assert_eq!(cfg.k, DEFAULT_K);
        assert_eq!(cfg.repo_key, None);
    }

    #[test]
    fn resolve_serve_override_warning_when_tiers_differ() {
        let file = ServeSettings {
            addr: Some("10.0.0.1:1000".parse().unwrap()),
            k: Some(500),
            repo_key: None,
            hash_domain: None,
            ..ServeSettings::default()
        };
        let resolution = resolve_serve(
            Some("127.0.0.1:2000".parse().unwrap()),
            Some(9),
            None,
            None,
            false,
            &file,
            |_| None,
        )
        .expect("resolve");
        assert_eq!(resolution.overridden.len(), 2, "addr and k both differ");
        let addr_warn = resolution
            .overridden
            .iter()
            .find(|o| o.setting == "serve.addr")
            .unwrap();
        assert_eq!(addr_warn.winner_tier, "CLI flag");
        assert_eq!(addr_warn.loser_tier, "repo.toml");
        let k_warn = resolution
            .overridden
            .iter()
            .find(|o| o.setting == "serve.k")
            .unwrap();
        assert_eq!(k_warn.winner_val, "9");
    }

    #[test]
    fn resolve_serve_no_override_when_tiers_agree() {
        let addr: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        let file = ServeSettings {
            addr: Some(addr),
            k: Some(1000),
            repo_key: None,
            hash_domain: None,
            ..ServeSettings::default()
        };
        let resolution = resolve_serve(Some(addr), Some(1000), None, None, false, &file, |_| None)
            .expect("resolve");
        assert!(
            resolution.overridden.is_empty(),
            "agreeing tiers must not warn"
        );
    }

    #[test]
    fn resolve_serve_hash_domain_file_and_default() {
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        assert_eq!(r.config.hash_domain, HashDomain::Blake3);
        assert_eq!(r.explicit_hash_domain, None);
        let file = ServeSettings {
            hash_domain: Some(HashDomain::Sha256),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert_eq!(r.config.hash_domain, HashDomain::Sha256);
        let r = resolve_serve(
            None,
            None,
            None,
            Some(HashDomain::Blake3),
            false,
            &file,
            |_| None,
        )
        .expect("resolve");
        assert_eq!(r.config.hash_domain, HashDomain::Blake3);
    }

    // --- new resolve_serve tests ---

    #[test]
    fn resolve_serve_env_beats_file() {
        let file = ServeSettings {
            addr: Some("10.0.0.1:1000".parse().unwrap()),
            k: Some(500),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| match k {
            "NAIAD_REPO_ADDR" => Some("10.0.0.2:2000".to_string()),
            "NAIAD_REPO_K" => Some("999".to_string()),
            _ => None,
        })
        .expect("resolve");
        assert_eq!(r.config.addr, "10.0.0.2:2000".parse().unwrap());
        assert_eq!(r.config.k, 999);
        // Warnings: env beats file for both.
        assert_eq!(r.overridden.len(), 2);
    }

    #[test]
    fn resolve_serve_flag_beats_env() {
        let r = resolve_serve(
            Some("127.0.0.1:3000".parse().unwrap()),
            Some(777),
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| match k {
                "NAIAD_REPO_ADDR" => Some("0.0.0.0:4000".to_string()),
                "NAIAD_REPO_K" => Some("888".to_string()),
                _ => None,
            },
        )
        .expect("resolve");
        assert_eq!(r.config.addr, "127.0.0.1:3000".parse().unwrap());
        assert_eq!(r.config.k, 777);
        // Warnings: CLI beats env for both.
        assert_eq!(r.overridden.len(), 2);
        assert!(r.overridden.iter().all(|o| o.winner_tier == "CLI flag"));
    }

    #[test]
    fn resolve_serve_env_default_looking_value_beats_file() {
        // Even when env value == DEFAULT_K (1000), it beats file k=500.
        let file = ServeSettings {
            k: Some(500),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_K").then(|| "1000".to_string())
        })
        .expect("resolve");
        assert_eq!(r.config.k, 1000);
        // Warning: env "1000" beats file "500".
        assert_eq!(r.overridden.len(), 1);
        let w = &r.overridden[0];
        assert_eq!(w.setting, "serve.k");
        assert_eq!(w.winner_tier, "NAIAD_REPO_K env");
    }

    #[test]
    fn resolve_serve_warns_once_per_losing_tier() {
        // CLI + env + file all different on addr → 2 overrides.
        let file = ServeSettings {
            addr: Some("10.0.0.1:1000".parse().unwrap()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(
            Some("127.0.0.1:3000".parse().unwrap()),
            None,
            None,
            None,
            false,
            &file,
            |k| (k == "NAIAD_REPO_ADDR").then(|| "0.0.0.0:2000".to_string()),
        )
        .expect("resolve");
        let addr_warns: Vec<_> = r
            .overridden
            .iter()
            .filter(|o| o.setting == "serve.addr")
            .collect();
        assert_eq!(addr_warns.len(), 2, "env and file both lose to CLI");
        let loser_tiers: Vec<_> = addr_warns.iter().map(|o| o.loser_tier).collect();
        assert!(loser_tiers.contains(&"NAIAD_REPO_ADDR env"));
        assert!(loser_tiers.contains(&"repo.toml"));
    }

    #[test]
    fn resolve_serve_empty_repo_key_env_means_none() {
        // Empty NAIAD_REPO_KEY beats the file repo_key value and resolves to None.
        let file = ServeSettings {
            repo_key: Some("filekey".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_KEY").then(|| "".to_string())
        })
        .expect("resolve");
        assert_eq!(r.config.repo_key, None, "empty env = explicit none");
        // Warning: env (none) beat file "filekey".
        let kw = r.overridden.iter().find(|o| o.setting == "serve.repo_key");
        assert!(kw.is_some());
        let kw = kw.unwrap();
        assert_eq!(kw.winner_val, "(none)");
        assert_eq!(kw.loser_val, "filekey");
    }

    // --- serve.name / NAIAD_REPO_NAME tests ---

    #[test]
    fn resolve_serve_name_env_beats_file_and_warns() {
        let file = ServeSettings {
            name: Some("FileRepo".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_NAME").then(|| "EnvRepo".to_string())
        })
        .expect("resolve");
        assert_eq!(r.config.name.as_deref(), Some("EnvRepo"));
        let w = r.overridden.iter().find(|o| o.setting == "serve.name");
        assert!(w.is_some(), "must have a serve.name override warning");
        let w = w.unwrap();
        assert_eq!(w.winner_tier, "NAIAD_REPO_NAME env");
        assert_eq!(w.loser_tier, "repo.toml");
    }

    #[test]
    fn resolve_serve_name_blank_env_is_unset() {
        // Whitespace-only env value = explicit "none", beats file value.
        let file = ServeSettings {
            name: Some("FileRepo".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_NAME").then(|| "  ".to_string())
        })
        .expect("resolve");
        assert_eq!(r.config.name, None, "whitespace-only env = explicit none");
        // Warning: env (none) beat file "FileRepo".
        let w = r.overridden.iter().find(|o| o.setting == "serve.name");
        assert!(w.is_some(), "must have override warning");
        let w = w.unwrap();
        assert_eq!(w.winner_val, "(none)");
        assert_eq!(w.loser_val, "FileRepo");
    }

    #[test]
    fn resolve_serve_name_file_alone_works() {
        let file = ServeSettings {
            name: Some("MyRepo".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert_eq!(r.config.name.as_deref(), Some("MyRepo"));
        assert!(
            r.overridden.iter().all(|o| o.setting != "serve.name"),
            "no warning when only one tier sets name"
        );
    }

    #[test]
    fn resolve_serve_name_absent_everywhere_is_none() {
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        assert_eq!(r.config.name, None);
    }

    #[test]
    fn resolve_serve_name_env_is_trimmed() {
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_NAME").then(|| " NOS ".to_string()),
        )
        .expect("resolve");
        assert_eq!(r.config.name.as_deref(), Some("NOS"));
    }

    #[test]
    fn resolve_serve_name_file_is_trimmed() {
        let file = ServeSettings {
            name: Some("  NOS  ".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert_eq!(
            r.config.name.as_deref(),
            Some("NOS"),
            "file name must be trimmed"
        );
    }

    #[test]
    fn resolve_serve_name_file_whitespace_only_is_none() {
        let file = ServeSettings {
            name: Some("   ".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert_eq!(
            r.config.name, None,
            "whitespace-only file name must resolve to None"
        );
    }

    #[test]
    fn resolve_serve_malformed_env_is_fatal() {
        // Bad NAIAD_REPO_ADDR.
        let err = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_ADDR").then(|| "not-an-addr".to_string()),
        )
        .expect_err("malformed ADDR must fail");
        assert!(
            format!("{err:#}").contains("NAIAD_REPO_ADDR"),
            "error must name the variable"
        );

        // Bad NAIAD_REPO_K.
        let err = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_K").then(|| "not-a-number".to_string()),
        )
        .expect_err("malformed K must fail");
        assert!(format!("{err:#}").contains("NAIAD_REPO_K"));

        // Bad NAIAD_REPO_HASH_DOMAIN.
        let err = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_HASH_DOMAIN").then(|| "md5".to_string()),
        )
        .expect_err("malformed HASH_DOMAIN must fail");
        assert!(format!("{err:#}").contains("NAIAD_REPO_HASH_DOMAIN"));
    }

    #[test]
    fn read_connections_resolves_and_clamps() {
        // file Some(8) → 8
        let file = ServeSettings {
            read_connections: Some(8),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert_eq!(r.config.read_connections, 8);

        // default → DEFAULT_READ_CONNECTIONS (4)
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        assert_eq!(r.config.read_connections, DEFAULT_READ_CONNECTIONS);

        // env "999" → clamped 64
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_READ_CONNECTIONS").then(|| "999".to_string()),
        )
        .expect("resolve");
        assert_eq!(r.config.read_connections, 64);

        // env beats file: env "2" over file 8 → 2
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_READ_CONNECTIONS").then(|| "2".to_string())
        })
        .expect("resolve");
        assert_eq!(r.config.read_connections, 2);

        // malformed env is fatal
        let err = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_READ_CONNECTIONS").then(|| "nope".to_string()),
        )
        .expect_err("malformed read_connections must fail");
        assert!(
            format!("{err:#}").contains("NAIAD_REPO_READ_CONNECTIONS"),
            "error must name the variable"
        );
    }

    #[test]
    fn resolve_serve_reports_explicit_hash_domain() {
        // None when nothing set.
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        assert_eq!(r.explicit_hash_domain, None);

        // Some when CLI sets it.
        let r = resolve_serve(
            None,
            None,
            None,
            Some(HashDomain::Sha256),
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        assert_eq!(r.explicit_hash_domain, Some(HashDomain::Sha256));

        // Some when env sets it. Use sha256 (not the default blake3) so the
        // assertion pins the env tier end-to-end rather than matching the default.
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_HASH_DOMAIN").then(|| "sha256".to_string()),
        )
        .expect("resolve");
        assert_eq!(r.explicit_hash_domain, Some(HashDomain::Sha256));
        assert_eq!(
            r.config.hash_domain,
            HashDomain::Sha256,
            "env-set hash domain must appear in resolved config"
        );

        // Some when file sets it.
        let file = ServeSettings {
            hash_domain: Some(HashDomain::Sha256),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert_eq!(r.explicit_hash_domain, Some(HashDomain::Sha256));
    }

    // --- resolve_log tests ---

    #[test]
    fn resolve_log_rust_log_beats_env_level_beats_file() {
        let file = LogSettings {
            level: Some("warn".into()),
            ..LogSettings::default()
        };
        // RUST_LOG beats all.
        let r = resolve_log(&file, |k| match k {
            "RUST_LOG" => Some("trace".into()),
            "NAIAD_REPO_LOG_LEVEL" => Some("debug".into()),
            _ => None,
        })
        .expect("resolve");
        assert_eq!(r.config.level, "trace");
        // Two warnings: env and file both lost to RUST_LOG.
        assert_eq!(r.overridden.len(), 2);
        assert!(r.overridden.iter().all(|o| o.winner_tier == "RUST_LOG env"));

        // NAIAD_REPO_LOG_LEVEL beats file.
        let r2 = resolve_log(&file, |k| {
            (k == "NAIAD_REPO_LOG_LEVEL").then(|| "debug".into())
        })
        .expect("resolve");
        assert_eq!(r2.config.level, "debug");
        assert_eq!(r2.overridden.len(), 1);
        assert_eq!(r2.overridden[0].winner_tier, "NAIAD_REPO_LOG_LEVEL env");
        assert_eq!(r2.overridden[0].loser_tier, "repo.toml");

        // File beats default.
        let r3 = resolve_log(&file, |_| None).expect("resolve");
        assert_eq!(r3.config.level, "warn");
        assert!(r3.overridden.is_empty());

        // Default when nothing set.
        let r4 = resolve_log(&LogSettings::default(), |_| None).expect("resolve");
        assert_eq!(r4.config.level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn resolve_log_console_env_beats_file() {
        let file = LogSettings {
            console: Some(true),
            ..LogSettings::default()
        };
        let r = resolve_log(&file, |k| {
            (k == "NAIAD_REPO_LOG_CONSOLE").then(|| "0".into())
        })
        .expect("resolve");
        assert!(!r.config.console);
    }

    #[test]
    fn resolve_log_console_malformed_env_is_fatal() {
        let err = resolve_log(&LogSettings::default(), |k| {
            (k == "NAIAD_REPO_LOG_CONSOLE").then(|| "maybe".into())
        })
        .expect_err("malformed CONSOLE must fail");
        assert!(format!("{err:#}").contains("NAIAD_REPO_LOG_CONSOLE"));
    }

    #[test]
    fn resolve_log_file_env_beats_file_and_empty_means_none() {
        let file = LogSettings {
            file: Some("repo.log".into()),
            ..LogSettings::default()
        };
        // Non-empty env beats file.
        let r = resolve_log(&file, |k| {
            (k == "NAIAD_REPO_LOG_FILE").then(|| "/var/log/naiad.log".into())
        })
        .expect("resolve");
        assert_eq!(r.config.file.as_deref(), Some("/var/log/naiad.log"));

        // Empty env = explicit none (beats file).
        let r2 = resolve_log(&file, |k| (k == "NAIAD_REPO_LOG_FILE").then(|| "".into()))
            .expect("resolve");
        assert_eq!(r2.config.file, None);

        // Absent env → file value.
        let r3 = resolve_log(&file, |_| None).expect("resolve");
        assert_eq!(r3.config.file.as_deref(), Some("repo.log"));
    }

    #[test]
    fn resolve_log_blank_values_are_absent() {
        // Blank RUST_LOG is absent (falls through).
        let r = resolve_log(&LogSettings::default(), |k| {
            (k == "RUST_LOG").then(|| "   ".into())
        })
        .expect("resolve");
        assert_eq!(r.config.level, DEFAULT_LOG_LEVEL);

        // Blank NAIAD_REPO_LOG_LEVEL is absent.
        let r2 = resolve_log(&LogSettings::default(), |k| {
            (k == "NAIAD_REPO_LOG_LEVEL").then(|| "".into())
        })
        .expect("resolve");
        assert_eq!(r2.config.level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn resolve_log_warns_when_rust_log_and_env_level_differ() {
        let r = resolve_log(&LogSettings::default(), |k| match k {
            "RUST_LOG" => Some("debug".into()),
            "NAIAD_REPO_LOG_LEVEL" => Some("warn".into()),
            _ => None,
        })
        .expect("resolve");
        assert_eq!(r.config.level, "debug");
        let w = r
            .overridden
            .iter()
            .find(|o| o.loser_tier == "NAIAD_REPO_LOG_LEVEL env")
            .expect("should warn about NAIAD_REPO_LOG_LEVEL losing to RUST_LOG");
        assert_eq!(w.winner_tier, "RUST_LOG env");
        assert_eq!(w.winner_val, "debug");
        assert_eq!(w.loser_val, "warn");
    }

    /// The shipped sample must stay byte-identical to the first-run template —
    /// one source of truth, mechanically enforced.
    #[test]
    fn shipped_sample_matches_scaffold() {
        let sample = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/repo.toml.sample"
        ))
        .expect("scripts/repo.toml.sample exists");
        // Normalize line endings: git may check the sample out with CRLF.
        assert_eq!(sample.replace("\r\n", "\n"), SCAFFOLD);
    }

    #[test]
    fn bridge_section_parses() {
        let d = dir();
        std::fs::write(
            d.path().join("repo.toml"),
            "[bridge]\nenabled = true\nptr_url = \"https://example.test:1\"\n\
             ptr_key = \"deadbeef\"\nstate_db = \"s.db\"\n",
        )
        .unwrap();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("load");
        assert!(s.bridge.enabled);
        assert_eq!(s.bridge.ptr_url, "https://example.test:1");
        assert_eq!(s.bridge.ptr_key, "deadbeef");
        assert_eq!(s.bridge.state_db, "s.db");
        assert!(ignored.is_empty());
    }

    #[test]
    fn bridge_defaults_when_absent() {
        let d = dir();
        std::fs::write(d.path().join("repo.toml"), "[serve]\nk = 5\n").unwrap();
        let (s, _ignored) = load(&d.path().join("repo.db")).expect("load");
        assert!(!s.bridge.enabled);
        assert_eq!(s.bridge.ptr_url, DEFAULT_PTR_URL);
        assert_eq!(s.bridge.ptr_key, DEFAULT_PTR_KEY);
        assert_eq!(s.bridge.state_db, DEFAULT_STATE_DB);
    }

    #[test]
    fn resolve_bridge_env_over_file() {
        let file = BridgeSettings {
            enabled: false,
            ptr_url: "https://file.test".into(),
            ptr_key: "filekey".into(),
            state_db: "file.db".into(),
            ..BridgeSettings::default()
        };
        let cfg = resolve_bridge(&file, |k| match k {
            "NAIAD_REPO_BRIDGE_ENABLED" => Some("true".into()),
            "NAIAD_REPO_BRIDGE_PTR_URL" => Some("https://env.test".into()),
            "NAIAD_REPO_BRIDGE_PTR_KEY" => Some("envkey".into()),
            "NAIAD_REPO_BRIDGE_STATE_DB" => Some("env.db".into()),
            _ => None,
        });
        assert!(cfg.enabled); // env wins
        assert_eq!(cfg.ptr_url, "https://env.test"); // env wins
        assert_eq!(cfg.ptr_key, "envkey"); // env wins
        assert_eq!(cfg.state_db, "env.db"); // env wins

        // Garbage bool falls back to the file value; other keys not set → file fallthrough.
        let cfg2 = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_ENABLED").then(|| "banana".into())
        });
        assert!(!cfg2.enabled);
        assert_eq!(cfg2.ptr_key, "filekey"); // file falls through
        assert_eq!(cfg2.state_db, "file.db"); // file falls through

        // Opposite polarity: env "false"/"0" must beat file enabled = true.
        let file_on = BridgeSettings {
            enabled: true,
            ..file.clone()
        };
        for v in ["false", "0", "no", "off", "FALSE"] {
            let cfg3 = resolve_bridge(&file_on, |k| {
                (k == "NAIAD_REPO_BRIDGE_ENABLED").then(|| v.into())
            });
            assert!(!cfg3.enabled, "env {v:?} should disable over file=true");
        }
    }

    // --- new correctness tests ---

    #[test]
    fn resolve_serve_blank_key_env_means_none() {
        // Whitespace-only NAIAD_REPO_KEY behaves like "": explicit none, beats file.
        let file = ServeSettings {
            repo_key: Some("filekey".into()),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_KEY").then(|| "   ".to_string())
        })
        .expect("resolve");
        assert_eq!(
            r.config.repo_key, None,
            "whitespace-only env key = explicit none"
        );
        let kw = r
            .overridden
            .iter()
            .find(|o| o.setting == "serve.repo_key")
            .expect("must emit a cross-tier warning");
        assert_eq!(kw.winner_val, "(none)", "warning winner must show (none)");
        assert_eq!(kw.loser_val, "filekey");
    }

    #[test]
    fn resolve_log_blank_log_file_env_means_none() {
        // Whitespace-only NAIAD_REPO_LOG_FILE = explicit none (no file sink).
        let file = LogSettings {
            file: Some("repo.log".into()),
            ..LogSettings::default()
        };
        let r = resolve_log(&file, |k| {
            (k == "NAIAD_REPO_LOG_FILE").then(|| "   ".into())
        })
        .expect("resolve");
        assert_eq!(
            r.config.file, None,
            "whitespace-only LOG_FILE env = no file sink"
        );
    }

    #[test]
    fn resolve_log_bad_directive_file_warns_and_falls_through() {
        // [log].level with an EnvFilter-rejectable value: warn-and-continue, fall through to default.
        // Uses "naiad_server=deubg" — a directive with an invalid level name that EnvFilter rejects.
        let file = LogSettings {
            level: Some("naiad_server=deubg".into()),
            ..LogSettings::default()
        };
        let r = resolve_log(&file, |_| None).expect("invalid file directive must warn, not fail");
        assert_eq!(
            r.config.level, DEFAULT_LOG_LEVEL,
            "must fall through to default"
        );
        assert!(
            !r.extra_warnings.is_empty(),
            "must push an extra_warning about the bad file value"
        );
        assert!(
            r.extra_warnings[0].contains("repo.toml"),
            "warning must mention repo.toml"
        );
    }

    #[test]
    fn resolve_log_bad_plain_level_naiad_env_is_fatal() {
        // NAIAD_REPO_LOG_LEVEL with a bare non-level word is a fatal error.
        // "ifno" is syntactically a valid EnvFilter target-filter directive, but the
        // hybrid rule rejects bare non-level words so operators see an error instead
        // of silently running at an unintended filter.
        let err = resolve_log(&LogSettings::default(), |k| {
            (k == "NAIAD_REPO_LOG_LEVEL").then(|| "ifno".into())
        })
        .expect_err("bare non-level word from NAIAD_REPO_LOG_LEVEL must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("NAIAD_REPO_LOG_LEVEL"),
            "error must name the variable"
        );
        assert!(msg.contains("ifno"), "error must echo the bad value");
    }

    #[test]
    fn resolve_log_bad_plain_level_rust_log_warns_and_falls_through() {
        // RUST_LOG with a bare non-level word: warn-and-continue, fall through to default.
        let r = resolve_log(&LogSettings::default(), |k| {
            (k == "RUST_LOG").then(|| "ifno".into())
        })
        .expect("must not fail — RUST_LOG bare non-level word is warn-and-continue");
        assert_eq!(
            r.config.level, DEFAULT_LOG_LEVEL,
            "must fall through to default"
        );
        assert!(
            !r.extra_warnings.is_empty(),
            "must push an extra_warning about the bad RUST_LOG value"
        );
        assert!(
            r.extra_warnings[0].contains("RUST_LOG"),
            "warning must mention RUST_LOG"
        );
    }

    #[test]
    fn resolve_log_bad_plain_level_file_warns_and_falls_through() {
        // [log].level with a bare non-level word: warn-and-continue, fall through to default.
        let file = LogSettings {
            level: Some("ifno".into()),
            ..LogSettings::default()
        };
        let r = resolve_log(&file, |_| None)
            .expect("must not fail — repo.toml bare non-level word is warn-and-continue");
        assert_eq!(
            r.config.level, DEFAULT_LOG_LEVEL,
            "must fall through to default"
        );
        assert!(
            !r.extra_warnings.is_empty(),
            "must push an extra_warning about the bad file value"
        );
        assert!(
            r.extra_warnings[0].contains("repo.toml"),
            "warning must mention repo.toml"
        );
    }

    #[test]
    fn resolve_log_padded_level_env_is_trimmed_and_accepted() {
        // "debug " with trailing space must be trimmed and accepted as "debug".
        let r = resolve_log(&LogSettings::default(), |k| {
            (k == "NAIAD_REPO_LOG_LEVEL").then(|| "debug ".into())
        })
        .expect("padded level must be accepted after trimming");
        assert_eq!(r.config.level, "debug", "trimmed value must be effective");
        assert!(
            r.extra_warnings.is_empty(),
            "no warnings for valid padded level"
        );

        // Same for RUST_LOG.
        let r2 = resolve_log(&LogSettings::default(), |k| {
            (k == "RUST_LOG").then(|| "warn ".into())
        })
        .expect("padded RUST_LOG level must be accepted");
        assert_eq!(r2.config.level, "warn");

        // Same for file tier.
        let file = LogSettings {
            level: Some("error ".into()),
            ..LogSettings::default()
        };
        let r3 = resolve_log(&file, |_| None).expect("padded file level must be accepted");
        assert_eq!(r3.config.level, "error");
    }

    #[test]
    fn resolve_log_off_level_is_accepted() {
        // "off" is a valid EnvFilter directive that silences all output.
        let r = resolve_log(&LogSettings::default(), |k| {
            (k == "NAIAD_REPO_LOG_LEVEL").then(|| "off".into())
        })
        .expect("'off' must be accepted as a valid level");
        assert_eq!(r.config.level, "off");
    }

    #[test]
    fn resolve_log_bad_directive_naiad_env_is_fatal() {
        // A directive-shaped but invalid value is caught by EnvFilter on NAIAD_REPO_LOG_LEVEL.
        let err = resolve_log(&LogSettings::default(), |k| {
            (k == "NAIAD_REPO_LOG_LEVEL").then(|| "naiad_server=deubg".into())
        })
        .expect_err("invalid directive from NAIAD_REPO_LOG_LEVEL must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("NAIAD_REPO_LOG_LEVEL"),
            "error must name the variable"
        );
        assert!(
            msg.contains("naiad_server=deubg"),
            "error must echo the bad value"
        );
    }

    #[test]
    fn resolve_log_bad_directive_rust_log_warns_and_falls_through() {
        // A directive-shaped but invalid RUST_LOG value: warn-and-continue.
        let r = resolve_log(&LogSettings::default(), |k| {
            (k == "RUST_LOG").then(|| "naiad_server=deubg".into())
        })
        .expect("invalid RUST_LOG directive must warn, not fail");
        assert_eq!(
            r.config.level, DEFAULT_LOG_LEVEL,
            "must fall through to default"
        );
        assert!(
            !r.extra_warnings.is_empty(),
            "must push an extra_warning about the bad RUST_LOG value"
        );
        assert!(
            r.extra_warnings[0].contains("RUST_LOG"),
            "warning must mention RUST_LOG"
        );
    }

    // --- Task 5: BridgeMode / snapshot fields ---

    #[test]
    fn bridge_snapshot_section_parses() {
        let d = dir();
        std::fs::write(
            d.path().join("repo.toml"),
            "[bridge]\nenabled = true\nmode = \"snapshot\"\n\
             snapshot_dir = \"/srv/ptr\"\nsnapshot_service_id = 9\nmax_query_bits = 32\n\
             min_query_bits = 16\n",
        )
        .unwrap();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("load");
        assert!(s.bridge.enabled);
        assert_eq!(s.bridge.mode, BridgeMode::Snapshot);
        assert_eq!(s.bridge.snapshot_dir.as_deref(), Some("/srv/ptr"));
        assert_eq!(s.bridge.snapshot_service_id, Some(9));
        assert_eq!(s.bridge.max_query_bits, 32);
        assert_eq!(s.bridge.min_query_bits, 16);
        assert!(ignored.is_empty(), "no unknown keys: {ignored:?}");
    }

    #[test]
    fn bridge_mode_defaults_to_mirror() {
        // An upgraded mirror deployment that never heard of `mode` keeps
        // behaving as a mirror.
        let d = dir();
        std::fs::write(d.path().join("repo.toml"), "[bridge]\nenabled = true\n").unwrap();
        let (s, _) = load(&d.path().join("repo.db")).expect("load");
        assert_eq!(s.bridge.mode, BridgeMode::Mirror);
        assert_eq!(s.bridge.snapshot_dir, None);
        assert_eq!(s.bridge.max_query_bits, DEFAULT_BRIDGE_MAX_QUERY_BITS);
        assert_eq!(
            s.bridge.min_query_bits, DEFAULT_BRIDGE_MIN_QUERY_BITS,
            "min_query_bits must default to SNAPSHOT_MIN_QUERY_BITS (8) when absent"
        );
    }

    #[test]
    fn resolve_bridge_reads_snapshot_env_vars() {
        let file = BridgeSettings::default();
        let cfg = resolve_bridge(&file, |k| match k {
            "NAIAD_REPO_BRIDGE_ENABLED" => Some("1".into()),
            "NAIAD_REPO_BRIDGE_MODE" => Some(" SNAPSHOT ".into()),
            "NAIAD_REPO_BRIDGE_SNAPSHOT_DIR" => Some("/env/ptr".into()),
            "NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID" => Some("11".into()),
            "NAIAD_REPO_BRIDGE_MAX_QUERY_BITS" => Some("64".into()),
            "NAIAD_REPO_BRIDGE_MIN_QUERY_BITS" => Some("16".into()),
            _ => None,
        });
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, BridgeMode::Snapshot);
        assert_eq!(cfg.snapshot_dir.as_deref(), Some("/env/ptr"));
        assert_eq!(cfg.snapshot_service_id, Some(11));
        assert_eq!(cfg.max_query_bits, 64);
        assert_eq!(cfg.min_query_bits, 16, "env MIN_QUERY_BITS must be parsed");
    }

    #[test]
    fn resolve_bridge_malformed_env_falls_back_to_file() {
        let file = BridgeSettings {
            mode: BridgeMode::Snapshot,
            max_query_bits: 24,
            min_query_bits: 12,
            snapshot_service_id: Some(7),
            ..BridgeSettings::default()
        };
        let cfg = resolve_bridge(&file, |k| match k {
            "NAIAD_REPO_BRIDGE_MODE" => Some("teleport".into()),
            "NAIAD_REPO_BRIDGE_MAX_QUERY_BITS" => Some("lots".into()),
            "NAIAD_REPO_BRIDGE_MIN_QUERY_BITS" => Some("many".into()),
            "NAIAD_REPO_BRIDGE_SNAPSHOT_SERVICE_ID" => Some("nine".into()),
            _ => None,
        });
        assert_eq!(
            cfg.mode,
            BridgeMode::Snapshot,
            "bad mode falls back to file"
        );
        assert_eq!(cfg.max_query_bits, 24, "bad max_bits falls back to file");
        assert_eq!(
            cfg.min_query_bits, 12,
            "bad min_bits falls back to file value"
        );
        assert_eq!(
            cfg.snapshot_service_id,
            Some(7),
            "bad id falls back to file value (not None)"
        );
    }

    #[test]
    fn resolve_bridge_blank_snapshot_dir_env_falls_back_to_file() {
        // blank (env_nonblank) = unset → fall through to file value.
        let file = BridgeSettings {
            snapshot_dir: Some("/file/ptr".into()),
            ..BridgeSettings::default()
        };
        let cfg = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_SNAPSHOT_DIR").then(|| "".into())
        });
        assert_eq!(
            cfg.snapshot_dir.as_deref(),
            Some("/file/ptr"),
            "blank env SNAPSHOT_DIR must fall back to file value"
        );
    }

    #[test]
    fn resolve_bridge_blank_ptr_url_falls_back_to_file() {
        let file = BridgeSettings {
            ptr_url: "https://file.test".into(),
            ptr_key: "filekey".into(),
            state_db: "file.db".into(),
            ..BridgeSettings::default()
        };
        // Empty PTR_URL falls through to file.
        let cfg = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_PTR_URL").then(|| "".into())
        });
        assert_eq!(
            cfg.ptr_url, "https://file.test",
            "blank env PTR_URL must fall back to file"
        );

        // Whitespace-only PTR_URL also falls through.
        let cfg2 = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_PTR_URL").then(|| "   ".into())
        });
        assert_eq!(
            cfg2.ptr_url, "https://file.test",
            "whitespace PTR_URL must fall back to file"
        );

        // Non-blank value still overrides.
        let cfg3 = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_PTR_URL").then(|| "https://env.test".into())
        });
        assert_eq!(
            cfg3.ptr_url, "https://env.test",
            "non-blank env PTR_URL must override file"
        );
    }

    #[test]
    fn resolve_bridge_blank_ptr_key_falls_back_to_file() {
        let file = BridgeSettings {
            ptr_key: "filekey".into(),
            ..BridgeSettings::default()
        };
        let cfg = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_PTR_KEY").then(|| "".into())
        });
        assert_eq!(
            cfg.ptr_key, "filekey",
            "blank env PTR_KEY must fall back to file"
        );

        let cfg2 = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_PTR_KEY").then(|| "envkey".into())
        });
        assert_eq!(
            cfg2.ptr_key, "envkey",
            "non-blank env PTR_KEY must override file"
        );
    }

    #[test]
    fn resolve_bridge_blank_state_db_falls_back_to_file() {
        let file = BridgeSettings {
            state_db: "file.db".into(),
            ..BridgeSettings::default()
        };
        let cfg = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_STATE_DB").then(|| "".into())
        });
        assert_eq!(
            cfg.state_db, "file.db",
            "blank env STATE_DB must fall back to file"
        );

        let cfg2 = resolve_bridge(&file, |k| {
            (k == "NAIAD_REPO_BRIDGE_STATE_DB").then(|| "env.db".into())
        });
        assert_eq!(
            cfg2.state_db, "env.db",
            "non-blank env STATE_DB must override file"
        );
    }

    #[test]
    fn bridge_enabled_with_explicit_blake3_is_no_longer_an_error() {
        // The 2026-07-24 rule ("explicit blake3 + bridge = fatal") is deleted:
        // that is now the normal dual-domain configuration.
        let file = ServeSettings {
            hash_domain: Some(HashDomain::Blake3),
            ..ServeSettings::default()
        };
        let resolution = resolve_serve(None, None, None, None, false, &file, |_| None)
            .expect("explicit blake3 must resolve cleanly");
        assert_eq!(resolution.config.hash_domain, HashDomain::Blake3);
        assert_eq!(
            resolution.explicit_hash_domain,
            Some(HashDomain::Blake3),
            "still recorded, just no longer fatal"
        );
    }

    // --- no_egress resolution tests (§4, #190) ---

    #[test]
    fn resolve_serve_no_egress_cli_flag_beats_env_and_file() {
        // CLI flag --no-egress (true) beats env=false and file=false.
        let file = ServeSettings {
            no_egress: Some(false),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, true, &file, |k| {
            (k == "NAIAD_REPO_NO_EGRESS").then(|| "false".to_string())
        })
        .expect("resolve");
        assert!(r.config.no_egress, "CLI flag must win");
        // Two warnings: env=false and file=false both lost to CLI=true.
        assert_eq!(r.overridden.len(), 2);
        assert!(r.overridden.iter().all(|o| o.setting == "serve.no_egress"));
    }

    #[test]
    fn resolve_serve_no_egress_env_beats_file() {
        let file = ServeSettings {
            no_egress: Some(false),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_NO_EGRESS").then(|| "true".to_string())
        })
        .expect("resolve");
        assert!(r.config.no_egress, "env must beat file");
        assert_eq!(r.overridden.len(), 1);
        let w = &r.overridden[0];
        assert_eq!(w.setting, "serve.no_egress");
        assert_eq!(w.winner_tier, "NAIAD_REPO_NO_EGRESS env");
        assert_eq!(w.loser_tier, "repo.toml");
    }

    #[test]
    fn resolve_serve_no_egress_file_beats_default() {
        let file = ServeSettings {
            no_egress: Some(true),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |_| None).expect("resolve");
        assert!(r.config.no_egress, "file value must beat default false");
    }

    #[test]
    fn resolve_serve_no_egress_default_is_false() {
        let r = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve");
        assert!(!r.config.no_egress, "default must be false");
    }

    #[test]
    fn resolve_serve_no_egress_malformed_env_is_fatal() {
        let err = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_NO_EGRESS").then(|| "maybe".to_string()),
        )
        .expect_err("malformed NAIAD_REPO_NO_EGRESS must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("NAIAD_REPO_NO_EGRESS"),
            "error must name the variable"
        );
    }

    #[test]
    fn resolve_serve_no_egress_blank_env_falls_through() {
        // Blank env = absent; falls through to file default.
        let file = ServeSettings {
            no_egress: Some(true),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file, |k| {
            (k == "NAIAD_REPO_NO_EGRESS").then(|| "".to_string())
        })
        .expect("resolve");
        // Blank env is absent → file wins.
        assert!(
            r.config.no_egress,
            "blank env must fall through to file=true"
        );
    }

    // --- resolve_stats tests (Task 1, #235) ---

    #[test]
    fn resolve_stats_defaults() {
        let cfg = resolve_stats(&StatsSettings::default(), Path::new("/srv/repo.db"), |_| {
            None
        })
        .expect("defaults resolve");
        assert!(cfg.enabled);
        assert_eq!(cfg.listen, "127.0.0.1:9092".parse().unwrap());
        assert!(!cfg.allow_non_loopback);
        assert_eq!(
            cfg.db_path,
            resolve_beside_db(Path::new("/srv/repo.db"), "stats.db")
        );
    }

    #[test]
    fn resolve_stats_env_beats_file() {
        let file = StatsSettings {
            enabled: Some(true),
            listen: Some("127.0.0.1:9092".parse().unwrap()),
            allow_non_loopback: None,
            db_path: Some("file.db".into()),
        };
        let env = |k: &str| match k {
            "NAIAD_REPO_STATS_LISTEN" => Some("127.0.0.1:7000".to_string()),
            "NAIAD_REPO_STATS_DB" => Some("env.db".to_string()),
            _ => None,
        };
        let cfg = resolve_stats(&file, Path::new("/srv/repo.db"), env).expect("resolve");
        assert_eq!(cfg.listen, "127.0.0.1:7000".parse().unwrap());
        assert_eq!(
            cfg.db_path,
            resolve_beside_db(Path::new("/srv/repo.db"), "env.db")
        );
    }

    #[test]
    fn resolve_stats_malformed_listen_is_fatal() {
        let env = |k: &str| (k == "NAIAD_REPO_STATS_LISTEN").then(|| "not-an-addr".to_string());
        let err = resolve_stats(&StatsSettings::default(), Path::new("/srv/repo.db"), env)
            .expect_err("malformed listen must be fatal");
        assert!(format!("{err:#}").contains("NAIAD_REPO_STATS_LISTEN"));
    }

    #[test]
    fn resolve_stats_non_loopback_refused_by_default() {
        let file = StatsSettings {
            listen: Some("0.0.0.0:9092".parse().unwrap()),
            ..Default::default()
        };
        let err = resolve_stats(&file, Path::new("/srv/repo.db"), |_| None)
            .expect_err("non-loopback must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("0.0.0.0"), "names the address: {msg}");
        assert!(
            msg.contains("allow_non_loopback"),
            "names the escape hatch: {msg}"
        );
    }

    #[test]
    fn resolve_stats_non_loopback_allowed_with_flag() {
        let file = StatsSettings {
            listen: Some("0.0.0.0:9092".parse().unwrap()),
            allow_non_loopback: Some(true),
            ..Default::default()
        };
        let cfg = resolve_stats(&file, Path::new("/srv/repo.db"), |_| None).expect("allowed");
        assert_eq!(cfg.listen, "0.0.0.0:9092".parse().unwrap());
    }

    #[test]
    fn resolve_stats_disabled_skips_loopback_guard() {
        let file = StatsSettings {
            enabled: Some(false),
            listen: Some("0.0.0.0:9092".parse().unwrap()),
            ..Default::default()
        };
        let cfg = resolve_stats(&file, Path::new("/srv/repo.db"), |_| None)
            .expect("disabled skips guard");
        assert!(!cfg.enabled);
    }

    #[test]
    fn resolve_stats_malformed_enabled_is_fatal() {
        let env = |k: &str| (k == "NAIAD_REPO_STATS_ENABLED").then(|| "maybe".to_string());
        let err = resolve_stats(&StatsSettings::default(), Path::new("/srv/repo.db"), env)
            .expect_err("malformed enabled must be fatal");
        assert!(format!("{err:#}").contains("NAIAD_REPO_STATS_ENABLED"));
    }

    #[test]
    fn resolve_stats_malformed_allow_non_loopback_is_fatal() {
        let env =
            |k: &str| (k == "NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK").then(|| "maybe".to_string());
        let err = resolve_stats(&StatsSettings::default(), Path::new("/srv/repo.db"), env)
            .expect_err("malformed allow_non_loopback must be fatal");
        assert!(format!("{err:#}").contains("NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK"));
    }

    #[test]
    fn resolve_stats_loopback_127_always_fine() {
        let file = StatsSettings {
            listen: Some("127.0.0.1:9092".parse().unwrap()),
            ..Default::default()
        };
        let cfg = resolve_stats(&file, Path::new("/srv/repo.db"), |_| None)
            .expect("127.0.0.1 always fine");
        assert_eq!(cfg.listen, "127.0.0.1:9092".parse().unwrap());
    }

    #[test]
    fn resolve_stats_loopback_ipv6_always_fine() {
        let file = StatsSettings {
            listen: Some("[::1]:9092".parse().unwrap()),
            ..Default::default()
        };
        let cfg = resolve_stats(&file, Path::new("/srv/repo.db"), |_| None)
            .expect("::1 is a loopback address and must be accepted");
        assert_eq!(cfg.listen, "[::1]:9092".parse().unwrap());
    }

    #[test]
    fn resolve_stats_ipv6_wildcard_refused_by_default() {
        let file = StatsSettings {
            listen: Some("[::]:9092".parse().unwrap()),
            ..Default::default()
        };
        let err = resolve_stats(&file, Path::new("/srv/repo.db"), |_| None)
            .expect_err(":: is non-loopback and must be refused by default");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("allow_non_loopback"),
            "names the escape hatch: {msg}"
        );
    }

    #[test]
    fn resolve_stats_allow_non_loopback_via_env_permits_wildcard() {
        // NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK=1 in env must permit 0.0.0.0.
        let file = StatsSettings {
            listen: Some("0.0.0.0:9092".parse().unwrap()),
            ..Default::default()
        };
        let env = |k: &str| (k == "NAIAD_REPO_STATS_ALLOW_NON_LOOPBACK").then(|| "1".to_string());
        let cfg = resolve_stats(&file, Path::new("/srv/repo.db"), env)
            .expect("env ALLOW_NON_LOOPBACK=1 must permit 0.0.0.0");
        assert_eq!(cfg.listen, "0.0.0.0:9092".parse().unwrap());
        assert!(cfg.allow_non_loopback);
    }

    #[test]
    fn stats_section_parses_from_toml() {
        let d = dir();
        std::fs::write(
            d.path().join("repo.toml"),
            "[stats]\nenabled = false\nlisten = \"127.0.0.1:9093\"\nallow_non_loopback = false\ndb_path = \"my-stats.db\"\n",
        )
        .unwrap();
        let (s, ignored) = load(&d.path().join("repo.db")).expect("load");
        assert_eq!(s.stats.enabled, Some(false));
        assert_eq!(s.stats.listen, Some("127.0.0.1:9093".parse().unwrap()));
        assert_eq!(s.stats.allow_non_loopback, Some(false));
        assert_eq!(s.stats.db_path.as_deref(), Some("my-stats.db"));
        assert!(ignored.is_empty(), "no unknown keys: {ignored:?}");
    }

    #[test]
    fn old_caps_count_ttl_secs_is_tolerated() {
        // Pre-v0.2.85 repo.toml files may contain caps_count_ttl_secs.
        // serde_ignored must swallow the unknown key without error.
        let toml = "[serve]\naddr = \"0.0.0.0:8080\"\ncaps_count_ttl_secs = 600\n";
        let parsed: RepoSettings = toml::from_str(toml)
            .expect("old caps_count_ttl_secs key must be tolerated by serde_ignored");
        assert_eq!(
            parsed.serve.addr.map(|a| a.to_string()).as_deref(),
            Some("0.0.0.0:8080")
        );
    }

    // --- read_only resolution tests (#202, Task 7) ---

    /// `read_only` file Some(true) → resolved true; default → false; env beats file;
    /// malformed env → fatal (same semantics as `no_egress`).
    #[test]
    fn read_only_resolves_like_no_egress() {
        // file Some(true) → true (file beats default).
        let file_true = ServeSettings {
            read_only: Some(true),
            ..ServeSettings::default()
        };
        let r = resolve_serve(None, None, None, None, false, &file_true, |_| None)
            .expect("resolve with file=true");
        assert!(r.config.read_only, "file=true must resolve to true");

        // default → false.
        let r2 = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |_| None,
        )
        .expect("resolve default");
        assert!(!r2.config.read_only, "default must be false");

        // env beats file: env=false, file=true → false.
        let r3 = resolve_serve(None, None, None, None, false, &file_true, |k| {
            (k == "NAIAD_REPO_READ_ONLY").then(|| "false".to_string())
        })
        .expect("resolve env=false beats file=true");
        assert!(!r3.config.read_only, "env=false must beat file=true");

        // env=true beats default false.
        let r4 = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_READ_ONLY").then(|| "true".to_string()),
        )
        .expect("resolve env=true");
        assert!(r4.config.read_only, "env=true must beat default false");

        // malformed env → fatal.
        let err = resolve_serve(
            None,
            None,
            None,
            None,
            false,
            &ServeSettings::default(),
            |k| (k == "NAIAD_REPO_READ_ONLY").then(|| "maybe".to_string()),
        )
        .expect_err("malformed NAIAD_REPO_READ_ONLY must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("NAIAD_REPO_READ_ONLY"),
            "error must name the variable: {msg}"
        );
    }
}
