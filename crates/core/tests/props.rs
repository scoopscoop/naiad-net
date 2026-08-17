//! Property-based tests (README §10) for core invariants: tag normalization,
//! hash hex round-trips, bucket math laws, and path encoding.

use std::path::PathBuf;

use naiad_core::{Hash, Tag, bucket_key, bucket_upper, path_from_bytes, path_to_bytes};
use proptest::prelude::*;

/// Mask a 32-byte array to its top `bits` bits (mirror of bucket_key's mask,
/// kept independent so the test doesn't reuse the code under test).
fn mask_bytes(mut b: [u8; 32], bits: u32) -> [u8; 32] {
    let bits = bits.min(256) as usize;
    for (i, byte) in b.iter_mut().enumerate() {
        let bit_start = i * 8;
        if bit_start + 8 <= bits {
            continue; // fully kept
        }
        if bit_start >= bits {
            *byte = 0; // fully masked
        } else {
            let keep = (bits - bit_start) as u32;
            *byte &= 0xffu8 << (8 - keep);
        }
    }
    b
}

proptest! {
    #[test]
    fn tag_parse_never_panics(s in ".*") {
        let _ = Tag::parse(&s);
    }

    #[test]
    fn tag_display_round_trips(s in ".*") {
        if let Ok(t) = Tag::parse(&s) {
            let back = Tag::parse(&t.to_string()).unwrap();
            prop_assert_eq!(t, back);
        }
    }

    /// Exercise leading-colon shapes that the general `".*"` generator rarely hits.
    /// Ensures that emoticon-style and double-colon inputs round-trip: if `parse`
    /// accepts `s`, then `parse(display(parse(s))) == parse(s)`.
    #[test]
    fn tag_display_round_trips_colon_heavy(s in "[:a-z )]{0,10}") {
        if let Ok(t) = Tag::parse(&s) {
            let displayed = t.to_string();
            let back = Tag::parse(&displayed).unwrap();
            prop_assert_eq!(t, back);
        }
    }

    #[test]
    fn parsed_tags_are_normalized(s in ".*") {
        if let Ok(t) = Tag::parse(&s) {
            // Normalization is idempotent: the canonical form re-parses to itself
            // and carries no uppercase or doubled interior whitespace.
            prop_assert!(!t.subtag.is_empty());
            prop_assert_eq!(t.namespace.clone(), t.namespace.to_lowercase());
            prop_assert_eq!(t.subtag.clone(), t.subtag.to_lowercase());
            prop_assert!(!t.namespace.contains("  "));
            prop_assert!(!t.subtag.contains("  "));
        }
    }

    #[test]
    fn hash_hex_round_trips(bytes in prop::array::uniform32(any::<u8>())) {
        let h = Hash::from_bytes(bytes);
        let hex = h.to_hex();
        prop_assert_eq!(hex.len(), 64);
        prop_assert_eq!(hex.parse::<Hash>().unwrap(), h);
        prop_assert_eq!(hex.to_uppercase().parse::<Hash>().unwrap(), h);
    }

    #[test]
    fn hash_rejects_wrong_lengths(s in "[0-9a-f]{0,63}") {
        prop_assert!(s.parse::<Hash>().is_err());
    }

    #[test]
    fn bucket_key_laws(bytes in prop::array::uniform32(any::<u8>()), bits in 0u32..=256) {
        let h = Hash::from_bytes(bytes);
        let key = bucket_key(&h, bits);
        let upper = bucket_upper(&h, bits);
        let hex = h.to_hex();

        // The hash lives inside its own bucket's range [key, upper).
        // ("g" sorts after every hex string, so the sentinel works here too.)
        prop_assert!(key <= hex, "key {} > hex {}", key, hex);
        prop_assert!(hex < upper, "hex {} >= upper {}", hex, upper);

        // Masking is idempotent: the key re-masks to itself.
        let masked: Hash = key.parse().unwrap();
        prop_assert_eq!(bucket_key(&masked, bits), key);

        // Full precision is the identity; zero bits is the all-zero bucket.
        prop_assert_eq!(bucket_key(&h, 256), hex);
        prop_assert_eq!(bucket_key(&h, 0), "0".repeat(64));
    }

    #[test]
    fn shared_prefix_means_shared_bucket(
        a in prop::array::uniform32(any::<u8>()),
        b in prop::array::uniform32(any::<u8>()),
        bits in 0u32..=256,
    ) {
        let ha = Hash::from_bytes(a);
        let hb = Hash::from_bytes(b);
        let same_prefix = mask_bytes(a, bits) == mask_bytes(b, bits);
        prop_assert_eq!(bucket_key(&ha, bits) == bucket_key(&hb, bits), same_prefix);
    }

    #[test]
    fn path_encoding_round_trips(parts in prop::collection::vec("[a-zA-Z0-9 ._\\-]{1,12}", 1..5)) {
        let p: PathBuf = parts.iter().collect();
        prop_assert_eq!(path_from_bytes(&path_to_bytes(&p)), p);
    }
}
