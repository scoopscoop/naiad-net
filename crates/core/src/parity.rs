//! Streaming digest over sorted (hash, tag) mapping pairs, shared by the mirror
//! parity audit (issue #184). Callers MUST feed pairs in ascending (hash, tag)
//! order; the digest is order-sensitive by construction. Length-prefixing the
//! tag removes delimiter-injection ambiguity so no tag byte can be mistaken for
//! a pair boundary.

/// Accumulates a blake3 digest and a row count over `(hash, tag)` pairs.
#[derive(Debug)]
pub struct PairDigest {
    hasher: blake3::Hasher,
    count: u64,
}

impl PairDigest {
    /// Fresh, empty digest.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            count: 0,
        }
    }

    /// Absorb one pair. `hash` is the raw 32-byte content hash; `tag` is the
    /// full tag string. Framing: `hash ‖ u32_le(tag.len()) ‖ tag`.
    pub fn update(&mut self, hash: &[u8; 32], tag: &str) {
        self.hasher.update(hash);
        self.hasher.update(&(tag.len() as u32).to_le_bytes());
        self.hasher.update(tag.as_bytes());
        self.count += 1;
    }

    /// Number of pairs absorbed so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Consume the digest, returning `(count, 32-byte digest)`.
    #[must_use]
    pub fn finalize(self) -> (u64, [u8; 32]) {
        (self.count, *self.hasher.finalize().as_bytes())
    }
}

impl Default for PairDigest {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(pairs: &[([u8; 32], &str)]) -> (u64, [u8; 32]) {
        let mut d = PairDigest::new();
        for (h, t) in pairs {
            d.update(h, t);
        }
        d.finalize()
    }

    #[test]
    fn parity_same_pairs_same_order_match() {
        let a = digest(&[
            ([1u8; 32], "character:samus"),
            ([2u8; 32], "series:metroid"),
        ]);
        let b = digest(&[
            ([1u8; 32], "character:samus"),
            ([2u8; 32], "series:metroid"),
        ]);
        assert_eq!(a, b);
        assert_eq!(a.0, 2);
    }

    #[test]
    fn parity_order_sensitive() {
        let a = digest(&[([1u8; 32], "a"), ([2u8; 32], "b")]);
        let b = digest(&[([2u8; 32], "b"), ([1u8; 32], "a")]);
        assert_ne!(a.1, b.1);
    }

    #[test]
    fn parity_length_prefix_no_delimiter_collision() {
        // ("a", "b:c") vs ("a:b", "c") would collide under naive ':' joining;
        // length-prefixing keeps them distinct.
        let a = digest(&[([1u8; 32], "a"), ([1u8; 32], "b:c")]);
        let b = digest(&[([1u8; 32], "a:b"), ([1u8; 32], "c")]);
        assert_ne!(a.1, b.1);
    }

    #[test]
    fn parity_empty_and_newline_tags_are_distinct() {
        let a = digest(&[([1u8; 32], "")]);
        let b = digest(&[([1u8; 32], "\n")]);
        assert_ne!(a.1, b.1);
    }
}
