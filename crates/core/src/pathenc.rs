//! Lossless conversion between a filesystem [`Path`] and a `Vec<u8>` BLOB.
//!
//! `std` exposes no portable raw-byte view of an `OsStr`, so we branch per OS:
//! on Unix the BLOB is the OS bytes directly; on Windows we serialize the
//! UTF-16 code units (`encode_wide`) as little-endian `u8` pairs. Both branches
//! round-trip losslessly on their own platform — including non-UTF-8 paths.
//! A path stored on Windows is NOT byte-identical to the same path on Unix;
//! that is acceptable (the requirement is same-platform round-trip). Both
//! `encode_wide`/`from_wide` are safe, so `unsafe_code = "forbid"` holds.

use std::path::{Path, PathBuf};

/// Encode `path` as a platform-specific byte blob suitable for a SQLite `BLOB`.
#[cfg(unix)]
#[must_use]
pub fn path_to_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// Decode a blob produced by [`path_to_bytes`] back into a [`PathBuf`].
#[cfg(unix)]
#[must_use]
pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

/// Encode `path` as little-endian UTF-16 code-unit pairs.
#[cfg(windows)]
#[must_use]
pub fn path_to_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// Decode little-endian UTF-16 code-unit pairs back into a [`PathBuf`].
///
/// A trailing odd byte (a corrupt blob) is dropped rather than panicking.
#[cfg(windows)]
#[must_use]
pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    PathBuf::from(OsString::from_wide(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_path_round_trips() {
        let p = PathBuf::from("/lib/photos/a.png");
        let bytes = path_to_bytes(&p);
        assert_eq!(path_from_bytes(&bytes), p);
    }

    #[test]
    fn non_ascii_path_round_trips() {
        // Mixed scripts + emoji exercise multi-byte UTF-8 / surrogate pairs.
        let p = PathBuf::from("/lib/日本語/Ω/🦀/файл.txt");
        let bytes = path_to_bytes(&p);
        assert_eq!(path_from_bytes(&bytes), p);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_round_trips_unix() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        // 0x80 is an invalid UTF-8 lead byte: a genuinely non-UTF-8 path.
        let os = OsString::from_vec(vec![b'/', b't', 0x80, b'x']);
        let p = PathBuf::from(os);
        let bytes = path_to_bytes(&p);
        assert_eq!(path_from_bytes(&bytes), p);
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_surrogate_path_round_trips_windows() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        // 0xD800 is a lone high surrogate: a non-UTF-8-representable path.
        let units: [u16; 3] = ['x' as u16, 0xD800, 'y' as u16];
        let p = PathBuf::from(OsString::from_wide(&units));
        let bytes = path_to_bytes(&p);
        assert_eq!(path_from_bytes(&bytes), p);
    }

    #[test]
    fn empty_path_round_trips() {
        let p = PathBuf::from("");
        let bytes = path_to_bytes(&p);
        assert_eq!(path_from_bytes(&bytes), p);
    }
}
