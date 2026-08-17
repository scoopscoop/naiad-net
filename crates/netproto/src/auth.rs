//! Unified request-auth scheme for all authenticated naiad endpoints (spec §4).
//!
//! One domain (`naiad-auth:v{VERSION}`) covers every authenticated route:
//! submit, report, moderator queue, moderator act. A fixed-size blake3 body
//! hash binds the payload without bloating the canonical frame.
//!
//! Document-level signatures that persist in the submission log (`naiad-sub`,
//! `naiad-rel`) are separate concerns handled in `sign.rs`.

use anyhow::{Result, bail};
use ed25519_dalek::{Signer, VerifyingKey};

use crate::{Account, HashDomain, PROTOCOL_VERSION};

/// Freshness window: requests whose `|now − timestamp| > 300 s` are rejected.
pub const AUTH_FRESHNESS_SECS: i64 = 300;

/// HTTP header carrying the signer's Ed25519 public key (lowercase hex).
pub const HDR_AUTH_KEY: &str = "x-naiad-key";

/// HTTP header carrying the Unix timestamp (decimal seconds) used at signing.
pub const HDR_AUTH_TS: &str = "x-naiad-ts";

/// HTTP header carrying the Ed25519 signature (lowercase hex).
pub const HDR_AUTH_SIG: &str = "x-naiad-sig";

/// Domain-separated bytes that a request-auth signature covers.
///
/// `method` must be a valid HTTP token (ASCII alphabetic, no whitespace or
/// control characters — e.g. `"GET"`, `"POST"`).
/// `path` must be a URI request-target with no raw `'\n'` characters
/// (e.g. `"/repo/submit"`). It is the path **only** — the query string is not
/// part of it; see the invariant below.
/// `domain` is the hash domain the caller asked for: `Some(d)` when the request
/// carries `?domain=<d>`, `None` when it carries no `domain=` at all (blank and
/// whitespace-only values canonicalize to `None`, matching
/// [`resolve_domain`](crate::resolve_domain)). It is rendered as its canonical
/// wire spelling, or an empty field when absent — so a rewritten, stripped, or
/// added `?domain=` fails verification.
/// `timestamp` is Unix seconds (the value placed in `x-naiad-ts`).
/// `body` is the raw request body; an empty slice for body-less requests.
///
/// The blake3 body hash keeps the frame fixed-size regardless of payload
/// length.
///
/// # INVARIANT: nothing outside this frame may steer an authenticated handler
///
/// The server verifies against `uri.path()`, which **drops the query string**.
/// Any request input that changes what an authenticated endpoint does must
/// therefore appear as a field here, or it is attacker-controlled. `?domain=`
/// was exactly that hole (#161, protocol v7): it selected a hash domain from
/// outside the signed bytes. A future query parameter on an authenticated route
/// must either be folded into this frame — which is a wire break, so bump
/// [`PROTOCOL_VERSION`] and `MIN_SUPPORTED_VERSION` together — or carried in the
/// signed body. `DomainParam` is `deny_unknown_fields` so an unsigned parameter
/// cannot be introduced silently.
///
/// **Injection safety:** the canonical frame uses `'\n'` as a delimiter.
/// Inputs that violate the constraints above (e.g. a `method` containing a
/// raw newline) can produce colliding byte sequences — `("A\nB", "C")` yields
/// the same frame as `("A", "B\nC")`. HTTP request parsers reject such inputs
/// before they reach this function, so the constraint is not practically
/// exploitable, but callers must not pass unsanitized strings. `domain` is a
/// closed enum whose spellings contain no delimiter, so it cannot collide.
#[must_use]
pub fn auth_canonical_bytes(
    method: &str,
    path: &str,
    domain: Option<HashDomain>,
    timestamp: i64,
    body: &[u8],
) -> Vec<u8> {
    let body_hash = blake3::hash(body).to_hex();
    let domain = domain.map_or("", |d| d.as_str());
    format!("naiad-auth:v{PROTOCOL_VERSION}\n{method}\n{path}\n{domain}\n{timestamp}\n{body_hash}")
        .into_bytes()
}

