//! The daemon's account: an Ed25519 key file beside the library database. The
//! key lives in its own file (`naiad.key`), never inside `naiad.db` — honoring
//! "keypairs never in the library DB" (README §7). All crypto is in
//! `netproto::Account`; this module only resolves the path and loads it.
//!
//! ADR 0020 §6 adds a 32-byte master seed (`naiad.master`) beside the key.
//! Every per-repo contributor key is derived from it via BLAKE3, so different
//! repos see unrelated pseudonyms. The seed itself never enters `naiad.db` and
//! never leaves the machine.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use naiad_netproto::Account;

/// The key file path beside a database file: `<db dir>/naiad.key`.
#[must_use]
pub fn key_path_for(db_path: &Path) -> PathBuf {
    db_path.with_file_name("naiad.key")
}

/// The master-seed path beside a database file: `<db dir>/naiad.master`.
/// One 32-byte seed; every per-repo contributor key derives from it. Never in
/// naiad.db, never leaves the machine.
#[must_use]
pub fn master_path_for(db_path: &Path) -> PathBuf {
    db_path.with_file_name("naiad.master")
}

/// Load the account at `path` without creating one (the read-only `account`
/// view): `None` if no key exists yet.
///
/// # Errors
/// Returns an error if a present key file cannot be read.
pub fn load(path: &Path) -> Result<Option<Account>> {
    Account::load(path)
}

/// Load the 32-byte master seed, creating it on first need (same best-effort
/// 0600 discipline as `Account::save`).
///
/// Creation is atomic in BOTH existence and contents: the seed is written to a
/// uniquely-named temp file in the same directory, fully flushed to disk, then
/// published with a no-clobber rename (`persist_noclobber`). A concurrent caller
/// therefore never observes a half-written `naiad.master` — the file appears only
/// once it already holds all 32 bytes. The no-clobber semantics elect a single
/// first-writer: `std::fs::rename` would clobber on both Unix and Windows (see
/// `settings::write_atomic`), so a plain rename could let a losing thread
/// overwrite the winner's seed and permanently break the pseudonym for any anchor
/// frozen during the race. `persist_noclobber` instead fails with `AlreadyExists`
/// when another caller won, and we fall through to reading the winner's seed.
///
/// # Errors
/// Returns an error if the file cannot be read/written or has a wrong length.
pub fn load_or_create_master(path: &Path) -> Result<[u8; 32]> {
    let seed = Account::generate().secret_bytes(); // 32 fresh random bytes; discarded if the file already exists

    // Write to a uniquely-named temp file in the SAME directory as the target so
    // the publish is a rename within one filesystem.
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let mut tmp = match parent {
        Some(dir) => tempfile::NamedTempFile::new_in(dir),
        None => tempfile::NamedTempFile::new_in("."),
    }
    .map_err(|e| {
        anyhow::anyhow!(e).context(format!(
            "creating temp file for master seed at {}",
            path.display()
        ))
    })?;

    tmp.write_all(&seed)?;
    // The seed is unrecoverable if lost, so flush it to disk before publishing.
    tmp.as_file().sync_all()?;

    match tmp.persist_noclobber(path) {
        Ok(_file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(seed)
        }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another thread (or a previous run) already created the seed. Our
            // temp file (held in `e.file`) is dropped and auto-removed here; read
            // the winner's fully-written seed.
            let bytes = std::fs::read(path)?;
            <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| anyhow::anyhow!("master seed at {} is not 32 bytes", path.display()))
        }
        Err(e) => Err(anyhow::anyhow!(e.error)
            .context(format!("persisting master seed at {}", path.display()))),
    }
}

/// Derive the per-repo contributor account: BLAKE3 KDF over master ‖ anchor
/// under a fixed domain context. Deterministic — nothing needs persisting.
#[must_use]
pub fn derive_contributor(master: &[u8; 32], repo_anchor: &str) -> Account {
    let mut material = Vec::with_capacity(32 + repo_anchor.len());
    material.extend_from_slice(master);
    material.extend_from_slice(repo_anchor.as_bytes());
    let seed = blake3::derive_key("naiad-contributor:v1", &material);
    Account::from_secret_bytes(&seed)
}

