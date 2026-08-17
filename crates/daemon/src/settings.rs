//! Client settings file (`naiad.toml`) — the single source of truth for editable
//! scalar client settings. Lives beside the database (next to `naiad.key`). The
//! daemon owns it: mtime-gated reads pick up external hand-edits on the next
//! query; CLI edits go daemon -> file via a comment-preserving atomic write.
//!
//! Secrets never live here — the Ed25519 key stays in `naiad.key`.
//!
//! The trust floor (`[trust]`) section was removed in the client/server pivot.
//! Old `naiad.toml` files that carry a `[trust]` section are still parsed without
//! error — unknown sections and keys are tolerated by the `serde` deserializer
//! (no `deny_unknown_fields`). The section simply has no effect.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use naiad_db::Db;

/// DB-location hint keys: if a root-level `naiad.toml` key matches any of
/// these, emit an extra hint explaining why db-location belongs in `NAIAD_DB`
/// or `--db`, not in the settings file.
const DB_HINT_KEYS: &[&str] = &[
    "db",
    "database",
    "db_path",
    "dir",
    "directory",
    "data_dir",
    "path",
];

/// First-run template: a fully-commented `naiad.toml` documenting every
/// daemon-owned setting at its default. Every key is commented, so it parses to
/// `Settings::default()` until the user (or the daemon) sets a value.
const SCAFFOLD: &str = "\
# naiad client settings — single source of truth for editable settings.
# Hand-edits are picked up automatically on the next read.
# Secrets live in naiad.key, not here.

[hydrus]
# Path to your Hydrus database directory (the folder with client.db).
# dir = \"/path/to/hydrus/db\"
# Tag service IDs to import. Empty/absent = all services.
# tag_services = [1, 2]

[log]
# Log filter. A bare level (error | warn | info | debug | trace) OR full
# per-target directives, e.g. \"info,db=debug,scan=debug\" (targets: db, scan,
# thumb, watch, sync). RUST_LOG overrides this. Takes effect on daemon restart.
# level = \"info\"
# Auto-open the desktop debug console window at launch (same as passing
# --console or setting NAIAD_CONSOLE=1). Default: false.
# console = false
# Also write logs to this file (in addition to the console). A relative path is
# resolved next to the database. Appends across runs. Absent = no file.
# file = \"naiad.log\"

[net]
# Allow binding to a non-loopback address (e.g. 0.0.0.0 or a LAN IP), exposing
# the library — original media, file paths, and mutating endpoints — to other
# machines. Remote exposure is UNSUPPORTED and has no authentication. Leave
# false unless you understand the risk. Default: false.
# allow_remote = false

[privacy]
# Maximum precision (in leading hash bits) a pull query may reveal to a repo.
# Targeted pull asks for k-anonymity \"buckets\" — the leading bits of each owned
# file's hash — so the repo sees a crowd, not your exact files. The repo
# advertises how many bits to use; this caps how many YOUR client will ever emit,
# regardless of what a repo asks for. Lower = a bigger crowd, more privacy, and
# more download (tags for files you don't own). Higher = smaller crowd, less
# download, and — at the extreme — a repo could pin your query to individual file
# hashes (256 bits = your exact library). Only raise this if you accept that
# correlation risk (e.g. you pull over a VPN or Tor). Default: 24.
# max_query_bits = 24

# Subscribed tag repositories — the source of truth at daemon start.
# Adding an entry subscribes; deleting one detaches it (its pulled tags are
# KEPT; purging is an explicit UI/CLI action, never a toml edit).
# One [[repos]] block per repository:
# [[repos]]
# name = \"example\"
# url = \"http://127.0.0.1:9090\"
# Optional per-repo override of [privacy].max_query_bits (full override, 1-256).
# Snapshot-mode repos need 256 (exact-hash queries) to answer quickly; setting
# 256 reveals your exact file hashes to THIS repo only — use it solely for
# repos whose operator you trust.
# max_query_bits = 256

[[repos]]
name = \"naiad-net\"
url = \"https://v2202608398476500144.ultrasrv.de\"
";

/// One subscribed repository: a `[[repos]]` entry in `naiad.toml`.
///
/// Both fields carry `#[serde(default)]`, so an entry with an omitted `name`
/// or `url` deserialises to `""` rather than failing — even via
/// [`SettingsStore::settings_strict`]. Semantic validation (rejecting blank
/// name or url, logging a warning and skipping the entry) is the caller's
/// responsibility (boot reconcile).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepoEntry {
    /// Repository display name. Defaults to `""` when the key is absent.
    #[serde(default)]
    pub name: String,
    /// Repository base URL. Defaults to `""` when the key is absent.
    #[serde(default)]
    pub url: String,
    /// Optional per-repo override of `[privacy] max_query_bits` — a FULL
    /// override (may be higher or lower than the global). `None` = use the
    /// global ceiling. Out-of-range values are clamped into `[1, 256]` at
    /// resolution time with a warning. See #169: snapshot-mode repos need
    /// 256-bit (exact-hash) queries to answer within the HTTP timeout, and a
    /// trusted home repo is exactly where revealing exact hashes is acceptable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_query_bits: Option<u32>,
}

