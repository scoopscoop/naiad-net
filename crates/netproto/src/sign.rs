//! Ed25519 accounts and the canonical signing rule shared by signer (daemon)
//! and verifier (repo). The only place a private key is handled.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use naiad_core::{Hash, Tag};

use crate::{Op, PROTOCOL_VERSION, RelKind, RelationSubmission, Submission, ensure_supported};

// ── Wire constants ────────────────────────────────────────────────────────────

/// Maximum byte length of the `origin` field on a wire submission (#166).
///
/// Bounds the per-row budget charge and the DB storage length. 128 is generous
/// for realistic tool names like `wd14-tagger` or `gelbooru`, while staying
/// well within a single JSON string token. Counted in UTF-8 bytes, not chars,
/// consistent with the canonical-bytes length-prefix framing.
///
/// Note: this is a server-acceptance constraint, not a frame change —
/// `PROTOCOL_VERSION` stays 8.
pub const MAX_ORIGIN_LEN: usize = 128;

// ── Account ───────────────────────────────────────────────────────────────────

/// An Ed25519 account. The identity *is* the public key; there are no usernames.
pub struct Account {
    /// `pub(crate)` so that `auth.rs` can sign canonical bytes without
    /// exposing the raw key outside the crate.
    pub(crate) signing: SigningKey,
}

impl Account {
    /// Generate a fresh keypair from the OS RNG.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// Rebuild an account from its 32-byte secret seed.
    #[must_use]
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(bytes),
        }
    }

    /// The 32-byte secret seed, for persistence.
    #[must_use]
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// The public key as 64-char lowercase hex — the author identity.
    #[must_use]
    pub fn public_hex(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    /// Sign one operation over a file's content hash and a normalized tag, with
    /// no asserted generation origin (manual). Thin shim over
    /// [`Account::sign_with_origin`] with `origin = None` — keeps the ~20 existing
    /// call sites unchanged.
    #[must_use]
    pub fn sign(&self, op: Op, hash: &Hash, tag: &Tag) -> Submission {
        self.sign_with_origin(op, hash, tag, None)
    }

    /// Sign one operation, asserting `origin` (the generation source) into the
    /// signed canonical bytes. `None` = manual/unattested. Asserted, not proven
    /// (ADR 0026).
    #[must_use]
    pub fn sign_with_origin(
        &self,
        op: Op,
        hash: &Hash,
        tag: &Tag,
        origin: Option<&str>,
    ) -> Submission {
        let hash = hash.to_hex();
        let tag = tag.to_string();
        let sig = self.signing.sign(&canonical_bytes(op, &hash, &tag, origin));
        Submission {
            version: PROTOCOL_VERSION,
            op,
            hash,
            tag,
            author: self.public_hex(),
            signature: hex::encode(sig.to_bytes()),
            origin: origin.map(str::to_string),
        }
    }

    /// Sign one relation operation over a `kind` and two normalized tags.
    #[must_use]
    pub fn sign_relation(&self, op: Op, kind: RelKind, from: &Tag, to: &Tag) -> RelationSubmission {
        let from = from.to_string();
        let to = to.to_string();
        let sig = self
            .signing
            .sign(&relation_canonical_bytes(op, kind, &from, &to));
        RelationSubmission {
            version: PROTOCOL_VERSION,
            op,
            kind,
            from,
            to,
            author: self.public_hex(),
            signature: hex::encode(sig.to_bytes()),
            origin: None,
        }
    }

    /// Load the account whose secret seed is stored at `path`, or `None` if the
    /// file does not exist.
    ///
    /// # Errors
    /// Returns an error if the file exists but cannot be read or is not 32 bytes.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!(e).context(format!("reading key {}", path.display()))),
        };
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("key file {} is not 32 bytes", path.display()))?;
        Ok(Some(Self::from_secret_bytes(&seed)))
    }

    /// Write the account's secret seed to `path` (best-effort `0600` on Unix).
    ///
    /// # Errors
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.secret_bytes())
            .with_context(|| format!("writing key {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Load the account at `path`, generating and saving a fresh one if absent.
    ///
    /// # Errors
    /// Returns an error if an existing file cannot be read or a new one written.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Some(acct) = Self::load(path)? {
            return Ok(acct);
        }
        let acct = Self::generate();
        acct.save(path)?;
        Ok(acct)
    }
}