/// Normalize a repo URL for anchor use.
///
/// Only the scheme and host are lowercased; path/query components are
/// preserved as-is so that two URLs differing only by path casing are NOT
/// collapsed to the same anchor (path components are case-sensitive on most
/// servers).  A trailing slash on the path is stripped.
///
/// Parsing is minimal (split on `"://"` then first `"/"`); no URL-crate
/// dependency is added.
///
/// # Stability note
/// Changing this function's output is safe for existing subscriptions ONLY
/// because `services.repo_anchor` is write-once frozen (see
/// `freeze_repo_anchor` in daemon `ops.rs`).  If that freeze is ever relaxed,
/// a change here would re-derive anchors from scratch and orphan all derived
/// pseudonyms that were created under the old normalization.
#[must_use]
pub fn normalize_repo_url(url: &str) -> String {
    let url = url.trim_end_matches('/');
    // Split on the first "://" to isolate the scheme.
    if let Some((scheme, rest)) = url.split_once("://") {
        // Split on the first "/" after the authority (host[:port]) to
        // separate the host from the path.
        if let Some((host, path)) = rest.split_once('/') {
            // Lowercase only scheme + host; preserve path.
            format!(
                "{}://{}/{}",
                scheme.to_lowercase(),
                host.to_lowercase(),
                path
            )
        } else {
            // No path component — lowercase scheme + host only.
            format!("{}://{}", scheme.to_lowercase(), rest.to_lowercase())
        }
    } else {
        // No "://" — fall back to full lowercase (e.g. bare hostnames).
        url.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_path_sits_beside_the_database() {
        let p = key_path_for(Path::new("/lib/naiad.db"));
        assert_eq!(p, PathBuf::from("/lib/naiad.key"));
    }

    #[test]
    fn master_path_sits_beside_the_database() {
        let p = master_path_for(Path::new("/lib/naiad.db"));
        assert_eq!(p, PathBuf::from("/lib/naiad.master"));
    }

    #[test]
    fn load_is_none_until_create_then_some() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.key");
        assert!(load(&path).unwrap().is_none());
        let made = Account::load_or_create(&path).unwrap();
        assert_eq!(
            load(&path).unwrap().unwrap().public_hex(),
            made.public_hex()
        );
    }

    #[test]
    fn derivation_is_deterministic_and_distinct_per_anchor() {
        let master = [7u8; 32];
        let a1 = derive_contributor(&master, "aa".repeat(32).as_str());
        let a2 = derive_contributor(&master, "aa".repeat(32).as_str());
        let b = derive_contributor(&master, "bb".repeat(32).as_str());
        let other_master = derive_contributor(&[8u8; 32], "aa".repeat(32).as_str());
        assert_eq!(
            a1.public_hex(),
            a2.public_hex(),
            "same (master, anchor) = same pseudonym"
        );
        assert_ne!(
            a1.public_hex(),
            b.public_hex(),
            "different room, different pseudonym"
        );
        assert_ne!(a1.public_hex(), other_master.public_hex());
    }

    #[test]
    fn master_seed_loads_or_creates_beside_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("naiad.db");
        assert_eq!(master_path_for(&db), dir.path().join("naiad.master"));
        let m1 = load_or_create_master(&master_path_for(&db)).unwrap();
        let m2 = load_or_create_master(&master_path_for(&db)).unwrap();
        assert_eq!(m1, m2, "created once, stable thereafter");
    }

    /// Two threads calling `load_or_create_master` concurrently on a brand-new
    /// path must both receive the SAME 32-byte seed (the first-writer wins) and
    /// the file must be written exactly once (no overwrite).
    #[test]
    fn concurrent_first_use_yields_the_same_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.master");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let path1 = path.clone();
        let path2 = path.clone();
        let b1 = std::sync::Arc::clone(&barrier);
        let b2 = std::sync::Arc::clone(&barrier);

        let h1 = std::thread::spawn(move || {
            b1.wait();
            load_or_create_master(&path1).unwrap()
        });
        let h2 = std::thread::spawn(move || {
            b2.wait();
            load_or_create_master(&path2).unwrap()
        });

        let s1 = h1.join().unwrap();
        let s2 = h2.join().unwrap();

        assert_eq!(s1, s2, "both callers must return the same seed");
        // The file must contain exactly one of them (same value either way).
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len(), 32, "file holds exactly 32 bytes");
        assert_eq!(on_disk.as_slice(), s1, "file matches the returned seed");
    }

    #[test]
    fn url_fallback_normalizes() {
        assert_eq!(
            normalize_repo_url("HTTP://Repo.Example:9090/"),
            "http://repo.example:9090"
        );
        assert_eq!(
            normalize_repo_url("https://repo.example/path/"),
            "https://repo.example/path"
        );
        // Already normalized: idempotent.
        assert_eq!(
            normalize_repo_url("http://repo.example:9090"),
            "http://repo.example:9090"
        );
    }

    #[test]
    fn normalize_repo_url_lowercases_host_preserves_path_casing() {
        // Host is case-insensitive; path is case-sensitive on most servers.
        // Two URLs differing only by path casing must NOT collapse to the same
        // anchor — they could point to distinct repos.
        let a = normalize_repo_url("HTTPS://REPO.EXAMPLE/MyRepo/Tags");
        let b = normalize_repo_url("HTTPS://REPO.EXAMPLE/myrepo/tags");
        assert_ne!(a, b, "path casing must be preserved, not collapsed");

        // Scheme and host are still lowercased.
        assert_eq!(a, "https://repo.example/MyRepo/Tags");
        assert_eq!(b, "https://repo.example/myrepo/tags");
    }

    #[test]
    fn normalize_repo_url_trailing_slash_stripped() {
        assert_eq!(
            normalize_repo_url("https://repo.example/path/"),
            "https://repo.example/path"
        );
        // No path
        assert_eq!(
            normalize_repo_url("https://repo.example/"),
            "https://repo.example"
        );
    }
}