impl RepoEntry {
    /// Resolve this repo's effective privacy ceiling: the per-repo override
    /// (clamped into `[1, 256]`, warning when the clamp fires) or `global`.
    #[must_use]
    pub fn effective_max_query_bits(&self, global: u32) -> u32 {
        match self.max_query_bits {
            None => global,
            Some(v) => {
                let clamped = v.clamp(1, 256);
                if clamped != v {
                    tracing::warn!(
                        target: "settings",
                        repo = %self.name,
                        configured = v,
                        clamped,
                        "[[repos]] max_query_bits out of range [1, 256]; clamping"
                    );
                }
                clamped
            }
        }
    }
}

/// All persisted, file-resident client settings. Every field defaults, so a
/// missing key or section yields the default and an absent file yields
/// `Settings::default()`. Unknown keys and sections are tolerated (no
/// `deny_unknown_fields`) for forward compatibility and to silently accept old
/// files that carried the now-removed `[trust]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub hydrus: HydrusSettings,
    #[serde(default)]
    pub log: LogSettings,
    #[serde(default)]
    pub net: NetSettings,
    #[serde(default)]
    pub privacy: PrivacySettings,
    /// Subscribed repositories, reconciled against the DB at daemon boot.
    /// `None` = the file has no `repos` key at all (predates the feature or was
    /// hand-stripped) — boot reconcile treats that as "seed from the DB", never
    /// as "detach everything". `Some(vec![])` would mean an explicit empty list.
    #[serde(default)]
    pub repos: Option<Vec<RepoEntry>>,
}

/// Network binding settings. Controls whether the daemon may bind to a
/// non-loopback address. Default is local-only (`allow_remote = false`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetSettings {
    /// Allow binding a non-loopback address. Default `false`.
    #[serde(default)]
    pub allow_remote: bool,
}

/// Query-privacy settings. Bounds the precision a pull query reveals to a repo.
/// Default caps queries at 24 bits (`max_query_bits = 24`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrivacySettings {
    /// Ceiling on the prefix width (leading hash bits) a pull may mask to. The
    /// client clamps any repo-advertised `prefix_bits` down to this. Default 24.
    #[serde(default = "default_max_query_bits")]
    pub max_query_bits: u32,
}

fn default_max_query_bits() -> u32 {
    24
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            max_query_bits: default_max_query_bits(),
        }
    }
}

/// Hydrus importer settings. `dir` absent = unconfigured; `tag_services` empty =
/// import all services.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HydrusSettings {
    /// Hydrus DB directory (the folder containing `client.db`). `None` = unset.
    #[serde(default)]
    pub dir: Option<String>,
    /// Tag service IDs to import. Empty = all services.
    #[serde(default)]
    pub tag_services: Vec<i64>,
}

/// Logging settings. `level` is the default `tracing` filter used when
/// `RUST_LOG` is unset (`RUST_LOG` overrides it); it takes effect on the next
/// daemon start, not live. `console` asks the desktop shell to auto-open a debug
/// console window at launch, equivalent to `--console` / `NAIAD_CONSOLE=1`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogSettings {
    /// Default level: `error` | `warn` | `info` | `debug` | `trace`. `None`
    /// (absent) means the daemon default (`info`).
    #[serde(default)]
    pub level: Option<String>,
    /// Auto-open the desktop debug console at launch. `None` (absent in TOML)
    /// is treated as `false`. The first-set-wins console ladder uses this
    /// `Option` to distinguish "absent" from "explicitly set to false".
    #[serde(default)]
    pub console: Option<bool>,
    /// Also write logs to this file, additive to the console/stderr sink. A
    /// relative path is resolved next to the database; the file is opened in
    /// append mode so runs accumulate. `None` (absent) = no file sink.
    #[serde(default)]
    pub file: Option<String>,
}

/// The settings-file path beside a database file: `<db dir>/naiad.toml`.
/// Mirrors `account::key_path_for`.
#[must_use]
pub fn settings_path_for(db_path: &Path) -> PathBuf {
    db_path.with_file_name("naiad.toml")
}

/// Owns `naiad.toml`: an mtime-gated read cache plus a comment-preserving,
/// atomic writer. Cheap to construct; does no IO until first read/write.
pub struct SettingsStore {
    path: PathBuf,
    /// `(file mtime when parsed, parsed value)`. `None` mtime = file absent.
    cache: Mutex<Option<(Option<SystemTime>, Settings)>>,
    /// Ignored key paths already warned about, from the most recent fresh parse.
    /// Boot reads `naiad.toml` through more than one path (`settings_strict` for
    /// reconcile, `settings_and_diagnostics` for everything else) and
    /// `settings_strict` deliberately bypasses the mtime cache — without this,
    /// one unknown key logs its warning two or three times at startup.
    warned: Mutex<Vec<String>>,
}