// ── Canonical bytes ───────────────────────────────────────────────────────────

/// The exact bytes a submission's signature covers: domain-separated by protocol
/// version, newline-delimited. `origin` is length-prefix framed (ADR 0016) so
/// `None` (`-`) is unambiguously distinct from an empty-string origin (`0:`) and
/// from any tool name: two submissions differing only in origin sign to
/// different bytes. Safe because `hash` is fixed hex and `tag` is
/// whitespace-normalized.
#[must_use]
pub fn canonical_bytes(op: Op, hash: &str, tag: &str, origin: Option<&str>) -> Vec<u8> {
    let origin_framed = match origin {
        None => "-".to_string(),
        Some(o) => format!("{}:{o}", o.len()),
    };
    format!(
        "naiad-sub:v{PROTOCOL_VERSION}\n{}\n{hash}\n{tag}\n{origin_framed}",
        op.as_str(),
    )
    .into_bytes()
}

/// The exact bytes a relation submission's signature covers. Domain-separated
/// from mapping submissions (`naiad-rel` vs `naiad-sub`) so a relation signature
/// can never be replayed as a mapping or vice-versa.
#[must_use]
pub fn relation_canonical_bytes(op: Op, kind: RelKind, from: &str, to: &str) -> Vec<u8> {
    format!(
        "naiad-rel:v{PROTOCOL_VERSION}\n{}\n{}\n{from}\n{to}",
        op.as_str(),
        kind.as_str()
    )
    .into_bytes()
}

// ── Verification ──────────────────────────────────────────────────────────────

/// Verify a submission: supported version, well-formed hash, normalized tag, and
/// a valid Ed25519 signature by the stated author.
///
/// # Errors
/// Returns an error on an unsupported version, a malformed hash/tag, a
/// non-normalized tag, bad hex, or a signature mismatch.
pub fn verify(sub: &Submission) -> Result<()> {
    tracing::trace!(target: "sync", version = sub.version, "verifying submission");
    ensure_supported(sub.version)?;
    let parsed_hash: Hash = sub.hash.parse().context("submission hash")?;
    if parsed_hash.to_hex() != sub.hash {
        bail!("submission hash is not canonical lowercase hex");
    }
    let tag = Tag::parse(&sub.tag).context("submission tag")?;
    if tag.to_string() != sub.tag {
        bail!("submission tag is not normalized");
    }
    // Validate the asserted origin field (ADR 0026, #166).
    // Control chars inflate JSON-escaped size past the raw-byte budget charge;
    // an empty or whitespace-only origin must be sent as None (ADR 0026: NULL =
    // manual). PROTOCOL_VERSION stays 8 — this is acceptance tightening only.
    if let Some(o) = sub.origin.as_deref() {
        if o.len() > MAX_ORIGIN_LEN {
            bail!(
                "submission origin exceeds {MAX_ORIGIN_LEN}-byte limit ({} bytes)",
                o.len()
            );
        }
        if o.trim().is_empty() {
            bail!("submission origin is blank; send None for manual/unattested");
        }
        if o.chars().any(|c| c < '\x20' || c == '\x7F') {
            bail!("submission origin contains a control character");
        }
    }
    let author = decode_verifying_key(&sub.author)?;
    let signature = decode_signature(&sub.signature)?;
    author
        .verify_strict(
            &canonical_bytes(sub.op, &sub.hash, &sub.tag, sub.origin.as_deref()),
            &signature,
        )
        .map_err(|_| anyhow!("signature does not match author"))
}

/// Verify a relation submission: supported version, both tags normalized,
/// `from != to`, and a valid Ed25519 signature by the stated author.
///
/// # Errors
/// Returns an error on an unsupported version, a malformed/non-normalized tag, a
/// self-edge, bad hex, or a signature mismatch.
pub fn verify_relation(sub: &RelationSubmission) -> Result<()> {
    tracing::trace!(target: "sync", version = sub.version, "verifying relation submission");
    ensure_supported(sub.version)?;
    let from = Tag::parse(&sub.from).context("relation from-tag")?;
    if from.to_string() != sub.from {
        bail!("relation from-tag is not normalized");
    }
    let to = Tag::parse(&sub.to).context("relation to-tag")?;
    if to.to_string() != sub.to {
        bail!("relation to-tag is not normalized");
    }
    if sub.from == sub.to {
        bail!("relation is a self-edge (from == to)");
    }
    let author = decode_verifying_key(&sub.author)?;
    let signature = decode_signature(&sub.signature)?;
    author
        .verify_strict(
            &relation_canonical_bytes(sub.op, sub.kind, &sub.from, &sub.to),
            &signature,
        )
        .map_err(|_| anyhow!("signature does not match author"))
}

