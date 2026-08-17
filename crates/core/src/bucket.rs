//! k-anonymity prefix-bucket math (ADR 0001): the hash masking + range bounds
//! that the client (which buckets to ask for), the repo (which ranges to scan),
//! and the local db (which files a bucket covers) all agree on. Defined once
//! here so the three can never drift.

use crate::Hash;

/// The 32 bytes of `hash` with its low `256 − bits` bits zeroed. `bits` is
/// clamped to `[0, 256]`. Shared by [`bucket_key`] and [`bucket_upper`] so the
/// masking is defined exactly once.
fn masked(hash: &Hash, bits: u32) -> [u8; 32] {
    let bits = bits.min(256) as usize;
    let mut bytes = *hash.as_bytes();
    let full = bits / 8; // bytes kept in full
    let rem = bits % 8; // leftover high bits in the next byte
    if full < 32 {
        if rem > 0 {
            bytes[full] &= 0xFFu8 << (8 - rem); // keep the top `rem` bits
            for b in &mut bytes[full + 1..] {
                *b = 0;
            }
        } else {
            for b in &mut bytes[full..] {
                *b = 0;
            }
        }
    }
    bytes
}

/// The bucket a hash falls into at `prefix_bits`: the hash with its low
/// `256 − prefix_bits` bits zeroed, as 64-char lowercase hex. The wire identity
/// of a bucket. `prefix_bits` is clamped to `[0, 256]`.
#[must_use]
pub fn bucket_key(hash: &Hash, prefix_bits: u32) -> String {
    Hash::from_bytes(masked(hash, prefix_bits)).to_hex()
}

/// The exclusive upper bound of `lo`'s bucket at `prefix_bits`: a value strictly
/// greater than every hash in the bucket, for a `hash < hi` range scan. `lo` is
/// treated as a lo-bound (its low bits are masked off first). For the final
/// bucket (the increment overflows past 2²⁵⁶) this is the sentinel `"g"`, which
/// sorts after every lowercase-hex hash; `prefix_bits == 0` (one all-covering
/// bucket) is the same case.
#[must_use]
pub fn bucket_upper(lo: &Hash, prefix_bits: u32) -> String {
    let bits = prefix_bits.min(256);
    if bits == 0 {
        return "g".to_string();
    }
    let mut bytes = masked(lo, bits);
    // Add 1 at bit position (256 − bits), counted from the LSB.
    let add_bit = 256 - bits as usize; // 0..=255 here (bits >= 1)
    let idx = 31 - add_bit / 8; // big-endian array index
    let mut carry = 1u16 << (add_bit % 8);
    let mut i = idx as isize;
    while carry > 0 && i >= 0 {
        let v = u16::from(bytes[i as usize]) + carry;
        bytes[i as usize] = (v & 0xFF) as u8;
        carry = v >> 8;
        i -= 1;
    }
    if carry > 0 {
        return "g".to_string(); // overflowed the top → unbounded
    }
    Hash::from_bytes(bytes).to_hex()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_bytes;

    #[test]
    fn bucket_key_zero_bits_is_all_zero() {
        let h = hash_bytes(b"anything");
        assert_eq!(bucket_key(&h, 0), "0".repeat(64));
    }

    #[test]
    fn bucket_key_full_bits_is_identity() {
        let h = hash_bytes(b"anything");
        assert_eq!(bucket_key(&h, 256), h.to_hex());
    }

    #[test]
    fn bucket_key_masks_at_a_mid_byte_boundary() {
        // 12 bits keeps the first byte and the top nibble of the second byte.
        let h = Hash::from_bytes([0xAB; 32]);
        let key = bucket_key(&h, 12);
        assert!(key.starts_with("aba"), "kept 12 bits: {key}");
        assert_eq!(&key[3..], &"0".repeat(61));
    }

    #[test]
    fn same_prefix_shares_a_key_differing_past_it_does_not() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        a[0] = 0x12;
        a[1] = 0x30; // differs from b only below bit 12
        b[0] = 0x12;
        b[1] = 0x3F;
        let (a, b) = (Hash::from_bytes(a), Hash::from_bytes(b));
        assert_eq!(bucket_key(&a, 12), bucket_key(&b, 12), "same 12-bit prefix");
        assert_ne!(bucket_key(&a, 16), bucket_key(&b, 16), "differ at 16 bits");
    }

    #[test]
    fn bucket_upper_is_the_next_prefix() {
        // 8-bit buckets: width 2^248, so upper = lo with the top byte +1.
        let lo = Hash::from_bytes([0x00; 32]);
        let mut want = [0u8; 32];
        want[0] = 0x01;
        assert_eq!(bucket_upper(&lo, 8), Hash::from_bytes(want).to_hex());
    }

    #[test]
    fn bucket_upper_of_the_final_bucket_is_the_hex_ceiling() {
        // All-ones prefix: incrementing overflows, so the upper bound is the
        // sentinel "g", which sorts after every lowercase-hex hash.
        let lo = Hash::from_bytes([0xFF; 32]);
        assert_eq!(bucket_upper(&lo, 8), "g");
        assert_eq!(bucket_upper(&lo, 0), "g");
    }
}