/// Parse `content` as TOML into `Settings`, returning the parsed value and a
/// list of ignored key paths. The list is empty when all keys are recognised.
/// This function is **pure** — it emits no warnings; call
/// [`emit_settings_warnings`] to log them.
fn parse_settings(content: &str) -> Result<(Settings, Vec<String>), toml::de::Error> {
    let de = toml::Deserializer::new(content);
    let mut ignored: Vec<String> = Vec::new();
    let settings: Settings = serde_ignored::deserialize(de, |path| {
        ignored.push(path.to_string());
    })?;
    Ok((settings, ignored))
}

/// Emit `tracing::warn!` for each ignored key path returned by
/// [`parse_settings`]. A special hint is added for root-level keys that users
/// commonly mistake for a db-location setting.
fn emit_settings_warnings(ignored: &[String], file_label: &str) {
    for key in ignored {
        tracing::warn!(
            target: "settings",
            "{file_label}: ignoring unknown setting '{key}'"
        );
        let root = key.split('.').next().unwrap_or(key.as_str());
        if DB_HINT_KEYS.contains(&root) {
            tracing::warn!(
                target: "settings",
                "{file_label}: the database location cannot be set from naiad.toml \
                 (the file is found next to the database); use NAIAD_DB or --db instead"
            );
        }
    }
}