/// Check a 64-hex string decodes to a valid Ed25519 public key (for bundle
/// validation outside this module).
///
/// # Errors
/// Returns an error on bad hex, wrong length, or an invalid curve point.
pub fn validate_key_hex(key: &str) -> Result<()> {
    decode_verifying_key(key).map(|_| ())
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn decode_verifying_key(hex_str: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(hex_str)
        .map_err(|e| anyhow!("author is not hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("author is not 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow!("invalid author key: {e}"))
}

fn decode_signature(hex_str: &str) -> Result<ed25519_dalek::Signature> {
    let bytes: [u8; 64] = hex::decode(hex_str)
        .map_err(|e| anyhow!("signature is not hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature is not 64 bytes"))?;
    Ok(ed25519_dalek::Signature::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_core::hash_bytes;

    fn sample() -> (Account, Hash, Tag) {
        (
            Account::generate(),
            hash_bytes(b"file"),
            Tag::parse("character:samus").unwrap(),
        )
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let (acct, h, t) = sample();
        let sub = acct.sign(Op::Add, &h, &t);
        assert_eq!(sub.version, PROTOCOL_VERSION);
        assert_eq!(sub.author, acct.public_hex());
        verify(&sub).expect("a freshly signed submission verifies");
    }

    #[test]
    fn verify_rejects_tampering() {
        let (acct, h, t) = sample();
        let good = acct.sign(Op::Add, &h, &t);

        let mut bad_tag = good.clone();
        bad_tag.tag = "character:zelda".into();
        assert!(verify(&bad_tag).is_err(), "tag tamper must fail");

        let mut bad_op = good.clone();
        bad_op.op = Op::Remove;
        assert!(verify(&bad_op).is_err(), "op tamper must fail");

        let mut bad_hash = good.clone();
        bad_hash.hash = hash_bytes(b"other").to_hex();
        assert!(verify(&bad_hash).is_err(), "hash tamper must fail");

        let mut bad_author = good.clone();
        bad_author.author = Account::generate().public_hex();
        assert!(verify(&bad_author).is_err(), "author swap must fail");

        let mut bad_ver = good.clone();
        bad_ver.version = PROTOCOL_VERSION + 1;
        assert!(verify(&bad_ver).is_err(), "version mismatch must fail");
    }

    #[test]
    fn verify_rejects_an_unnormalized_tag() {
        let (acct, h, _) = sample();
        let mut sub = acct.sign(Op::Add, &h, &Tag::parse("character:samus").unwrap());
        sub.tag = "Character:Samus".into();
        assert!(verify(&sub).is_err());
    }

    #[test]
    fn verify_rejects_a_noncanonical_hash() {
        let (acct, h, t) = sample();
        let mut sub = acct.sign(Op::Add, &h, &t);
        sub.hash = sub.hash.to_uppercase();
        assert!(verify(&sub).is_err(), "non-canonical hash must fail");
    }

    #[test]
    fn secret_bytes_round_trip() {
        let acct = Account::generate();
        let restored = Account::from_secret_bytes(&acct.secret_bytes());
        assert_eq!(acct.public_hex(), restored.public_hex());
    }

    #[test]
    fn load_or_create_creates_then_loads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naiad.key");
        assert!(Account::load(&path).unwrap().is_none(), "absent at first");
        let created = Account::load_or_create(&path).unwrap();
        let loaded = Account::load(&path).unwrap().expect("present after create");
        assert_eq!(created.public_hex(), loaded.public_hex());
    }

    #[test]
    fn canonical_bytes_are_stable_and_domain_separated() {
        let h = hash_bytes(b"file").to_hex();
        // None origin → literal `-` sentinel.
        let bytes = canonical_bytes(Op::Add, &h, "character:samus", None);
        let expected = format!("naiad-sub:v{PROTOCOL_VERSION}\nadd\n{h}\ncharacter:samus\n-");
        assert_eq!(bytes, expected.into_bytes());

        let remove = canonical_bytes(Op::Remove, &h, "character:samus", None);
        let expected_rm = format!("naiad-sub:v{PROTOCOL_VERSION}\nremove\n{h}\ncharacter:samus\n-");
        assert_eq!(remove, expected_rm.into_bytes());

        // Some("wd14-tagger") → length-prefix framed: "wd14-tagger" is 11 bytes.
        let with_origin = canonical_bytes(Op::Add, &h, "character:samus", Some("wd14-tagger"));
        let expected_origin =
            format!("naiad-sub:v{PROTOCOL_VERSION}\nadd\n{h}\ncharacter:samus\n11:wd14-tagger");
        assert_eq!(with_origin, expected_origin.into_bytes());

        // Multi-byte UTF-8: "オリジン" is 12 UTF-8 bytes (4 × 3-byte codepoints).
        // The length prefix must count BYTES, not characters.
        let with_multibyte = canonical_bytes(Op::Add, &h, "character:samus", Some("オリジン"));
        let expected_multibyte =
            format!("naiad-sub:v{PROTOCOL_VERSION}\nadd\n{h}\ncharacter:samus\n12:オリジン");
        assert_eq!(with_multibyte, expected_multibyte.into_bytes());
    }

    // ── Origin validation ─────────────────────────────────────────────────────

    #[test]
    fn verify_accepts_valid_origin() {
        let (acct, h, t) = sample();
        let sub = acct.sign_with_origin(Op::Add, &h, &t, Some("wd14-tagger"));
        verify(&sub).expect("a valid origin must verify");
    }

    #[test]
    fn verify_rejects_origin_too_long() {
        let (acct, h, t) = sample();
        let long = "a".repeat(MAX_ORIGIN_LEN + 1); // 129 bytes
        let mut sub = acct.sign_with_origin(Op::Add, &h, &t, Some("placeholder"));
        sub.origin = Some(long);
        assert!(verify(&sub).is_err(), "129-byte origin must be rejected");
    }

    #[test]
    fn verify_rejects_empty_origin() {
        let (acct, h, t) = sample();
        let mut sub = acct.sign_with_origin(Op::Add, &h, &t, Some("placeholder"));
        sub.origin = Some(String::new());
        assert!(verify(&sub).is_err(), "empty origin must be rejected");
    }

    #[test]
    fn verify_rejects_whitespace_only_origin() {
        let (acct, h, t) = sample();
        let mut sub = acct.sign_with_origin(Op::Add, &h, &t, Some("placeholder"));
        sub.origin = Some("   ".into());
        assert!(
            verify(&sub).is_err(),
            "whitespace-only origin must be rejected"
        );
    }

    #[test]
    fn verify_rejects_origin_with_newline() {
        let (acct, h, t) = sample();
        let mut sub = acct.sign_with_origin(Op::Add, &h, &t, Some("placeholder"));
        sub.origin = Some("bad\norigin".into());
        assert!(verify(&sub).is_err(), "origin with \\n must be rejected");
    }

    #[test]
    fn verify_rejects_origin_with_control_char_0x01() {
        let (acct, h, t) = sample();
        let mut sub = acct.sign_with_origin(Op::Add, &h, &t, Some("placeholder"));
        sub.origin = Some("bad\x01origin".into());
        assert!(verify(&sub).is_err(), "origin with U+0001 must be rejected");
    }

    #[test]
    fn verify_rejects_origin_with_del_0x7f() {
        let (acct, h, t) = sample();
        let mut sub = acct.sign_with_origin(Op::Add, &h, &t, Some("placeholder"));
        sub.origin = Some("bad\x7Forigin".into());
        assert!(
            verify(&sub).is_err(),
            "origin with DEL (0x7F) must be rejected"
        );
    }

    #[test]
    fn verify_accepts_128_byte_multibyte_utf8_origin_at_cap() {
        // "オ" is 3 UTF-8 bytes; 42 × 3 = 126, + "ab" = 128 exactly.
        let at_cap = "オ".repeat(42) + "ab";
        assert_eq!(
            at_cap.len(),
            MAX_ORIGIN_LEN,
            "test origin must be exactly {MAX_ORIGIN_LEN} bytes"
        );
        let (acct, h, t) = sample();
        let sub = acct.sign_with_origin(Op::Add, &h, &t, Some(&at_cap));
        verify(&sub).expect("an origin at the exact byte cap must verify");
    }

    #[test]
    fn sign_relation_then_verify_round_trips() {
        let acct = Account::generate();
        let from = Tag::parse("character:samus_aran").unwrap();
        let to = Tag::parse("character:samus").unwrap();
        let sub = acct.sign_relation(Op::Add, RelKind::Sibling, &from, &to);
        assert_eq!(sub.version, PROTOCOL_VERSION);
        assert_eq!(sub.author, acct.public_hex());
        verify_relation(&sub).expect("a freshly signed relation verifies");
    }

    #[test]
    fn verify_relation_rejects_tampering() {
        let acct = Account::generate();
        let from = Tag::parse("character:samus_aran").unwrap();
        let to = Tag::parse("character:samus").unwrap();
        let good = acct.sign_relation(Op::Add, RelKind::Sibling, &from, &to);

        let mut bad_op = good.clone();
        bad_op.op = Op::Remove;
        assert!(verify_relation(&bad_op).is_err(), "op tamper must fail");

        let mut bad_kind = good.clone();
        bad_kind.kind = RelKind::Parent;
        assert!(verify_relation(&bad_kind).is_err(), "kind tamper must fail");

        let mut bad_from = good.clone();
        bad_from.from = "character:zelda".into();
        assert!(verify_relation(&bad_from).is_err(), "from tamper must fail");

        let mut bad_to = good.clone();
        bad_to.to = "character:zelda".into();
        assert!(verify_relation(&bad_to).is_err(), "to tamper must fail");

        let mut bad_author = good.clone();
        bad_author.author = Account::generate().public_hex();
        assert!(
            verify_relation(&bad_author).is_err(),
            "author swap must fail"
        );

        let mut bad_ver = good.clone();
        bad_ver.version = PROTOCOL_VERSION + 1;
        assert!(
            verify_relation(&bad_ver).is_err(),
            "version mismatch must fail"
        );
    }

    #[test]
    fn relation_and_mapping_signatures_are_domain_separated() {
        let acct = Account::generate();
        let from = Tag::parse("character:samus_aran").unwrap();
        let to = Tag::parse("character:samus").unwrap();
        let rel = acct.sign_relation(Op::Add, RelKind::Sibling, &from, &to);

        let mut as_mapping = acct.sign(Op::Add, &hash_bytes(b"file"), &to);
        as_mapping.signature = rel.signature.clone();
        assert!(
            verify(&as_mapping).is_err(),
            "rel sig must not verify as mapping"
        );

        let mapping = acct.sign(Op::Add, &hash_bytes(b"file"), &to);
        let mut spliced = rel.clone();
        spliced.signature = mapping.signature;
        assert!(
            verify_relation(&spliced).is_err(),
            "mapping sig must not verify as rel"
        );
    }

    #[test]
    fn verify_relation_rejects_self_edge_and_unnormalized() {
        let acct = Account::generate();
        let from = Tag::parse("character:samus_aran").unwrap();
        let to = Tag::parse("character:samus").unwrap();

        let same = acct.sign_relation(Op::Add, RelKind::Sibling, &from, &from);
        assert!(verify_relation(&same).is_err(), "self-edge must fail");

        let mut sub = acct.sign_relation(Op::Add, RelKind::Sibling, &from, &to);
        sub.from = "Character:Samus_Aran".into();
        assert!(
            verify_relation(&sub).is_err(),
            "non-normalized from must fail"
        );
    }

    #[test]
    fn relation_canonical_bytes_are_stable_and_framed() {
        let bytes = relation_canonical_bytes(
            Op::Add,
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
        );
        let expected = format!(
            "naiad-rel:v{PROTOCOL_VERSION}\nadd\nsibling\ncharacter:samus_aran\ncharacter:samus"
        );
        assert_eq!(bytes, expected.into_bytes());
    }
}
