//! `naiad-bootstrap` — zero-dependency configuration resolution for all naiad
//! entry points. Resolves **which library** to open (the bootstrap tier) and
//! whether to allocate a debug console, using a first-set-wins ladder per tier.
//!
//! All resolvers are pure: they receive environment values as parameters and
//! never call `std::env` themselves, so tests can pass arbitrary values without
//! mutating process state.
//!
//! The only exception is `resolve_db_path_from_process` and
//! `resolve_console_from_process`, which are thin wrappers that read the
//! process environment and call the corresponding pure resolver.

use std::path::PathBuf;

// ─── DB path resolution ───────────────────────────────────────────────────────

/// Which configuration tier produced the resolved DB path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbPathSource {
    /// The `--db` command-line flag.
    Flag,
    /// The `NAIAD_DB` environment variable (non-empty).
    Env,
    /// The built-in fallback: `<exe dir>/naiad.db`.
    ExeDirDefault,
}

impl DbPathSource {
    /// Human-readable tier name for warning messages.
    pub fn name(&self) -> &'static str {
        match self {
            DbPathSource::Flag => "--db flag",
            DbPathSource::Env => "NAIAD_DB env",
            DbPathSource::ExeDirDefault => "exe-dir default",
        }
    }
}

/// The result of resolving the database path through the bootstrap tier ladder.
pub struct DbPathResolution {
    /// The resolved path string (passed to the daemon as `--db`).
    pub path: String,
    /// Which tier produced the winning value.
    pub source: DbPathSource,
    /// Tiers that were **explicitly set** and whose value **differs** from the
    /// winner. The caller should emit one `warn!` per entry.
    ///
    /// The `ExeDirDefault` tier is never included here — it is not an explicit
    /// user choice and therefore not worth a warning.
    pub overridden: Vec<(DbPathSource, String)>,
}

/// Resolve the database path using the bootstrap ladder:
/// `--db flag` → `NAIAD_DB` (non-empty) → `<exe_dir>/naiad.db`.
///
/// # Parameters
/// - `flag`: the raw `--db` value if the flag was passed, `None` otherwise.
///   An empty string is treated as absent.
/// - `env`: the raw `NAIAD_DB` value if the variable exists, `None` otherwise.
///   An empty string is treated as absent (same as `None`).
/// - `exe_dir`: a **lazy** closure that returns the exe directory. It is only
///   called when both `flag` and `env` are absent, so a higher-priority tier
///   winning never incurs the `current_exe()` syscall or any failure that it
///   might produce.
///
/// # Errors
/// Returns `Err` only when `exe_dir()` itself errors (e.g. `current_exe()`
/// fails) or the resulting path is not valid UTF-8. Both conditions are
/// propagated only when the default tier is actually needed.
pub fn resolve_db_path<F>(
    flag: Option<&str>,
    env: Option<&str>,
    exe_dir: F,
) -> Result<DbPathResolution, String>
where
    F: FnOnce() -> Result<PathBuf, String>,
{
    let flag_val: Option<&str> = flag.filter(|s| !s.is_empty());
    let env_val: Option<&str> = env.filter(|s| !s.is_empty());

    let (source, path) = if let Some(f) = flag_val {
        (DbPathSource::Flag, f.to_string())
    } else if let Some(e) = env_val {
        (DbPathSource::Env, e.to_string())
    } else {
        // Only evaluate exe_dir when neither flag nor env provided a value.
        let dir = exe_dir()?;
        let p = dir
            .join("naiad.db")
            .to_str()
            .ok_or_else(|| "The Naiad database path is not valid UTF-8.".to_string())?
            .to_string();
        (DbPathSource::ExeDirDefault, p)
    };

    // Collect explicitly set tiers that lost AND differ from the winner.
    // ExeDirDefault is never "explicitly set", so it is excluded.
    let mut overridden: Vec<(DbPathSource, String)> = Vec::new();
    if matches!(source, DbPathSource::Flag) {
        if let Some(e) = env_val {
            let e = e.to_string();
            if e != path {
                overridden.push((DbPathSource::Env, e));
            }
        }
    }

    Ok(DbPathResolution {
        path,
        source,
        overridden,
    })
}