impl SettingsStore {
    /// Construct a store for the file at `path`. No IO happens here.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new(None),
            warned: Mutex::new(Vec::new()),
        }
    }

    /// Emit warnings for the ignored keys this store has not already warned
    /// about, then remember the current set. Returns the keys actually warned
    /// about. A key that disappears from the file and is later reintroduced
    /// warns again, because the remembered set is replaced (not accumulated) on
    /// every fresh parse.
    fn warn_once(&self, ignored: &[String], file_label: &str) -> Vec<String> {
        let fresh: Vec<String> = {
            let mut warned = self.warned.lock().expect("settings warned-set poisoned");
            let fresh = ignored
                .iter()
                .filter(|k| !warned.contains(k))
                .cloned()
                .collect();
            warned.clear();
            warned.extend_from_slice(ignored);
            fresh
        };
        emit_settings_warnings(&fresh, file_label);
        fresh
    }

    /// The file's current modification time, or `None` if it does not exist /
    /// cannot be stat-ed.
    fn current_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
    }

    /// The current settings plus the list of ignored key paths from a **fresh
    /// parse**. On a cache hit (same mtime as the last parse) the ignored-keys
    /// Vec is empty — warnings were already emitted when the file was first
    /// read. Callers that only need the settings value should use
    /// [`SettingsStore::settings`] instead.
    ///
    /// A missing file yields `(Settings::default(), vec![])`. A malformed file
    /// yields the last-good cached value and logs a warning — a bad file never
    /// fails or panics.
    #[must_use]
    pub fn settings_and_diagnostics(&self) -> (Settings, Vec<String>) {
        let mtime = self.current_mtime();
        let mut cache = self.cache.lock().expect("settings cache poisoned");
        if let Some((cached_mtime, cached)) = cache.as_ref() {
            if *cached_mtime == mtime {
                return (cached.clone(), vec![]);
            }
        }
        let label = self.path.display().to_string();
        match std::fs::read_to_string(&self.path) {
            Ok(content) => match parse_settings(&content) {
                Ok((parsed, ignored)) => {
                    self.warn_once(&ignored, &label);
                    *cache = Some((mtime, parsed.clone()));
                    (parsed, ignored)
                }
                Err(e) => {
                    tracing::warn!(
                        target: "settings",
                        "naiad.toml ({label}): parse error: {e}; keeping last-good settings"
                    );
                    // Deliberately do NOT update the cache mtime, so a corrected
                    // file (new mtime) is re-parsed on the next read.
                    let fallback = cache
                        .as_ref()
                        .map_or_else(Settings::default, |(_, s)| s.clone());
                    (fallback, vec![])
                }
            },
            Err(_) => {
                // Missing or unreadable: defaults, cached against this (None) mtime.
                let d = Settings::default();
                *cache = Some((mtime, d.clone()));
                (d, vec![])
            }
        }
    }

    /// The current settings, served from an mtime-gated cache. A missing file
    /// yields `Settings::default()`. A malformed file yields the last-good
    /// cached value (or the default) and logs a warning — a bad file never
    /// fails or panics a read. An external edit (changed mtime) is re-parsed on
    /// the next call. Unknown keys are logged with `tracing::warn!` on fresh
    /// parses only — cached reads do not re-emit warnings.
    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings_and_diagnostics().0
    }

    /// The current Hydrus importer settings (mtime-gated, same cache as `settings`).
    #[must_use]
    pub fn hydrus(&self) -> HydrusSettings {
        self.settings().hydrus
    }

    /// The current logging settings (mtime-gated, same cache as `settings`).
    #[must_use]
    pub fn log(&self) -> LogSettings {
        self.settings().log
    }

    /// The current network binding settings (mtime-gated, same cache as `settings`).
    #[must_use]
    pub fn net(&self) -> NetSettings {
        self.settings().net
    }

    /// The current query-privacy settings (mtime-gated, same cache as `settings`).
    #[must_use]
    pub fn privacy(&self) -> PrivacySettings {
        self.settings().privacy
    }

    /// Set the Hydrus importer config in `naiad.toml`, preserving comments, key
    /// order, and unknown keys via `toml_edit`, written atomically. `dir` `None`
    /// (or empty) clears the directory; an empty `tag_services` clears that key
    /// (meaning "all services"). An emptied `[hydrus]` table is removed.
    ///
    /// # Errors
    /// Returns an error if the existing file is not valid TOML, or the write fails.
    pub fn set_hydrus(&self, dir: Option<&str>, tag_services: &[i64]) -> anyhow::Result<()> {
        let existing = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut doc = existing
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| anyhow::anyhow!("{} is not valid TOML: {e}", self.path.display()))?;
        // Ensure `[hydrus]` is a standard (sectioned) table, not an inline one.
        if !doc.get("hydrus").is_some_and(toml_edit::Item::is_table) {
            doc["hydrus"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        match dir.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => doc["hydrus"]["dir"] = toml_edit::value(d),
            None => {
                if let Some(t) = doc.get_mut("hydrus").and_then(|i| i.as_table_mut()) {
                    t.remove("dir");
                }
            }
        }
        if tag_services.is_empty() {
            if let Some(t) = doc.get_mut("hydrus").and_then(|i| i.as_table_mut()) {
                t.remove("tag_services");
            }
        } else {
            let mut arr = toml_edit::Array::new();
            for s in tag_services {
                arr.push(*s);
            }
            doc["hydrus"]["tag_services"] = toml_edit::value(arr);
        }
        // Drop an emptied `[hydrus]` table so a fully-cleared config leaves no shell.
        if doc
            .get("hydrus")
            .and_then(toml_edit::Item::as_table)
            .is_some_and(toml_edit::Table::is_empty)
        {
            doc.as_table_mut().remove("hydrus");
        }
        let rendered = doc.to_string();
        self.write_atomic(&rendered)?;
        // Refresh the cache from what we just wrote, against the new mtime.
        let parsed = toml::from_str::<Settings>(&rendered).unwrap_or_default();
        let mtime = self.current_mtime();
        *self.cache.lock().expect("settings cache poisoned") = Some((mtime, parsed));
        Ok(())
    }

    /// Replace the `[[repos]]` array-of-tables, preserving comments, key order,
    /// and every other section via `toml_edit`, written atomically. An empty
    /// list removes the section.
    ///
    /// # Errors
    /// Returns an error if the existing file is not valid TOML, or the write fails.
    pub fn set_repos(&self, repos: &[RepoEntry]) -> anyhow::Result<()> {
        let existing = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut doc = existing
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| anyhow::anyhow!("{} is not valid TOML: {e}", self.path.display()))?;
        if repos.is_empty() {
            doc.as_table_mut().remove("repos");
        } else {
            let repos_existed = doc.contains_key("repos");
            let mut arr = toml_edit::ArrayOfTables::new();
            for r in repos {
                let mut t = toml_edit::Table::new();
                t["name"] = toml_edit::value(r.name.as_str());
                t["url"] = toml_edit::value(r.url.as_str());
                if let Some(bits) = r.max_query_bits {
                    t["max_query_bits"] = toml_edit::value(i64::from(bits));
                }
                arr.push(t);
            }
            if !repos_existed {
                // First-time insert: toml_edit appends the new `[[repos]]`
                // header immediately after the last real table, but the
                // document's trailing comment block renders *after* that — so
                // on the scaffold the header would wedge between `[privacy]` and
                // that section's commented keys (they belong to no live table),
                // capturing them into `[[repos]]` on re-parse (#168). Reclaim the
                // trailing trivia as the block's leading decor so the header
                // lands after every comment, at true document level.
                let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
                if !trailing.trim().is_empty() {
                    doc.set_trailing("");
                    if let Some(first) = arr.get_mut(0) {
                        let block = trailing.trim_end_matches(['\n', ' ', '\t']);
                        first.decor_mut().set_prefix(format!("{block}\n\n"));
                    }
                }
            } else {
                // Replacing an existing `[[repos]]` array: carry over the
                // leading decor of the original first entry so that comments
                // between the last real section and `[[repos]]` (e.g. the
                // subscribed-repos explanation block) are preserved.
                let existing_prefix = doc
                    .get("repos")
                    .and_then(|item| item.as_array_of_tables())
                    .and_then(|aot| aot.get(0))
                    .and_then(|t| t.decor().prefix())
                    .and_then(|r| r.as_str())
                    .map(ToOwned::to_owned);
                if let Some(prefix) = existing_prefix {
                    if let Some(first) = arr.get_mut(0) {
                        first.decor_mut().set_prefix(prefix);
                    }
                }
            }
            doc["repos"] = toml_edit::Item::ArrayOfTables(arr);
        }
        let rendered = doc.to_string();
        self.write_atomic(&rendered)?;
        let parsed = toml::from_str::<Settings>(&rendered).unwrap_or_default();
        let mtime = self.current_mtime();
        *self.cache.lock().expect("settings cache poisoned") = Some((mtime, parsed));
        Ok(())
    }

    /// Strict read for boot reconcile: `Ok(None)` = file absent, `Ok(Some)` =
    /// parsed, `Err` = present but malformed. Unlike [`SettingsStore::settings`]
    /// this never falls back to defaults — a malformed file must make the
    /// caller skip reconcile, not detach everything. Unknown keys are logged
    /// with `tracing::warn!`.
    ///
    /// Note: semantic validation of `RepoEntry` fields (non-empty `name`/`url`)
    /// is the **caller's** responsibility. An entry with a missing `name` or
    /// `url` key will parse successfully as `""` — `#[serde(default)]` on
    /// those fields is intentional.
    ///
    /// This method neither consults nor updates the mtime cache; it always
    /// reads from disk and the result is not stored.
    ///
    /// # Errors
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn settings_strict(&self) -> anyhow::Result<Option<Settings>> {
        let label = self.path.display().to_string();
        match std::fs::read_to_string(&self.path) {
            Ok(content) => {
                let (settings, ignored) = parse_settings(&content)?;
                self.warn_once(&ignored, &label);
                Ok(Some(settings))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the commented first-run template iff the file does not yet exist.
    /// No-op when the file is present — never clobbers a user file or hand-edits.
    ///
    /// # Errors
    /// Returns an error if the write fails.
    pub fn ensure_scaffold(&self) -> anyhow::Result<()> {
        if self.path.exists() {
            return Ok(());
        }
        self.write_atomic(SCAFFOLD)
    }

    /// Write `content` to the settings file atomically: write a sibling temp file
    /// then rename over the target (rename replaces an existing file on both
    /// Unix and Windows).
    fn write_atomic(&self, content: &str) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Formerly migrated an old `app_settings.trust_floor` DB row into
/// `naiad.toml`. Migration 0030 deletes that row unconditionally, so this
/// function is now a no-op; kept to avoid breaking call-site compatibility.
///
/// # Errors
/// Never errors; the signature is preserved for call-site compatibility.
pub fn migrate_trust_floor_to_file(_db: &Db, _settings: &SettingsStore) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_path_sits_beside_the_database() {
        let p = settings_path_for(Path::new("/lib/naiad.db"));
        assert_eq!(p, PathBuf::from("/lib/naiad.toml"));
    }

    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn missing_file_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        assert_eq!(store.settings(), Settings::default());
    }

    #[test]
    fn malformed_file_falls_back_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "this is { not valid toml ===");
        let store = SettingsStore::new(path);
        // No panic, no error surfaced to the caller: defaults.
        assert_eq!(store.settings(), Settings::default());
    }

    /// Old `naiad.toml` files that carry a `[trust]` section (removed in the
    /// client/server pivot) must parse without error — the section is simply
    /// ignored by the `serde` deserializer (no `deny_unknown_fields`).
    #[test]
    fn old_trust_section_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(
            &path,
            "[trust]\nfloor = 3\ntrust_uses_auto = true\n\n[log]\nlevel = \"debug\"\n",
        );
        let store = SettingsStore::new(path);
        // No error, no panic: [trust] section silently ignored.
        assert_eq!(store.settings().log.level.as_deref(), Some("debug"));
        // No trust-related fields leak through.
        assert_eq!(
            store.settings(),
            Settings {
                hydrus: HydrusSettings::default(),
                log: LogSettings {
                    level: Some("debug".into()),
                    console: None,
                    file: None,
                },
                net: NetSettings::default(),
                privacy: PrivacySettings::default(),
                repos: None,
            }
        );
    }

    #[test]
    fn reads_hydrus_from_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[hydrus]\ndir = \"/db\"\ntag_services = [1, 2]\n");
        let store = SettingsStore::new(path);
        let h = store.hydrus();
        assert_eq!(h.dir.as_deref(), Some("/db"));
        assert_eq!(h.tag_services, vec![1, 2]);
    }

    #[test]
    fn default_hydrus_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        let h = store.hydrus();
        assert_eq!(h.dir, None);
        assert!(h.tag_services.is_empty());
    }

    #[test]
    fn set_hydrus_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        store.set_hydrus(Some("/db"), &[9]).unwrap();
        let h = store.hydrus();
        assert_eq!(h.dir.as_deref(), Some("/db"));
        assert_eq!(h.tag_services, vec![9]);
        store.set_hydrus(None, &[]).unwrap();
        let h = store.hydrus();
        assert_eq!(h.dir, None);
        assert!(h.tag_services.is_empty());
    }

    #[test]
    fn set_hydrus_preserves_comments_and_unknown_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "# notes\n[future]\nsomething = \"keep me\"\n");
        let store = SettingsStore::new(path.clone());
        store.set_hydrus(Some("/db"), &[1]).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# notes"), "leading comment preserved");
        assert!(
            on_disk.contains("something = \"keep me\""),
            "unknown section preserved"
        );
        assert!(on_disk.contains("dir = \"/db\""), "hydrus dir written");
    }

    #[test]
    fn scaffold_writes_a_parseable_template_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let store = SettingsStore::new(path.clone());
        assert!(!path.exists());
        store.ensure_scaffold().unwrap();
        assert!(path.exists(), "scaffold created the file");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("[hydrus]"), "documents hydrus section");
        assert!(on_disk.contains("[log]"), "documents log section");
        assert!(on_disk.contains("[net]"), "documents net section");
        assert!(on_disk.contains("[privacy]"), "documents privacy section");
        assert!(on_disk.contains("[[repos]]"), "documents repos block");
        // Scaffold pre-configures the community repo; everything else is default.
        let settings = store.settings();
        let repos = settings
            .repos
            .as_ref()
            .expect("scaffold must include the default naiad-net repo");
        assert_eq!(repos.len(), 1, "exactly one default repo");
        assert_eq!(repos[0].name, "naiad-net");
        assert_eq!(repos[0].url, "https://v2202608398476500144.ultrasrv.de");
        assert_eq!(repos[0].max_query_bits, None);
        let expected = Settings {
            repos: settings.repos.clone(),
            ..Settings::default()
        };
        assert_eq!(settings, expected, "all other settings remain default");
    }

    #[test]
    fn scaffold_is_a_noop_when_the_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[hydrus]\ndir = \"/db\"\n");
        let store = SettingsStore::new(path.clone());
        store.ensure_scaffold().unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk, "[hydrus]\ndir = \"/db\"\n",
            "existing file untouched"
        );
    }

    #[test]
    fn reads_log_from_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[log]\nlevel = \"debug\"\nconsole = true\n");
        let store = SettingsStore::new(path);
        let l = store.log();
        assert_eq!(l.level.as_deref(), Some("debug"));
        assert_eq!(l.console, Some(true));
    }

    #[test]
    fn default_log_is_unset_and_console_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        let l = store.log();
        assert_eq!(l.level, None);
        // Absent in TOML → None; treat as false.
        assert_eq!(l.console, None);
    }

    #[test]
    fn scaffold_documents_the_log_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let store = SettingsStore::new(path.clone());
        store.ensure_scaffold().unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("[log]"), "documents log section");
        // Scaffold includes the default naiad-net repo; remaining settings are default.
        let settings = store.settings();
        let expected = Settings {
            repos: settings.repos.clone(),
            ..Settings::default()
        };
        assert_eq!(settings, expected, "all non-repos settings remain default");
    }

    #[test]
    fn migration_is_a_noop() {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        migrate_trust_floor_to_file(&db, &store).unwrap();
        assert_eq!(store.settings(), Settings::default());
    }

    #[test]
    fn default_net_is_local_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        assert!(!store.net().allow_remote, "default must be local-only");
    }

    #[test]
    fn net_allow_remote_parses_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[net]\nallow_remote = true\n");
        let store = SettingsStore::new(path);
        assert!(store.net().allow_remote);
    }

    #[test]
    fn old_file_without_net_section_defaults_to_local_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[hydrus]\ndir = \"/db\"\n[log]\nlevel = \"info\"\n");
        let store = SettingsStore::new(path);
        assert!(
            !store.net().allow_remote,
            "absent [net] must default to false"
        );
    }

    #[test]
    fn default_privacy_is_24() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("naiad.toml"));
        assert_eq!(store.privacy().max_query_bits, 24);
    }

    #[test]
    fn privacy_max_query_bits_parses_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[privacy]\nmax_query_bits = 40\n");
        let store = SettingsStore::new(path);
        assert_eq!(store.privacy().max_query_bits, 40);
    }

    #[test]
    fn old_file_without_privacy_section_defaults_to_24() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[hydrus]\ndir = \"/db\"\n[log]\nlevel = \"info\"\n");
        let store = SettingsStore::new(path);
        assert_eq!(store.privacy().max_query_bits, 24);
    }

    #[test]
    fn default_privacy_via_settings_default() {
        // Guards against a stray #[derive(Default)] regressing the field to 0.
        assert_eq!(Settings::default().privacy.max_query_bits, 24);
    }

    #[test]
    fn repos_parse_from_array_of_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(
            &path,
            "[[repos]]\nname = \"a\"\nurl = \"http://a\"\n\n[[repos]]\nname = \"b\"\nurl = \"http://b\"\n",
        );
        let store = SettingsStore::new(path);
        let repos = store.settings().repos.unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "a");
        assert_eq!(repos[1].url, "http://b");
    }

    #[test]
    fn absent_repos_key_is_none_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "[log]\nlevel = \"info\"\n");
        let store = SettingsStore::new(path);
        assert_eq!(store.settings().repos, None, "no [[repos]] key at all");
    }

    #[test]
    fn set_repos_round_trips_and_preserves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "# notes\n[log]\nlevel = \"debug\"\n");
        let store = SettingsStore::new(path.clone());
        store
            .set_repos(&[RepoEntry {
                name: "testrepo".into(),
                url: "http://127.0.0.1:9190".into(),
                max_query_bits: None,
            }])
            .unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# notes"), "comment preserved");
        assert!(
            on_disk.contains("level = \"debug\""),
            "other section preserved"
        );
        assert!(on_disk.contains("[[repos]]"), "array-of-tables written");
        assert_eq!(store.settings().repos.unwrap().len(), 1);

        store.set_repos(&[]).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("[[repos]]"),
            "empty list removes the section"
        );
        assert!(on_disk.contains("# notes"), "comment still preserved");
    }

    #[test]
    fn settings_strict_flags_a_malformed_file_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let store = SettingsStore::new(path.clone());
        assert!(
            store.settings_strict().unwrap().is_none(),
            "absent = Ok(None)"
        );
        write_file(&path, "this is { not valid toml ===");
        assert!(
            store.settings_strict().is_err(),
            "malformed = Err, never defaults"
        );
    }

    /// `set_repos` on a nonexistent file must create the file containing the
    /// `[[repos]]` section and the entry must round-trip correctly.
    #[test]
    fn set_repos_on_nonexistent_file_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let store = SettingsStore::new(path.clone());
        assert!(!path.exists(), "precondition: file absent");
        store
            .set_repos(&[RepoEntry {
                name: "created".into(),
                url: "http://127.0.0.1:9191".into(),
                max_query_bits: None,
            }])
            .unwrap();
        assert!(path.exists(), "file was created");
        let repos = store.settings().repos.unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "created");
        assert_eq!(repos[0].url, "http://127.0.0.1:9191");
    }

    /// Regression for #168: `set_repos` on a freshly scaffolded file must append
    /// the `[[repos]]` block at document level — never *inside* the trailing
    /// `[privacy]` section, which would capture that section's commented keys
    /// (and any later-uncommented key) into the repos table.
    #[test]
    fn set_repos_on_scaffold_keeps_repos_below_privacy_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let store = SettingsStore::new(path.clone());
        // Fresh scaffold, exactly as first-run writes it.
        store.ensure_scaffold().unwrap();

        store
            .set_repos(&[RepoEntry {
                name: "repo1".into(),
                url: "http://127.0.0.1:9090".into(),
                max_query_bits: None,
            }])
            .unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();

        // The privacy comment block must still precede the repos block, i.e. the
        // `[[repos]]` header must not be wedged between `[privacy]` and its keys.
        let privacy_hdr = on_disk.find("[privacy]").expect("privacy header present");
        let privacy_comment = on_disk
            .find("# max_query_bits = 24")
            .expect("privacy documents its key");
        let repos_hdr = on_disk.find("[[repos]]").expect("repos block written");
        assert!(
            privacy_hdr < privacy_comment,
            "privacy header precedes its own commented key\n{on_disk}"
        );
        assert!(
            privacy_comment < repos_hdr,
            "the [[repos]] block must come AFTER the [privacy] comment block, \
             not inside the [privacy] section\n{on_disk}"
        );

        // And a privacy key uncommented *after* the edit must still belong to
        // [privacy], not to [[repos]] — parse the realistic follow-on edit.
        let uncommented = on_disk.replace("# max_query_bits = 24", "max_query_bits = 40");
        let settings: Settings =
            toml::from_str(&uncommented).expect("uncommenting a privacy key still parses");
        assert_eq!(
            settings.privacy.max_query_bits, 40,
            "the uncommented key resolves under [privacy], not swallowed by [[repos]]"
        );
        let repos = settings.repos.expect("repos parsed at document level");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "repo1");
    }

    /// `set_repos` on a malformed file must return `Err` and leave the original
    /// content on disk completely untouched.
    #[test]
    fn set_repos_with_malformed_file_returns_err_and_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let garbage = "this is { not valid toml ===";
        write_file(&path, garbage);
        let store = SettingsStore::new(path.clone());
        let result = store.set_repos(&[RepoEntry {
            name: "x".into(),
            url: "http://x".into(),
            max_query_bits: None,
        }]);
        assert!(result.is_err(), "malformed file must yield Err");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, garbage, "malformed content must remain untouched");
    }

    // ── per-repo max_query_bits tests ───────────────────────────────────────

    #[test]
    fn repo_entry_max_query_bits_parses_and_defaults() {
        let (s, _) = parse_settings(
            "[[repos]]\nname = \"a\"\nurl = \"http://x\"\nmax_query_bits = 256\n\n[[repos]]\nname = \"b\"\nurl = \"http://y\"\n",
        )
        .unwrap();
        let repos = s.repos.unwrap();
        assert_eq!(repos[0].max_query_bits, Some(256));
        assert_eq!(repos[1].max_query_bits, None);
    }

    #[test]
    fn effective_max_query_bits_override_and_clamp() {
        let mut r = RepoEntry {
            name: "a".into(),
            url: "u".into(),
            max_query_bits: None,
        };
        assert_eq!(r.effective_max_query_bits(24), 24); // unset -> global
        r.max_query_bits = Some(256);
        assert_eq!(r.effective_max_query_bits(24), 256); // full override up
        r.max_query_bits = Some(8);
        assert_eq!(r.effective_max_query_bits(24), 8); // full override down
        r.max_query_bits = Some(0);
        assert_eq!(r.effective_max_query_bits(24), 1); // clamped low
        r.max_query_bits = Some(4096);
        assert_eq!(r.effective_max_query_bits(24), 256); // clamped high
    }

    #[test]
    fn set_repos_round_trips_max_query_bits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        let store = SettingsStore::new(path.clone());
        store
            .set_repos(&[
                RepoEntry {
                    name: "snap".into(),
                    url: "http://127.0.0.1:9090".into(),
                    max_query_bits: Some(256),
                },
                RepoEntry {
                    name: "default".into(),
                    url: "http://127.0.0.1:9091".into(),
                    max_query_bits: None,
                },
            ])
            .unwrap();
        // (a) Settings round-trip via the store cache.
        let repos = store.settings().repos.unwrap();
        assert_eq!(repos[0].max_query_bits, Some(256));
        assert_eq!(repos[1].max_query_bits, None);
        // (b) Raw file text: key appears exactly once.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk.matches("max_query_bits").count(),
            1,
            "max_query_bits must appear exactly once in the file"
        );
    }

    // ── Unknown-key warning tests ────────────────────────────────────────────

    #[test]
    fn unknown_keys_returned_structurally() {
        // parse_settings must return the ignored keys as data, not just log them.
        let toml = "[log]\nlevel = \"info\"\nbogus_nested = 42\n[mystery]\nshiny = true\n";
        let (s, ignored) = parse_settings(toml).expect("must parse Ok");
        assert_eq!(s.log.level.as_deref(), Some("info"));
        assert!(
            ignored.contains(&"log.bogus_nested".to_string()),
            "nested unknown key must be in ignored list; got: {ignored:?}"
        );
        assert!(
            ignored
                .iter()
                .any(|k| k == "mystery" || k.starts_with("mystery.")),
            "unknown section must be in ignored list; got: {ignored:?}"
        );
    }

    #[test]
    fn scaffold_produces_zero_ignored_keys() {
        // The scaffold pre-configures the naiad-net community repo; every other
        // key is commented out.  The parser must see no unknown keys.
        let (s, ignored) = parse_settings(SCAFFOLD).expect("scaffold must parse Ok");
        // Repos field carries the default naiad-net entry; everything else is default.
        let repos = s
            .repos
            .as_ref()
            .expect("scaffold must include naiad-net repo");
        assert_eq!(repos.len(), 1, "exactly one default repo");
        assert_eq!(repos[0].name, "naiad-net");
        assert_eq!(repos[0].url, "https://v2202608398476500144.ultrasrv.de");
        let expected = Settings {
            repos: s.repos.clone(),
            ..Settings::default()
        };
        assert_eq!(s, expected, "all non-repos settings remain default");
        assert!(
            ignored.is_empty(),
            "scaffold must produce no ignored keys; got: {ignored:?}"
        );
    }

    #[test]
    fn db_hint_key_appears_in_ignored_list() {
        // Root-level "db" is in DB_HINT_KEYS and must appear in the returned Vec.
        let (_, ignored) = parse_settings("db = \"foo\"\n").expect("must parse Ok");
        assert!(
            ignored.iter().any(|k| k == "db"),
            "root 'db' key must be in ignored list; got: {ignored:?}"
        );

        // "colour" is not a hint key; it must appear in the list but NOT trigger
        // the hint (we verify structural presence, not the warn message).
        let (_, ignored2) = parse_settings("colour = \"mauve\"\n").expect("must parse Ok");
        assert!(
            ignored2.iter().any(|k| k == "colour"),
            "unknown root key 'colour' must be in ignored list; got: {ignored2:?}"
        );
    }

    #[test]
    fn cached_read_returns_empty_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        // A file with an unknown key so the first parse produces a non-empty Vec.
        write_file(&path, "[log]\nlevel = \"debug\"\nunknown_key = 1\n");
        let store = SettingsStore::new(path.clone());

        // Fresh parse: must return the ignored key structurally.
        let (first_settings, first_ignored) = store.settings_and_diagnostics();
        assert!(
            !first_ignored.is_empty(),
            "fresh parse must return ignored keys; got empty"
        );
        assert!(first_ignored.iter().any(|k| k == "log.unknown_key"));

        // Second call with same mtime: cache hit, diagnostics must be empty.
        let (second_settings, second_ignored) = store.settings_and_diagnostics();
        assert_eq!(first_settings, second_settings);
        assert!(
            second_ignored.is_empty(),
            "cached read must return empty diagnostics; got: {second_ignored:?}"
        );
    }

    #[test]
    fn an_unknown_key_warns_only_once_across_read_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.toml");
        write_file(&path, "db = \"elsewhere\"\n");
        let store = SettingsStore::new(path.clone());

        // Boot order: settings_strict (reconcile, bypasses the mtime cache) then
        // a normal read. Only the first may emit.
        store.settings_strict().expect("valid file parses");
        assert!(
            store.warn_once(&["db".to_string()], "label").is_empty(),
            "a key already warned about must not be re-emitted"
        );

        // A key that vanishes and comes back warns again: the set is replaced,
        // not accumulated.
        assert!(store.warn_once(&[], "label").is_empty());
        assert_eq!(
            store.warn_once(&["db".to_string()], "label"),
            vec!["db".to_string()],
            "a reintroduced key must warn again"
        );
    }
}