impl Account {
    /// Sign a request. Returns the 128-char lowercase hex Ed25519 signature
    /// (64 bytes). Place it in `x-naiad-sig`, the public key in `x-naiad-key`,
    /// and `timestamp` in `x-naiad-ts`. Pair with [`verify_auth`].
    ///
    /// `domain` must be the hash domain the caller actually puts on the wire:
    /// `Some(d)` if and only if the request URL carries `?domain=<d>`. Signing a
    /// domain the request does not send (or omitting one it does) verifies as a
    /// tamper and is rejected 401.
    #[must_use]
    pub fn sign_auth(
        &self,
        method: &str,
        path: &str,
        domain: Option<HashDomain>,
        timestamp: i64,
        body: &[u8],
    ) -> String {
        tracing::trace!(target: "sync", method, path, domain = ?domain, timestamp, "signing request auth");
        let bytes = auth_canonical_bytes(method, path, domain, timestamp, body);
        hex::encode(self.signing.sign(&bytes).to_bytes())
    }
}

/// Verify a request-auth header triple `(key_hex, sig_hex)` against the
/// reconstructed canonical bytes.
///
/// `domain` must be the *requested* domain parsed off this request's query
/// string — `None` when the request carried no `domain=` — not the domain the
/// server resolved it to. Binding the wire value is what makes rewriting,
/// stripping, or adding `?domain=` a 401 instead of a silent redirect to another
/// hash domain (#161); resolution stays a pure function of that bound value and
/// the server's own configuration.
///
/// # Errors
/// Returns an error if:
/// - `key_hex` or `sig_hex` is not valid hex or has the wrong byte length,
/// - `key_hex` is not a valid Ed25519 curve point,
/// - the Ed25519 signature does not verify over the canonical bytes, or
/// - `|now − timestamp| > AUTH_FRESHNESS_SECS` — uses saturating arithmetic
///   so `i64::MIN` / `i64::MAX` timestamps never panic or wrap.
// Eight arguments, one over clippy's threshold: adding `domain` (#161) pushed it
// past. They are not a grab-bag — five reconstruct the canonical frame verbatim
// and must stay positionally aligned with `auth_canonical_bytes`, which is what
// makes an omission a compile error rather than a silently weaker signature.
// Bundling them into a request struct would break that alignment for one lint.
#[allow(clippy::too_many_arguments)]
pub fn verify_auth(
    key_hex: &str,
    sig_hex: &str,
    method: &str,
    path: &str,
    domain: Option<HashDomain>,
    timestamp: i64,
    now: i64,
    body: &[u8],
) -> Result<()> {
    tracing::trace!(target: "sync", method, path, domain = ?domain, timestamp, "verifying request auth");
    // Saturating sub + saturating abs: i64::MIN.saturating_sub(i64::MAX) = i64::MIN,
    // i64::MIN.saturating_abs() = i64::MAX → stale, no panic, no wrap.
    if now.saturating_sub(timestamp).saturating_abs() > AUTH_FRESHNESS_SECS {
        bail!("stale auth timestamp");
    }

    let key_bytes: [u8; 32] = hex::decode(key_hex)
        .map_err(|e| anyhow::anyhow!("x-naiad-key is not hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("x-naiad-key is not 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| anyhow::anyhow!("invalid x-naiad-key: {e}"))?;

    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .map_err(|e| anyhow::anyhow!("x-naiad-sig is not hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("x-naiad-sig is not 64 bytes"))?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);

    key.verify_strict(
        &auth_canonical_bytes(method, path, domain, timestamp, body),
        &sig,
    )
    .map_err(|_| anyhow::anyhow!("auth signature mismatch"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Account;

    const NOW: i64 = 1_700_000_000;
    const METHOD: &str = "POST";
    const PATH: &str = "/repo/submit";
    const BODY: &[u8] = b"request body";
    /// A request that carries no `?domain=` at all — what every in-tree client
    /// sends today.
    const NO_DOMAIN: Option<HashDomain> = None;

    fn acct() -> Account {
        Account::generate()
    }

    #[test]
    fn sign_verify_round_trip() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        verify_auth(
            &a.public_hex(),
            &sig,
            METHOD,
            PATH,
            NO_DOMAIN,
            NOW,
            NOW,
            BODY,
        )
        .expect("freshly signed request must verify");
    }

    #[test]
    fn wrong_key_rejected() {
        let a = acct();
        let b = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        assert!(
            verify_auth(
                &b.public_hex(),
                &sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                NOW,
                NOW,
                BODY
            )
            .is_err(),
            "wrong key must fail"
        );
    }

    #[test]
    fn tampered_body_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                NOW,
                NOW,
                b"tampered"
            )
            .is_err(),
            "tampered body must fail"
        );
    }

    #[test]
    fn tampered_path_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                "/repo/other",
                NO_DOMAIN,
                NOW,
                NOW,
                BODY
            )
            .is_err(),
            "tampered path must fail"
        );
    }

    /// #161: a request signed with no `?domain=` must not verify once an
    /// on-path attacker appends one. This is the case that mattered — appending
    /// `?domain=sha256` to a validly-signed submit would otherwise steer it at a
    /// hash domain the signer never agreed to.
    #[test]
    fn appended_domain_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                Some(HashDomain::Sha256),
                NOW,
                NOW,
                BODY
            )
            .is_err(),
            "appending ?domain= to an unqualified request must fail"
        );
    }

    /// The mirror case: stripping a `?domain=` the signer did send.
    #[test]
    fn stripped_domain_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, Some(HashDomain::Sha256), NOW, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                NOW,
                NOW,
                BODY
            )
            .is_err(),
            "stripping ?domain= must fail"
        );
    }

    /// Swapping one served domain for another served domain — the phase-3
    /// redirect this frame exists to prevent.
    #[test]
    fn swapped_domain_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, Some(HashDomain::Sha256), NOW, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                Some(HashDomain::Blake3),
                NOW,
                NOW,
                BODY
            )
            .is_err(),
            "swapping ?domain= must fail"
        );
    }

    /// An explicit `?domain=` round-trips like any other field.
    #[test]
    fn explicit_domain_round_trips() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, Some(HashDomain::Sha256), NOW, BODY);
        verify_auth(
            &a.public_hex(),
            &sig,
            METHOD,
            PATH,
            Some(HashDomain::Sha256),
            NOW,
            NOW,
            BODY,
        )
        .expect("explicit domain must round-trip");
    }

    /// The absent-domain field is empty, not the native spelling: `None` and
    /// `Some(Blake3)` are distinct frames even though a blake3-native repo
    /// resolves both to the same domain. Guards against a future refactor
    /// "helpfully" defaulting `None` to the native domain, which would make the
    /// two interchangeable on the wire again.
    #[test]
    fn absent_domain_is_not_the_native_spelling() {
        let absent = auth_canonical_bytes(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        let explicit = auth_canonical_bytes(METHOD, PATH, Some(HashDomain::Blake3), NOW, BODY);
        assert_ne!(absent, explicit, "absent must not alias the native domain");
        let s = String::from_utf8(absent).unwrap();
        assert!(
            s.contains(&format!("{PATH}\n\n{NOW}")),
            "absent domain must be an empty field between path and timestamp, got {s:?}"
        );
    }

    #[test]
    fn tampered_method_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                "GET",
                PATH,
                NO_DOMAIN,
                NOW,
                NOW,
                BODY
            )
            .is_err(),
            "tampered method must fail"
        );
    }

    #[test]
    fn stale_past_timestamp_rejected() {
        let a = acct();
        let old_ts = NOW - AUTH_FRESHNESS_SECS - 1;
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, old_ts, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                old_ts,
                NOW,
                BODY
            )
            .is_err(),
            "old timestamp must be rejected"
        );
    }

    #[test]
    fn stale_future_timestamp_rejected() {
        let a = acct();
        let future_ts = NOW + AUTH_FRESHNESS_SECS + 1;
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, future_ts, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                future_ts,
                NOW,
                BODY
            )
            .is_err(),
            "future timestamp must be rejected"
        );
    }

    #[test]
    fn freshness_boundary_accepted() {
        let a = acct();
        // Exactly at the boundary (== 300) is accepted.
        let ts = NOW - AUTH_FRESHNESS_SECS;
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, ts, BODY);
        verify_auth(
            &a.public_hex(),
            &sig,
            METHOD,
            PATH,
            NO_DOMAIN,
            ts,
            NOW,
            BODY,
        )
        .expect("exactly at boundary must be accepted");
    }

    /// A crafted i64::MIN timestamp must be rejected (stale), not panicked/wrapped.
    #[test]
    fn i64_min_timestamp_does_not_panic() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, i64::MIN, b"");
        // Must not panic; result is irrelevant (will be stale).
        let result = verify_auth(
            &a.public_hex(),
            &sig,
            METHOD,
            PATH,
            NO_DOMAIN,
            i64::MIN,
            NOW,
            b"",
        );
        assert!(result.is_err(), "i64::MIN timestamp must be stale");
    }

    /// A crafted i64::MAX timestamp must be rejected (stale), not panicked/wrapped.
    #[test]
    fn i64_max_timestamp_does_not_panic() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, i64::MAX, b"");
        let result = verify_auth(
            &a.public_hex(),
            &sig,
            METHOD,
            PATH,
            NO_DOMAIN,
            i64::MAX,
            NOW,
            b"",
        );
        assert!(result.is_err(), "i64::MAX timestamp must be stale");
    }

    /// Signing with ts=NOW and verifying with ts=NOW-5 (both fresh) must fail.
    /// Proves the timestamp in the header cannot be silently swapped within the
    /// freshness window — the ts is part of the signed bytes, not just a
    /// freshness gate.
    #[test]
    fn swapped_timestamp_within_freshness_window_rejected() {
        let a = acct();
        let sign_ts = NOW;
        let verify_ts = NOW - 5; // still fresh (|diff| = 5 < 300)
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, sign_ts, BODY);
        assert!(
            verify_auth(
                &a.public_hex(),
                &sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                verify_ts,
                NOW,
                BODY
            )
            .is_err(),
            "timestamp swap within freshness window must fail"
        );
    }

    #[test]
    fn malformed_key_hex_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, b"");
        assert!(
            verify_auth("not-hex!!", &sig, METHOD, PATH, NO_DOMAIN, NOW, NOW, b"").is_err(),
            "bad key hex must fail"
        );
    }

    /// 66 hex chars = 33 bytes: valid hex but wrong length for an Ed25519 key.
    #[test]
    fn wrong_length_key_hex_rejected() {
        let a = acct();
        let sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, b"");
        let long_key = "ab".repeat(33); // 66 chars, 33 bytes — not 32
        assert!(
            verify_auth(&long_key, &sig, METHOD, PATH, NO_DOMAIN, NOW, NOW, b"").is_err(),
            "33-byte key hex must fail"
        );
    }

    #[test]
    fn malformed_sig_hex_rejected() {
        let a = acct();
        assert!(
            verify_auth(
                &a.public_hex(),
                "not-hex!!",
                METHOD,
                PATH,
                NO_DOMAIN,
                NOW,
                NOW,
                b""
            )
            .is_err(),
            "bad sig hex must fail"
        );
    }

    /// 66 hex chars = 33 bytes: valid hex but wrong length for an Ed25519 sig.
    #[test]
    fn wrong_length_sig_hex_rejected() {
        let a = acct();
        let long_sig = "cd".repeat(33); // 66 chars, 33 bytes — not 64
        assert!(
            verify_auth(
                &a.public_hex(),
                &long_sig,
                METHOD,
                PATH,
                NO_DOMAIN,
                NOW,
                NOW,
                b""
            )
            .is_err(),
            "33-byte sig hex must fail"
        );
    }

    #[test]
    fn empty_body_accepted() {
        let a = acct();
        let sig = a.sign_auth("GET", "/repo/caps", NO_DOMAIN, NOW, b"");
        verify_auth(
            &a.public_hex(),
            &sig,
            "GET",
            "/repo/caps",
            NO_DOMAIN,
            NOW,
            NOW,
            b"",
        )
        .expect("empty body must round-trip");
    }

    #[test]
    fn canonical_bytes_domain_separated_from_sub_and_rel() {
        let bytes = auth_canonical_bytes("POST", "/repo/submit", NO_DOMAIN, 42, b"");
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.starts_with(&format!("naiad-auth:v{}", crate::PROTOCOL_VERSION)),
            "must carry auth domain"
        );
        assert!(!s.contains("naiad-sub"), "must not collide with sub domain");
        assert!(!s.contains("naiad-rel"), "must not collide with rel domain");
    }

    #[test]
    fn auth_sig_does_not_verify_as_sub() {
        // A request-auth signature over (method, path, ts, body) must not
        // accidentally verify a submission's canonical bytes.
        let a = acct();
        let sub_sig = a.sign_auth(METHOD, PATH, NO_DOMAIN, NOW, BODY);
        // Construct a Submission whose signature field is the auth sig.
        let hash = naiad_core::hash_bytes(b"file");
        let tag = naiad_core::Tag::parse("character:samus").unwrap();
        let mut sub = a.sign(crate::Op::Add, &hash, &tag);
        sub.signature = sub_sig;
        assert!(
            crate::verify(&sub).is_err(),
            "auth sig must not verify as submission"
        );
    }
}
