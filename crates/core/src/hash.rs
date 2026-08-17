use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::Error;

/// A BLAKE3-256 content hash — Naiad's primary file identity.
///
/// BLAKE3 is chosen over SHA-256 for raw speed (the headline requirement). The
/// hash is a fixed 32-byte digest, displayed and parsed as 64 lowercase hex
/// characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Construct a hash from raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The hash as a 64-character lowercase hex string.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for byte in self.0 {
            // Two lowercase hex digits per byte.
            s.push(char::from_digit(u32::from(byte >> 4), 16).unwrap());
            s.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap());
        }
        s
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl FromStr for Hash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(Error::InvalidHashHex(s.to_string()));
        }
        let mut bytes = [0u8; 32];
        let raw = s.as_bytes();
        for (i, byte) in bytes.iter_mut().enumerate() {
            let hi = hex_val(raw[i * 2]).ok_or_else(|| Error::InvalidHashHex(s.to_string()))?;
            let lo = hex_val(raw[i * 2 + 1]).ok_or_else(|| Error::InvalidHashHex(s.to_string()))?;
            *byte = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

/// Parse a single hex digit (lower or upper case) into its 0-15 value.
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Hash a byte slice.
#[must_use]
pub fn hash_bytes(data: &[u8]) -> Hash {
    Hash(*blake3::hash(data).as_bytes())
}

/// Hash a reader by streaming it through BLAKE3 — constant memory regardless of
/// input size, so it is safe on large media files.
///
/// # Errors
/// Returns any I/O error encountered while reading from `reader`.
pub fn hash_reader<R: Read>(mut reader: R) -> io::Result<Hash> {
    let mut hasher = blake3::Hasher::new();
    // 64 KiB buffer balances syscall overhead against memory use.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Hash(*hasher.finalize().as_bytes()))
}

/// Hash a reader through BLAKE3 **and** SHA-256 in a single streaming pass —
/// constant memory regardless of input size. BLAKE3 is Naiad's identity; the
/// SHA-256 hex is the interop key (Hydrus, torrents). Returns the BLAKE3 [`Hash`]
/// and the 64-char lowercase SHA-256 hex.
///
/// # Errors
/// Returns any I/O error encountered while reading from `reader`.
pub fn hash_reader_dual<R: Read>(mut reader: R) -> io::Result<(Hash, String)> {
    let mut blake = blake3::Hasher::new();
    let mut sha = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        blake.update(&buf[..n]);
        sha.update(&buf[..n]);
    }
    let blake = Hash::from_bytes(*blake.finalize().as_bytes());
    let sha = hex::encode(sha.finalize());
    Ok((blake, sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known BLAKE3 test vector for the empty input.
    const EMPTY_HEX: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    // Known BLAKE3 test vector for "abc".
    const ABC_HEX: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

    #[test]
    fn empty_input_matches_known_vector() {
        assert_eq!(hash_bytes(b"").to_hex(), EMPTY_HEX);
    }

    #[test]
    fn abc_matches_known_vector() {
        assert_eq!(hash_bytes(b"abc").to_hex(), ABC_HEX);
    }

    #[test]
    fn reader_and_bytes_agree() {
        let data = b"the quick brown fox";
        let via_bytes = hash_bytes(data);
        let via_reader = hash_reader(&data[..]).unwrap();
        assert_eq!(via_bytes, via_reader);
    }

    #[test]
    fn hex_round_trip() {
        let h = hash_bytes(b"round trip me");
        let parsed: Hash = h.to_hex().parse().unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn from_str_accepts_uppercase() {
        let lower: Hash = ABC_HEX.parse().unwrap();
        let upper: Hash = ABC_HEX.to_uppercase().parse().unwrap();
        assert_eq!(lower, upper);
    }

    #[test]
    fn from_str_rejects_bad_length_and_chars() {
        assert!("abc".parse::<Hash>().is_err());
        let mut bad = ABC_HEX.to_string();
        bad.replace_range(0..1, "z");
        assert!(bad.parse::<Hash>().is_err());
    }

    // Known SHA-256 of "abc".
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn dual_hash_matches_blake3_and_sha256_vectors() {
        let data = b"abc";
        let (blake, sha) = hash_reader_dual(&data[..]).unwrap();
        assert_eq!(blake.to_hex(), ABC_HEX);
        assert_eq!(sha, ABC_SHA256);
    }
}