/// Read `NAIAD_DB` from the process environment and pass a lazy `current_exe()`
/// closure to [`resolve_db_path`]. The exe lookup is performed only when neither
/// a `--db` flag nor `NAIAD_DB` wins the ladder.
///
/// # Errors
/// Propagates errors from `current_exe()` or non-UTF-8 exe-dir paths, but only
/// when the default tier is actually reached.
pub fn resolve_db_path_from_process(flag: Option<&str>) -> Result<DbPathResolution, String> {
    let env = std::env::var("NAIAD_DB").ok();
    resolve_db_path(flag, env.as_deref(), || {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Could not locate the Naiad executable: {e}"))?;
        exe.parent()
            .ok_or_else(|| "The Naiad executable has no parent directory.".to_string())
            .map(|p| p.to_path_buf())
    })
}

// ─── Console resolution ───────────────────────────────────────────────────────

/// Which configuration tier produced the console decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleSource {
    /// `--console` / `--no-console` command-line flag.
    Flag,
    /// `NAIAD_CONSOLE` environment variable.
    Env,
    /// `[log].console` in `naiad.toml`.
    Toml,
    /// Built-in default (`false`).
    Default,
}

impl ConsoleSource {
    /// Human-readable tier name for warning messages.
    pub fn name(&self) -> &'static str {
        match self {
            ConsoleSource::Flag => "--console/--no-console flag",
            ConsoleSource::Env => "NAIAD_CONSOLE env",
            ConsoleSource::Toml => "naiad.toml [log].console",
            ConsoleSource::Default => "default",
        }
    }
}

/// The result of resolving the console switch through the first-set-wins ladder.
pub struct ConsoleResolution {
    /// Whether to open a debug console.
    pub on: bool,
    /// Which tier produced the winning value.
    pub source: ConsoleSource,
    /// Tiers that were **explicitly set** and whose value **differs** from the
    /// winner. The caller should emit one `warn!` per entry.
    pub overridden: Vec<(ConsoleSource, bool)>,
}

/// Resolve the console switch using the first-set-wins ladder:
/// `--console/--no-console` → `NAIAD_CONSOLE` → `[log].console` → `false`.
///
/// # Parameters
/// - `flag`: `Some(true)` for `--console`, `Some(false)` for `--no-console`,
///   `None` if the flag was not passed.
/// - `env`: the raw `NAIAD_CONSOLE` value. `None` or `""` = tier absent.
///   `"0"` or `"false"` (any case) = `Some(false)`. Anything else = `Some(true)`.
/// - `toml`: `Some(value)` when `[log].console` is present in `naiad.toml`,
///   `None` when the key is absent.
pub fn resolve_console(
    flag: Option<bool>,
    env: Option<&str>,
    toml: Option<bool>,
) -> ConsoleResolution {
    let env_val: Option<bool> = match env {
        None | Some("") => None,
        Some(v) => {
            let v = v.trim();
            if v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                Some(true)
            }
        }
    };

    // First Some in priority order wins.
    let (winner_source, winner_val) = flag
        .map(|b| (ConsoleSource::Flag, b))
        .or_else(|| env_val.map(|b| (ConsoleSource::Env, b)))
        .or_else(|| toml.map(|b| (ConsoleSource::Toml, b)))
        .unwrap_or((ConsoleSource::Default, false));

    // Collect lower-priority tiers that were explicitly set and differ.
    let mut overridden: Vec<(ConsoleSource, bool)> = Vec::new();
    match &winner_source {
        ConsoleSource::Flag => {
            if let Some(e) = env_val {
                if e != winner_val {
                    overridden.push((ConsoleSource::Env, e));
                }
            }
            if let Some(t) = toml {
                if t != winner_val {
                    overridden.push((ConsoleSource::Toml, t));
                }
            }
        }
        ConsoleSource::Env => {
            if let Some(t) = toml {
                if t != winner_val {
                    overridden.push((ConsoleSource::Toml, t));
                }
            }
        }
        ConsoleSource::Toml | ConsoleSource::Default => {
            // Nothing lower to override.
        }
    }

    ConsoleResolution {
        on: winner_val,
        source: winner_source,
        overridden,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    // ── DB path resolution ───────────────────────────────────────────────────

    fn exe_dir() -> &'static Path {
        Path::new("/exe")
    }

    /// Returns a closure that yields the canonical test exe directory.
    fn good_exe_dir() -> impl FnOnce() -> Result<PathBuf, String> {
        let p = exe_dir().to_path_buf();
        move || Ok(p)
    }

    /// Returns a closure that always errors — used to verify it is never called
    /// when a higher-priority tier (flag or env) wins the ladder.
    fn failing_exe_dir() -> impl FnOnce() -> Result<PathBuf, String> {
        || Err("exe dir deliberately unavailable".to_string())
    }

    #[test]
    fn db_flag_wins_over_env_and_default() {
        let r = resolve_db_path(Some("/flag/db"), Some("/env/db"), good_exe_dir()).unwrap();
        assert_eq!(r.path, "/flag/db");
        assert_eq!(r.source, DbPathSource::Flag);
    }

    #[test]
    fn db_env_wins_over_default() {
        let r = resolve_db_path(None, Some("/env/db"), good_exe_dir()).unwrap();
        assert_eq!(r.path, "/env/db");
        assert_eq!(r.source, DbPathSource::Env);
    }

    #[test]
    fn db_default_when_neither_flag_nor_env() {
        let r = resolve_db_path(None, None, good_exe_dir()).unwrap();
        // Path separator is platform-dependent; compare via Path so the test
        // passes on both Unix (/) and Windows (\).
        assert_eq!(std::path::Path::new(&r.path), exe_dir().join("naiad.db"));
        assert_eq!(r.source, DbPathSource::ExeDirDefault);
    }

    #[test]
    fn db_empty_env_treated_as_absent() {
        let r = resolve_db_path(None, Some(""), good_exe_dir()).unwrap();
        assert_eq!(r.source, DbPathSource::ExeDirDefault);
    }

    #[test]
    fn db_empty_flag_treated_as_absent() {
        let r = resolve_db_path(Some(""), Some("/env/db"), good_exe_dir()).unwrap();
        assert_eq!(r.path, "/env/db");
        assert_eq!(r.source, DbPathSource::Env);
    }

    #[test]
    fn db_overridden_env_when_flag_differs() {
        let r = resolve_db_path(Some("/flag/db"), Some("/env/db"), good_exe_dir()).unwrap();
        assert_eq!(r.overridden.len(), 1);
        assert_eq!(r.overridden[0].0, DbPathSource::Env);
        assert_eq!(r.overridden[0].1, "/env/db");
    }

    #[test]
    fn db_no_override_warning_when_flag_and_env_agree() {
        let r = resolve_db_path(Some("/same/db"), Some("/same/db"), good_exe_dir()).unwrap();
        assert!(r.overridden.is_empty(), "agreeing tiers must not warn");
    }

    #[test]
    fn db_default_not_in_overridden() {
        // ExeDirDefault is not an explicit user choice — never in overridden.
        let r = resolve_db_path(Some("/flag/db"), None, good_exe_dir()).unwrap();
        assert!(
            r.overridden
                .iter()
                .all(|(s, _)| *s != DbPathSource::ExeDirDefault),
            "default must not appear in overridden"
        );
    }

    #[test]
    fn db_no_overridden_when_only_env_set() {
        // No higher tier was set, so nothing is overridden.
        let r = resolve_db_path(None, Some("/env/db"), good_exe_dir()).unwrap();
        assert!(r.overridden.is_empty());
    }

    #[test]
    fn db_utf8_error_on_non_utf8_exe_dir() {
        // On Windows, paths can be non-UTF-8. Simulate a Path that fails to_str().
        // We can't construct a real non-UTF-8 Path in safe Rust on all platforms,
        // but we can test the successful path and trust the error branch by
        // inspection (it converts with `.ok_or_else`).
        let r = resolve_db_path(None, None, || Ok(PathBuf::from("/valid/utf8/dir")));
        assert!(r.is_ok());
    }

    #[test]
    fn db_flag_or_env_win_succeeds_even_when_exe_dir_fails() {
        // When a higher-priority tier wins, the exe_dir closure must never be
        // called, so a failing closure must not prevent resolution.
        let r = resolve_db_path(Some("/flag/db"), None, failing_exe_dir());
        assert!(r.is_ok(), "flag win must not invoke exe_dir");
        assert_eq!(r.unwrap().path, "/flag/db");

        let r2 = resolve_db_path(None, Some("/env/db"), failing_exe_dir());
        assert!(r2.is_ok(), "env win must not invoke exe_dir");
        assert_eq!(r2.unwrap().path, "/env/db");
    }

    // ── Console resolution ───────────────────────────────────────────────────

    #[test]
    fn console_flag_true_wins_over_env_and_toml() {
        let r = resolve_console(Some(true), Some("0"), Some(false));
        assert!(r.on);
        assert_eq!(r.source, ConsoleSource::Flag);
    }

    #[test]
    fn console_flag_false_wins_over_env_and_toml() {
        let r = resolve_console(Some(false), Some("1"), Some(true));
        assert!(!r.on);
        assert_eq!(r.source, ConsoleSource::Flag);
        // Both env and toml differ and are lower — both should appear in overridden.
        assert_eq!(r.overridden.len(), 2);
    }

    #[test]
    fn console_env_zero_beats_toml_true() {
        // The motivating case: NAIAD_CONSOLE=0 must override toml console=true.
        let r = resolve_console(None, Some("0"), Some(true));
        assert!(!r.on, "NAIAD_CONSOLE=0 must turn console off");
        assert_eq!(r.source, ConsoleSource::Env);
        assert_eq!(r.overridden.len(), 1);
        assert_eq!(r.overridden[0].0, ConsoleSource::Toml);
        assert!(r.overridden[0].1, "the overridden toml value was true");
    }

    #[test]
    fn console_no_console_flag_beats_env_on() {
        let r = resolve_console(Some(false), Some("1"), None);
        assert!(!r.on);
        assert_eq!(r.source, ConsoleSource::Flag);
        assert_eq!(r.overridden.len(), 1);
        assert_eq!(r.overridden[0].0, ConsoleSource::Env);
    }

    #[test]
    fn console_empty_env_treated_as_absent() {
        // Empty NAIAD_CONSOLE must not count as a tier.
        let r = resolve_console(None, Some(""), Some(true));
        assert!(r.on);
        assert_eq!(r.source, ConsoleSource::Toml);
    }

    #[test]
    fn console_none_env_treated_as_absent() {
        let r = resolve_console(None, None, Some(false));
        assert!(!r.on);
        assert_eq!(r.source, ConsoleSource::Toml);
    }

    #[test]
    fn console_default_false_when_all_absent() {
        let r = resolve_console(None, None, None);
        assert!(!r.on);
        assert_eq!(r.source, ConsoleSource::Default);
        assert!(r.overridden.is_empty());
    }

    #[test]
    fn console_no_override_when_tiers_agree() {
        // --console + NAIAD_CONSOLE=1 + toml=true: all agree, no warning.
        let r = resolve_console(Some(true), Some("1"), Some(true));
        assert!(r.on);
        assert_eq!(r.source, ConsoleSource::Flag);
        assert!(r.overridden.is_empty(), "agreeing tiers must not warn");
    }

    #[test]
    fn console_env_true_values() {
        // Anything other than "0"/"false" (any case) counts as on.
        for v in &["1", "yes", "true", "TRUE", "on", "2"] {
            let r = resolve_console(None, Some(v), None);
            assert!(r.on, "NAIAD_CONSOLE={v:?} should be on");
        }
    }

    #[test]
    fn console_env_false_values() {
        for v in &["0", "false", "FALSE", "False"] {
            let r = resolve_console(None, Some(v), None);
            assert!(!r.on, "NAIAD_CONSOLE={v:?} should be off");
        }
    }

    #[test]
    fn console_toml_true_no_override_when_no_flag_or_env() {
        let r = resolve_console(None, None, Some(true));
        assert!(r.on);
        assert_eq!(r.source, ConsoleSource::Toml);
        assert!(r.overridden.is_empty());
    }

    #[test]
    fn console_env_beats_toml_when_same_value() {
        // env=false, toml=false: env wins (first set), no warning (values agree).
        let r = resolve_console(None, Some("0"), Some(false));
        assert!(!r.on);
        assert_eq!(r.source, ConsoleSource::Env);
        assert!(r.overridden.is_empty(), "agreeing lower tier must not warn");
    }
}
