//! k-anonymity prefix buckets (ADR 0001): the capabilities/handshake DTOs, plus
//! a re-export of the shared masking + range math both the client (which
//! buckets to ask for) and the repo (which ranges to scan) agree on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The pull mode a repo advertises at `GET /repo/caps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum PullMode {
    /// Request the buckets covering your owned hashes at this prefix length.
    Bucketed { prefix_bits: u32 },
    /// The repo has fewer than `k` hashes: no prefix yields a real crowd, so
    /// download the whole repo (`GET /repo/snapshot`) instead.
    WholeRepo,
}

/// Which hash function keys a repo's mapping hashes. Default (and the only value
/// existing v6 repos ever produce) is BLAKE3 — naiad's primary file identity.
/// A bridge-enabled `naiad-repo` node mirroring the Hydrus PTR advertises `Sha256`, because
/// the PTR keys mappings by SHA-256. Clients that have computed SHA-256 for
/// their files (eager dual-hash at import, ADR 0018) can then pull sha256-keyed
/// mappings and land the tags on their BLAKE3 identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashDomain {
    /// BLAKE3 — naiad's primary identity. The default for every native repo.
    #[default]
    Blake3,
    /// SHA-256 — an interop key. Advertised by bridge nodes mirroring Hydrus.
    Sha256,
}

impl HashDomain {
    /// The canonical wire spelling of this domain (`"blake3"` or `"sha256"`).
    ///
    /// Use this instead of hand-typing the string literals so that renaming a
    /// variant is a single-site change. `Display` delegates here.
    pub const fn as_str(&self) -> &'static str {
        match self {
            HashDomain::Blake3 => "blake3",
            HashDomain::Sha256 => "sha256",
        }
    }
}

/// Wire spellings: `"blake3"` and `"sha256"`. Must match the `serde(rename_all
/// = "lowercase")` on the enum above. Used by ADR 0025 cross-tier override
/// warnings where `HashDomain` values are printed in warning messages via the
/// generic `pick<T: Display>` helper.
impl std::fmt::Display for HashDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a hash-domain string cannot be parsed.
///
/// The message names the bad input and lists the accepted values, making
/// `clap` and env-var error messages self-explanatory without extra wrappers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseHashDomainError(String);

impl std::fmt::Display for ParseHashDomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown hash domain {:?}; accepted values: blake3, sha256",
            self.0
        )
    }
}

impl std::error::Error for ParseHashDomainError {}

/// Parse a wire/CLI hash-domain spelling. Case-insensitive and whitespace
/// tolerant so `repo.toml`, `NAIAD_REPO_*` env vars, `--hash-domain` and a
/// `domain=` query parameter all accept the same values.
impl std::str::FromStr for HashDomain {
    type Err = ParseHashDomainError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "blake3" => Ok(HashDomain::Blake3),
            "sha256" => Ok(HashDomain::Sha256),
            _ => Err(ParseHashDomainError(s.trim().to_string())),
        }
    }
}

/// Lenient deserializer for `Caps::hash_domains`: parses each element as a
/// raw string and **silently drops** any value that [`HashDomain::from_str`]
/// does not recognise. This preserves forward-compatibility: a future repo
/// advertising an unknown domain (e.g. `"blake2s"`) does not kill the entire
/// caps handshake for the domains we do support — the unknown entry is simply
/// ignored and the caller falls back to the domains it knows.
fn lenient_hash_domains<'de, D>(de: D) -> std::result::Result<Vec<HashDomain>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<String> = Vec::deserialize(de)?;
    Ok(raw.iter().filter_map(|s| s.parse().ok()).collect())
}

/// One domain's advisory serve-temperature estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ServeHint {
    /// EWMA of recent per-bucket serve latency, in milliseconds. A warm repo
    /// reports a fraction of a millisecond; a cold one single-to-tens of ms per
    /// bucket. Non-negative, finite; the producer must omit the entry rather
    /// than serialise a `NaN`/`Inf` sentinel (enforced server-side by
    /// `ServeStats`, #173 Task 2).
    pub ms_per_bucket: f64,
    /// The prefix width, in bits, that `ms_per_bucket` is denominated in (#178) —
    /// the reference width the server normalised its serve-cost EWMA to (its
    /// advertised bucketed `prefix_bits`). A client re-scales the cost onto the
    /// width it is actually about to query with
    /// `ms_per_bucket × 2^(hint_bits − requested_bits)`.
    ///
    /// Absent means a pre-#178 server or one that does not normalise (a
    /// mirror/advise repo); the client then assumes the hint was measured at the
    /// repo's advertised width. Omitted on the wire when `None` so a
    /// non-normalising and a pre-#178 server emit byte-identical caps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_bits: Option<u32>,
}

/// Bound on the `hint_bits`↔width shift used by the server's EWMA normalisation
/// and the client's re-scaling (#178). The two sides must agree for the
/// round trip to be exact, so the const lives here and both crates import it.
/// 40 ⇒ scale factors in `[2^-40, 2^40]`, comfortably inside f64's range;
/// realistic widths (8..=32 both sides) never reach it.
pub const HINT_SHIFT_CLAMP: i32 = 40;

/// A repository's advertised pull capabilities. The `mode` is flattened into the
/// object, so the wire form is `{"version":6,"mode":"wholerepo"}` or
/// `{"version":6,"mode":"bucketed","prefix_bits":N}` — not a nested `mode.mode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    pub version: u32,
    #[serde(flatten)]
    pub mode: PullMode,
    /// Whether this repo serves incremental relation deltas (`?since=`). Absent on
    /// pre-incremental repos → `#[serde(default)]` makes it false there.
    #[serde(default)]
    pub relation_incremental: bool,
    /// Whether this repo serves incremental mapping deltas from `POST /repo/buckets`
    /// when the request carries a per-bucket `since` vector.
    #[serde(default)]
    pub mapping_incremental: bool,
    /// Whether this repo accepts anonymous reports (`POST /repo/report`) and
    /// serves the moderator queue (`GET /repo/reports`).
    #[serde(default)]
    pub reports: bool,
    /// This repo's identity public key (hex) — the rotation-chain tip. Absent
    /// on repos without a configured identity. Mirrors pin this on approval
    /// (TOFU) and verify it every sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_key: Option<String>,
    /// Which hash function keys this repo's mapping hashes. Absent on the wire →
    /// `Blake3` (every pre-bridge v6 repo). Additive, back-compatible: v6 stays v6.
    #[serde(default)]
    pub hash_domain: HashDomain,
    /// Every hash domain this repo serves (spec §1). Additive and
    /// back-compatible: absent on the wire ⇒ read through [`Caps::domains`],
    /// which falls back to `[hash_domain]`. `hash_domain` keeps its meaning —
    /// "the domain an old client should use" — so a dual-domain repo reports
    /// `hash_domain: "blake3"` there and a pre-change client sees a plain
    /// BLAKE3 repo. Omitted when empty so old clients see byte-identical caps.
    ///
    /// Deserialised leniently: unknown domain strings are **silently dropped**
    /// so a future repo advertising `"blake2s"` doesn't abort the handshake.
    #[serde(
        default,
        deserialize_with = "lenient_hash_domains",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hash_domains: Vec<HashDomain>,
    /// Domains for which this repo serves incremental bucket deltas (a `since`
    /// vector on `POST /repo/buckets`). Absent on the wire ⇒ `None` ⇒ fall back
    /// to the pre-#142 rule: `mapping_incremental` gates the NATIVE domain only.
    ///
    /// `Option<Vec<String>>`, not `Vec<String>`, so three states stay distinct:
    /// absent (`None`, an old server — fall back), present-empty (`Some([])`, a
    /// new server serving no deltas anywhere), and present-listed. `Vec<String>`
    /// of wire spellings (not `Vec<HashDomain>`) so that unknown future domain
    /// strings survive a serialise/deserialise cycle intact. Both this field and
    /// `hash_domains` are tolerant of unknown spellings, but in different ways:
    /// `hash_domains` silently **drops** unknown values via `lenient_hash_domains`,
    /// while `incremental_domains` **retains** them as raw strings so the client
    /// can still pass `since` for a domain whose enum variant hasn't landed yet.
    /// `skip_serializing_if` means a repo that does not set it emits
    /// byte-identical caps to today — no wire-version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incremental_domains: Option<Vec<String>>,
    /// The `naiad-repo` build version string (e.g. `"0.2.48"`). Absent on older
    /// servers → `None`. Clients should treat this as informational only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// A per-domain estimate of this repo's recent serve cost, in milliseconds
    /// per bucket, keyed by wire domain spelling (`"blake3"`/`"sha256"`).
    /// Advisory only: a client uses it to size its FIRST request window and
    /// never treats it as a contract. Absent when the repo has served no bucket
    /// requests since boot. Omitted on the wire when empty, so a pre-#173
    /// server and a just-booted server emit byte-identical caps — no
    /// wire-version bump.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub serve_hint: BTreeMap<String, ServeHint>,
    /// Whether this repo can stream `POST /repo/buckets` responses incrementally
    /// (#176). A client opts in per-request only when this is true. Absent on the
    /// wire ⇒ false ⇒ a pre-#176 server; `skip_serializing_if` keeps a
    /// non-streaming server byte-identical to today's caps — no wire-version bump.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub streaming: bool,
    /// The server's effective minimum bucket-query width for its snapshot
    /// (non-native) domain, in prefix bits (#179). A bucket request coarser than
    /// this is rejected with 400 (see #175). Advertised so a client whose privacy
    /// ceiling sits below this floor can clamp its query width UP to the floor —
    /// which is privacy-safe by construction (#175's k-anonymity analysis) — and
    /// pull, instead of discovering the floor via a failed request.
    ///
    /// `Some` only when this repo runs a snapshot backend (the only case the
    /// server enforces a floor); `None` on every other repo, including mirror-mode
    /// PTRs whose native domain is sha256 (they enforce no floor). Omitted on the
    /// wire when `None`, so a pre-#179 server and a non-snapshot server emit
    /// byte-identical caps — no wire-version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_query_bits: Option<u32>,
    /// This repo's total distinct-hash count, used by a client purely to
    /// translate a desired k-anonymity crowd into a bucket width and to
    /// estimate download volume. Advisory only — never a contract. Absent on
    /// pre-count servers → `None`; the client then falls back to a bits-only
    /// control with no size estimate. Omitted on the wire when `None`, so a
    /// pre-count server emits byte-identical caps — no wire-version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// Opaque store-generation id. Changes whenever the mirror is rebuilt from
    /// scratch (`bridge seed --rebuild`), so a client can detect a reshuffled seq
    /// space and re-pull from zero instead of resuming a stale cursor. Absent on
    /// repos that predate this feature → clients fall back to the backwards-cursor
    /// guard. Additive, back-compatible: old clients ignore the field; new clients
    /// against an old repo keep today's behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_generation: Option<String>,
    /// Human-readable display name the repo operator configured (e.g. "NOS").
    /// Absent on older servers and on servers with no name configured → `None`.
    /// Informational: the client captures it once at subscribe time; later
    /// renames do not ripple. Omitted on the wire when `None`, so an unnamed
    /// server emits byte-identical caps — no wire-version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Caps {
    /// The domains this repo serves, normalised: an absent or empty
    /// `hash_domains` means the one-element list `[hash_domain]`.
    #[must_use]
    pub fn domains(&self) -> Vec<HashDomain> {
        if self.hash_domains.is_empty() {
            vec![self.hash_domain]
        } else {
            self.hash_domains.clone()
        }
    }

    /// Whether this repo serves `domain`. Allocation-free: when `hash_domains`
    /// is empty (a pre-dual-domain repo) the check compares against
    /// `hash_domain`; otherwise it scans the explicit list.
    #[must_use]
    pub fn serves(&self, domain: HashDomain) -> bool {
        if self.hash_domains.is_empty() {
            self.hash_domain == domain
        } else {
            self.hash_domains.contains(&domain)
        }
    }

    /// The `domain=` value to put on the wire when querying `domain`.
    ///
    /// Returns `Some(domain)` only when **both** conditions hold:
    /// 1. The repo explicitly advertised a domain list (`hash_domains` is
    ///    non-empty), meaning it speaks the dual-domain protocol.
    /// 2. The repo actually serves `domain` (i.e. `self.serves(domain)`).
    ///
    /// Returns `None` in any other case — including when `hash_domains` is
    /// empty (a pre-dual-domain repo that does not understand the `domain=`
    /// query parameter even for its own native domain), so callers produce
    /// byte-identical requests to old repos without any special-casing.
    #[must_use]
    pub fn wire_domain(&self, domain: HashDomain) -> Option<HashDomain> {
        (!self.hash_domains.is_empty() && self.serves(domain)).then_some(domain)
    }

    /// Heuristic: does this repo look snapshot-backed for SHA-256?
    ///
    /// The server filters `sha256` out of `incremental_domains` exactly when a
    /// snapshot backend exists, so "serves sha256 but not incrementally" is
    /// today an exact proxy for snapshot mode. `hash_domains` (not
    /// [`Caps::serves`]) is checked deliberately: a pre-dual-domain repo (empty
    /// `hash_domains`) never infers.
    ///
    /// Known false positive: a 0.2.52–0.2.58 dual-domain server predating
    /// `incremental_domains` also matches. The consequence is only a WARN log
    /// and a hint appended to an already-failed pull, so this is accepted.
    #[must_use]
    pub fn snapshot_inferred(&self) -> bool {
        self.hash_domains.contains(&HashDomain::Sha256)
            && !self
                .incremental_domains
                .as_ref()
                .is_some_and(|d| d.iter().any(|s| s == HashDomain::Sha256.as_str()))
    }

    /// Whether the client should send a `since` vector for `domain` — i.e. this
    /// repo serves incremental deltas for it.
    ///
    /// - `incremental_domains: Some(list)` ⇒ `list` contains `domain`; the
    ///   authoritative per-domain answer.
    /// - `incremental_domains: None` (an old server) ⇒ the pre-#142 fallback:
    ///   the global `mapping_incremental` flag gates the NATIVE domain
    ///   (`hash_domain`) only. A non-native domain never gets `since` under the
    ///   fallback, which is exactly what keeps a client off a snapshot SHA-256
    ///   leg that would 400 it.
    #[must_use]
    pub fn serves_deltas(&self, domain: HashDomain) -> bool {
        match &self.incremental_domains {
            Some(list) => list.iter().any(|d| d == domain.as_str()),
            None => self.mapping_incremental && domain == self.hash_domain,
        }
    }
}

/// A batched bucket pull. Self-describing: it restates the `prefix_bits` the
/// client masked against, so a repo that crossed a sizing boundary between the
/// handshake and this call still scans the right ranges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRequest {
    pub version: u32,
    pub prefix_bits: u32,
    /// Deduped lo-bound hash hex, one per bucket (see [`bucket_key`]).
    pub buckets: Vec<String>,
    /// Optional per-bucket cursors, index-aligned with `buckets`. Absent means
    /// the legacy full-bucket pull shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Vec<u64>>,
    /// Which hash domain to query (spec §1). Absent ⇒ the repo's `hash_domain`,
    /// which is how pre-dual-domain clients keep working. Carried as a raw
    /// string rather than a typed [`HashDomain`] so an unrecognised value
    /// produces the actionable [`DomainError::Unrecognized`] message instead of
    /// a bare serde body rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Ask the server to stream this response incrementally as NDJSON (#176).
    /// Absent ⇒ the legacy materialized-JSON reply. Additive: an old server
    /// ignores the unknown field (BucketRequest has no deny_unknown_fields) and
    /// replies with materialized JSON, which is why a new client must only set
    /// this against a server whose caps advertised `streaming: true`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub stream: bool,
    /// Continuation cursor for a streamed pull (#176): resume scanning at the
    /// masked bucket key the previous streamed response returned in its `more`
    /// trailer. Absent ⇒ start from the beginning of the (masked, sorted) key set.
    /// Ignored unless `stream` is set. Additive; an old server ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<String>,
}

/// The `domain=` query parameter carried by domain-discriminated GET endpoints
/// (`GET /repo/snapshot`) and by `POST /repo/submit`. Raw `String` for the same
/// error-quality reason as [`BucketRequest::domain`].
///
/// `deny_unknown_fields` makes any other query parameter a 400 rather than a
/// silent no-op. On the authenticated routes that is load-bearing: the signed
/// canonical frame covers the path and this one domain field, so an unsigned
/// parameter that silently reached a handler would be attacker-controlled
/// (#161). Failing the request is what forces a future parameter to be added to
/// [`auth_canonical_bytes`](crate::auth_canonical_bytes) deliberately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainParam {
    /// The raw `domain=` value from the query string, or `None` when absent.
    /// Validated by [`resolve_domain`] rather than by serde so unrecognised
    /// values produce an actionable [`DomainError`] instead of a body parse
    /// failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

/// The requested hash domain as the request-auth canonical frame sees it:
/// `Some(d)` when the query string carries a parseable `domain=`, `None` when it
/// is absent, blank, or whitespace-only.
///
/// This is the *stated intent*, not the resolved domain — the client cannot know
/// which domain a repo treats as native without a prior `/repo/caps` round-trip,
/// so binding the resolution would couple every signed request to a handshake.
/// Binding the wire value is enough: resolution is a pure function of it and the
/// server's own configuration, so pinning the input pins the outcome (#161).
///
/// An unrecognised spelling collapses to `None` here, which is harmless — every
/// caller runs [`resolve_domain`] first and has already returned 400 for it.
#[must_use]
pub fn requested_domain(raw: Option<&str>) -> Option<HashDomain> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Why a requested `domain=` could not be honoured (spec §6). Both variants
/// carry the domains the repo *does* serve, so every rejection is actionable
/// and is never an empty 200.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// The value is not a hash domain this build knows about at all.
    Unrecognized {
        /// The raw string that was not recognised.
        requested: String,
        /// Every domain this repo serves (for the error message).
        served: Vec<HashDomain>,
    },
    /// A known hash domain, but not one this repo serves.
    NotServed {
        /// The parsed domain that was not served.
        requested: HashDomain,
        /// Every domain this repo serves (for the error message).
        served: Vec<HashDomain>,
    },
}

/// Render a served-domain list for an error message: `"blake3, sha256"`.
fn domain_list(served: &[HashDomain]) -> String {
    served
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DomainError::Unrecognized { requested, served } => write!(
                f,
                "unknown hash domain {requested:?}; this repo serves: {}",
                domain_list(served)
            ),
            DomainError::NotServed { requested, served } => write!(
                f,
                "this repo does not serve the {requested} hash domain; it serves: {}",
                domain_list(served)
            ),
        }
    }
}

impl std::error::Error for DomainError {}

/// Resolve a requested `domain=` value against the domains a repo serves.
///
/// `None` or a blank value yields `fallback` (the repo's `hash_domain`), which
/// is how pre-dual-domain clients keep working. Anything else must both parse
/// and be served, or this returns the corresponding [`DomainError`] — the
/// server never answers an unserved domain with an empty success (spec §6).
///
/// # Errors
/// Returns [`DomainError::Unrecognized`] for an unparseable value and
/// [`DomainError::NotServed`] for a known domain this repo does not serve.
pub fn resolve_domain(
    requested: Option<&str>,
    served: &[HashDomain],
    fallback: HashDomain,
) -> std::result::Result<HashDomain, DomainError> {
    // Invariant: the fallback must itself be a served domain, otherwise a
    // missing/blank `domain=` would silently return a domain the repo can't
    // answer — a mis-configuration, not a client error.
    debug_assert!(
        served.contains(&fallback),
        "resolve_domain: fallback {fallback} is not in served list {served:?}"
    );
    let Some(raw) = requested else {
        return Ok(fallback);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(fallback);
    }
    let domain: HashDomain = trimmed.parse().map_err(|_| DomainError::Unrecognized {
        requested: trimmed.to_string(),
        served: served.to_vec(),
    })?;
    if served.contains(&domain) {
        Ok(domain)
    } else {
        Err(DomainError::NotServed {
            requested: domain,
            served: served.to_vec(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MappingStatus {
    Current,
    Deleted,
}

/// One mapping entry in an incremental delta: a `(hash, tag, status, seq)` tuple
/// plus the tag's asserted generation `origin` (ADR 0026), omitted when manual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaMapping {
    pub hash: String,
    pub tag: String,
    pub status: MappingStatus,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// One tag with its asserted generation source (ADR 0026) in a full/snapshot
/// bucket response. Serialized compactly; `origin` omitted (manual) is the
/// common case. Origin is asserted, not proven — display/filter metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginTag {
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingDelta {
    pub version: u32,
    pub cursor: u64,
    pub changes: Vec<DeltaMapping>,
}

// ── NDJSON streaming frame types (#176) ──────────────────────────────────────

/// First line of a streamed `POST /repo/buckets` response (NDJSON, #176).
/// Carries the same `version` and `cursor` as the materialized [`Snapshot`] does,
/// emitted before any row lines so the client can validate the protocol version
/// and merge the cursor immediately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHeader {
    pub version: u32,
    pub cursor: u64,
}

/// One hash→tags row in a streamed `POST /repo/buckets` response (#176).
/// Short keys `"h"`/`"t"` keep per-row envelope overhead down at PTR scale.
/// Byte-for-byte the value type already in [`Snapshot::tags`]; the client
/// merges each row into the accumulator as it arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRow {
    pub h: String,
    pub t: Vec<OriginTag>,
}

/// Last line of a streamed `POST /repo/buckets` response (#176). Exactly one
/// of three shapes; the discriminating key determines which:
///
/// - `{"done":true}` — scan completed the entire key set.
/// - `{"more":"<masked-key>"}` — budget cutoff; `<masked-key>` is the first
///   un-served bucket. Client resumes with a follow-up streaming request
///   carrying `resume_at: "<masked-key>"`.
/// - `{"err":"<message>"}` — in-band error (already sent 200, so HTTP status
///   cannot signal it). Client surfaces as a hard fetch failure.
///
/// The `#[serde(untagged)]` repr keeps the exact wire shapes without a `type`
/// discriminator field; the client matches on which key is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamTrailer {
    /// `{"done":true}` — window complete.
    Done { done: bool },
    /// `{"more":"<masked-key>"}` — budget cutoff; resume from this key.
    More { more: String },
    /// `{"err":"<message>"}` — in-band error after bytes have flowed.
    Err { err: String },
}

/// The prefix width a client will actually mask to, given what a repo
/// `advertised` and the client's privacy `ceiling` (`[privacy].max_query_bits`).
///
/// This is simply `min(advertised, ceiling)`. Querying *coarser* than advertised
/// is always correct — [`BucketRequest`] restates the width it used and every
/// repo scans the ranges named in the request, so a coarser query returns a
/// superset the client filters locally. Coarser costs bandwidth, never privacy.
///
/// The `min` also neutralizes nonsense advertisements: `advertised > 256`
/// clamps to the ceiling like any other over-ceiling value, and
/// `advertised == 0` (one whole-repo-wide bucket) passes through untouched.
#[must_use]
pub fn effective_prefix_bits(advertised: u32, ceiling: u32) -> u32 {
    advertised.min(ceiling)
}

/// The prefix width a client will mask to for a **floored** domain, given what
/// a repo `advertised`, the client's privacy `ceiling`, and the repo's
/// advertised `floor` (`caps.min_query_bits`).
///
/// Without a floor this is just [`effective_prefix_bits`] — `min(advertised,
/// ceiling)`. With a floor it additionally raises the result **up** to the
/// floor, because querying coarser than the floor is refused by the server
/// (#175) and the floor is privacy-safe by construction. The final `.min(advertised)`
/// defends against a nonsensical advertisement (`floor > advertised`): the
/// client never queries finer than the repo said it can serve. Because the
/// server guarantees `floor ≤ advertised`, that clamp is normally inert.
///
/// Monotonic and one-directional: the result is always in
/// `[min(min(advertised, ceiling), advertised), advertised]` and only ever
/// **raises** the effective width relative to `effective_prefix_bits`. The
/// result never exceeds `advertised`, and reveals no finer precision than
/// `floor` — the minimum width the server operator judged k-anonymous (#175).
#[must_use]
pub fn effective_prefix_bits_floored(advertised: u32, ceiling: u32, floor: Option<u32>) -> u32 {
    let base = advertised.min(ceiling);
    match floor {
        Some(f) => base.max(f).min(advertised),
        None => base,
    }
}

// The bucket masking + range math lives in `naiad_core::bucket` (shared with
// the client db's coverage queries); re-exported here so wire-facing crates
// keep importing it from the protocol crate.
pub use naiad_core::{bucket_key, bucket_upper};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;

    // The bucket_key / bucket_upper masking tests live with the math in
    // `naiad_core::bucket`.

    #[test]
    fn caps_and_request_round_trip() {
        for mode in [PullMode::WholeRepo, PullMode::Bucketed { prefix_bits: 13 }] {
            let caps = Caps {
                version: PROTOCOL_VERSION,
                mode,
                relation_incremental: false,
                mapping_incremental: false,
                reports: false,
                repo_key: None,
                hash_domain: HashDomain::Blake3,
                hash_domains: Vec::new(),
                incremental_domains: None,
                server_version: None,
                serve_hint: Default::default(),
                streaming: false,
                min_query_bits: None,
                store_generation: None,
                count: None,
                name: None,
            };
            let back: Caps = serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
            assert_eq!(caps, back);
        }
        let req = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits: 13,
            buckets: vec!["00".repeat(32), "ab".repeat(32)],
            since: None,
            domain: None,
            stream: false,
            resume_at: None,
        };
        let back: BucketRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn caps_serializes_flat_not_nested() {
        // The mode is flattened into the caps object — no `mode.mode` nesting.
        assert_eq!(
            serde_json::to_value(Caps {
                version: PROTOCOL_VERSION,
                mode: PullMode::WholeRepo,
                relation_incremental: false,
                mapping_incremental: false,
                reports: false,
                repo_key: None,
                hash_domain: HashDomain::Blake3,
                hash_domains: Vec::new(),
                incremental_domains: None,
                server_version: None,
                serve_hint: Default::default(),
                streaming: false,
                min_query_bits: None,
                store_generation: None,
                count: None,
                name: None,
            })
            .unwrap(),
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "mode": "wholerepo",
                "relation_incremental": false,
                "mapping_incremental": false,
                "reports": false,
                "hash_domain": "blake3"
            })
        );
        assert_eq!(
            serde_json::to_value(Caps {
                version: PROTOCOL_VERSION,
                mode: PullMode::Bucketed { prefix_bits: 9 },
                relation_incremental: false,
                mapping_incremental: false,
                reports: false,
                repo_key: None,
                hash_domain: HashDomain::Blake3,
                hash_domains: Vec::new(),
                incremental_domains: None,
                server_version: None,
                serve_hint: Default::default(),
                streaming: false,
                min_query_bits: None,
                store_generation: None,
                count: None,
                name: None,
            })
            .unwrap(),
            serde_json::json!({
                "version": PROTOCOL_VERSION,
                "mode": "bucketed",
                "prefix_bits": 9,
                "relation_incremental": false,
                "mapping_incremental": false,
                "reports": false,
                "hash_domain": "blake3"
            })
        );
    }

    #[test]
    fn caps_relation_incremental_defaults_false_when_absent() {
        let old = r#"{"version":6,"mode":"wholerepo"}"#;
        let caps: Caps = serde_json::from_str(old).unwrap();
        assert!(!caps.relation_incremental);
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: true,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let back: Caps = serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(caps, back);
    }

    #[test]
    fn caps_mapping_incremental_defaults_false_when_absent() {
        let old = r#"{"version":6,"mode":"bucketed","prefix_bits":9}"#;
        let caps: Caps = serde_json::from_str(old).unwrap();
        assert!(!caps.mapping_incremental);
    }

    #[test]
    fn caps_reports_defaults_false_when_absent() {
        let c: Caps = serde_json::from_str(r#"{"version":6,"mode":"wholerepo"}"#).unwrap();
        assert!(!c.reports);
        // True round-trips correctly.
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: true,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let back: Caps = serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(caps, back);
    }

    #[test]
    fn caps_repo_key_defaults_none_and_round_trips() {
        // Old caps JSON (no repo_key) parses to None.
        let c: Caps = serde_json::from_str(r#"{"version":6,"mode":"wholerepo"}"#).unwrap();
        assert!(c.repo_key.is_none());
        // None is omitted on the wire.
        let mut caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        assert!(!serde_json::to_string(&caps).unwrap().contains("repo_key"));
        // Some round-trips.
        caps.repo_key = Some("ab".repeat(32));
        let back: Caps = serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(caps, back);
    }

    #[test]
    fn bucket_request_since_is_optional_and_round_trips() {
        let req = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits: 8,
            buckets: vec!["00".repeat(32), "80".to_string() + &"00".repeat(31)],
            since: Some(vec![0, 42]),
            domain: None,
            stream: false,
            resume_at: None,
        };
        let back: BucketRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(req, back);

        let old = format!(
            r#"{{"version":{},"prefix_bits":8,"buckets":["{}"]}}"#,
            PROTOCOL_VERSION,
            "00".repeat(32)
        );
        let parsed: BucketRequest = serde_json::from_str(&old).unwrap();
        assert_eq!(parsed.since, None);
    }

    #[test]
    fn bucket_request_omits_since_when_absent() {
        let req = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits: 8,
            buckets: vec!["00".repeat(32)],
            since: None,
            domain: None,
            stream: false,
            resume_at: None,
        };

        let value = serde_json::to_value(req).unwrap();
        assert!(value.get("since").is_none());
    }

    #[test]
    fn delta_mapping_plain_shape_round_trips() {
        // v6: DeltaMapping has hash, tag, status, seq; origin is optional and omitted when None.
        let delta = MappingDelta {
            version: PROTOCOL_VERSION,
            cursor: 10,
            changes: vec![
                DeltaMapping {
                    hash: "00".repeat(32),
                    tag: "series:metroid".into(),
                    status: MappingStatus::Current,
                    seq: 9,
                    origin: None,
                },
                DeltaMapping {
                    hash: "11".repeat(32),
                    tag: "series:zelda".into(),
                    status: MappingStatus::Deleted,
                    seq: 10,
                    origin: None,
                },
            ],
        };
        let json = serde_json::to_string(&delta).unwrap();
        // Must not contain any fields from the old (pre-pivot) v5 shape.
        assert!(!json.contains("supporters"));
        assert!(!json.contains("total"));
        let back: MappingDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(delta, back);
    }

    #[test]
    fn effective_prefix_bits_decision_table() {
        // (advertised, ceiling) → expected effective width.
        for (advertised, ceiling, expected) in [
            (13, 24, 13),    // under ceiling → untouched
            (24, 24, 24),    // at ceiling → untouched
            (32, 24, 24),    // over ceiling → clamped
            (256, 24, 24),   // exact-hash attack → clamped
            (300, 24, 24),   // > 256 nonsense → clamped
            (0, 24, 0),      // whole-repo-wide bucket → passes
            (24, 256, 24),   // raised ceiling honors advertised
            (200, 256, 200), // raised ceiling passes a fine prefix
        ] {
            assert_eq!(
                effective_prefix_bits(advertised, ceiling),
                expected,
                "advertised={advertised} ceiling={ceiling}"
            );
        }
    }

    #[test]
    fn caps_hash_domain_defaults_blake3_when_absent() {
        // Every existing v6 repo omits the field → must read as Blake3.
        let old = r#"{"version":6,"mode":"wholerepo"}"#;
        let caps: Caps = serde_json::from_str(old).unwrap();
        assert_eq!(caps.hash_domain, HashDomain::Blake3);
    }

    #[test]
    fn caps_hash_domain_sha256_round_trips() {
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Sha256,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            json.contains(r#""hash_domain":"sha256""#),
            "serializes lowercase: {json}"
        );
        let back: Caps = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }

    #[test]
    fn caps_server_version_defaults_none_and_round_trips() {
        // Absent in JSON → None (backward-compatible with older servers).
        let no_ver: Caps = serde_json::from_str(r#"{"version":6,"mode":"wholerepo"}"#).unwrap();
        assert_eq!(no_ver.server_version, None);

        // None → field absent in serialised form (skip_serializing_if).
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            !json.contains("server_version"),
            "server_version must be absent when None: {json}"
        );

        // Some(version) → round-trips cleanly.
        let with_ver = Caps {
            server_version: Some("0.2.48".to_string()),
            ..caps
        };
        let json2 = serde_json::to_string(&with_ver).unwrap();
        assert!(json2.contains("\"server_version\":\"0.2.48\""), "{json2}");
        let back: Caps = serde_json::from_str(&json2).unwrap();
        assert_eq!(back.server_version, Some("0.2.48".to_string()));
    }

    #[test]
    fn pullmode_serializes_tagged_lowercase() {
        assert_eq!(
            serde_json::to_value(PullMode::WholeRepo).unwrap(),
            serde_json::json!({ "mode": "wholerepo" })
        );
        assert_eq!(
            serde_json::to_value(PullMode::Bucketed { prefix_bits: 7 }).unwrap(),
            serde_json::json!({ "mode": "bucketed", "prefix_bits": 7 })
        );
    }

    #[test]
    fn caps_hash_domains_absent_defaults_to_single_hash_domain() {
        // Every pre-dual-domain repo omits the list entirely.
        let old = r#"{"version":6,"mode":"wholerepo"}"#;
        let caps: Caps = serde_json::from_str(old).unwrap();
        assert!(caps.hash_domains.is_empty(), "absent list parses as empty");
        assert_eq!(
            caps.domains(),
            vec![HashDomain::Blake3],
            "empty list normalises to [hash_domain]"
        );

        // A pre-dual-domain mirror repo: sha256 in the scalar field only.
        let old_mirror = r#"{"version":6,"mode":"wholerepo","hash_domain":"sha256"}"#;
        let caps: Caps = serde_json::from_str(old_mirror).unwrap();
        assert_eq!(caps.domains(), vec![HashDomain::Sha256]);
    }

    #[test]
    fn caps_hash_domains_round_trips_and_is_omitted_when_empty() {
        let mut caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        assert!(
            !serde_json::to_string(&caps)
                .unwrap()
                .contains("hash_domains"),
            "empty list must be omitted so old clients see today's bytes"
        );

        caps.hash_domains = vec![HashDomain::Blake3, HashDomain::Sha256];
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            json.contains(r#""hash_domains":["blake3","sha256"]"#),
            "dual-domain list serialises lowercase: {json}"
        );
        let back: Caps = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
        assert_eq!(
            back.hash_domain,
            HashDomain::Blake3,
            "the scalar field still names the domain an old client should use"
        );
    }

    #[test]
    fn caps_hash_domains_unknown_value_is_silently_dropped() {
        // A future repo advertising "blake2s" alongside known domains must not
        // kill the handshake for the domains we do support (forward compat).
        let json = r#"{"version":6,"mode":"wholerepo","hash_domain":"blake3","hash_domains":["blake3","sha256","blake2s"]}"#;
        let caps: Caps = serde_json::from_str(json).unwrap();
        assert_eq!(
            caps.domains(),
            vec![HashDomain::Blake3, HashDomain::Sha256],
            "unknown domain dropped, known ones kept: {:?}",
            caps.hash_domains
        );
    }

    #[test]
    fn caps_serves_and_wire_domain() {
        let dual = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        assert!(dual.serves(HashDomain::Blake3));
        assert!(dual.serves(HashDomain::Sha256));
        assert_eq!(
            dual.wire_domain(HashDomain::Sha256),
            Some(HashDomain::Sha256)
        );
        assert_eq!(
            dual.wire_domain(HashDomain::Blake3),
            Some(HashDomain::Blake3)
        );

        // A single-domain explicit list: SHA-256 is not served.
        let blake3_only = Caps {
            hash_domains: vec![HashDomain::Blake3],
            ..dual.clone()
        };
        assert!(blake3_only.serves(HashDomain::Blake3));
        assert!(!blake3_only.serves(HashDomain::Sha256));
        assert_eq!(
            blake3_only.wire_domain(HashDomain::Sha256),
            None,
            "wire_domain returns None for a domain the repo does not serve"
        );

        // A pre-dual-domain repo (hash_domains absent/empty): wire_domain must
        // return None for EVERY domain — old repos don't understand domain= at
        // all, so the daemon must send byte-identical requests to them.
        let old = Caps {
            hash_domains: Vec::new(),
            ..dual.clone()
        };
        assert!(old.serves(HashDomain::Blake3));
        assert!(!old.serves(HashDomain::Sha256));
        assert_eq!(
            old.wire_domain(HashDomain::Blake3),
            None,
            "never send domain= to a repo that never advertised hash_domains"
        );
        assert_eq!(
            old.wire_domain(HashDomain::Sha256),
            None,
            "unserved domain also returns None for pre-dual-domain repo"
        );
    }

    #[test]
    fn hash_domain_from_str_accepts_both_spellings() {
        assert_eq!("blake3".parse::<HashDomain>(), Ok(HashDomain::Blake3));
        assert_eq!("SHA256".parse::<HashDomain>(), Ok(HashDomain::Sha256));
        assert_eq!("  sha256 ".parse::<HashDomain>(), Ok(HashDomain::Sha256));
        assert!("md5".parse::<HashDomain>().is_err());
        assert!("".parse::<HashDomain>().is_err());
        // Error message includes the bad value and the accepted list.
        let msg = "md5".parse::<HashDomain>().unwrap_err().to_string();
        assert!(msg.contains("md5"), "bad value in message: {msg}");
        assert!(
            msg.contains("blake3") && msg.contains("sha256"),
            "accepted values listed: {msg}"
        );
    }

    #[test]
    fn resolve_domain_decision_table() {
        let dual = [HashDomain::Blake3, HashDomain::Sha256];
        let native_only = [HashDomain::Blake3];

        // Absent / blank → the fallback (how old clients keep working).
        assert_eq!(
            resolve_domain(None, &dual, HashDomain::Blake3),
            Ok(HashDomain::Blake3)
        );
        assert_eq!(
            resolve_domain(Some("   "), &dual, HashDomain::Blake3),
            Ok(HashDomain::Blake3)
        );

        // Served domain → honoured.
        assert_eq!(
            resolve_domain(Some("sha256"), &dual, HashDomain::Blake3),
            Ok(HashDomain::Sha256)
        );

        // Known domain the repo does not serve → NotServed, naming what it does.
        let err = resolve_domain(Some("sha256"), &native_only, HashDomain::Blake3)
            .expect_err("bridge-disabled repo must reject domain=sha256");
        assert_eq!(
            err,
            DomainError::NotServed {
                requested: HashDomain::Sha256,
                served: vec![HashDomain::Blake3],
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("sha256"), "message names the request: {msg}");
        assert!(
            msg.contains("blake3"),
            "message lists what is served: {msg}"
        );

        // Unrecognised value → Unrecognized, also listing what is served.
        let err = resolve_domain(Some("md5"), &dual, HashDomain::Blake3)
            .expect_err("unknown domain must be rejected");
        assert_eq!(
            err,
            DomainError::Unrecognized {
                requested: "md5".to_string(),
                served: vec![HashDomain::Blake3, HashDomain::Sha256],
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("md5"), "message names the bad value: {msg}");
        assert!(
            msg.contains("blake3, sha256"),
            "message lists served domains: {msg}"
        );
    }

    #[test]
    fn bucket_request_domain_is_optional_and_round_trips() {
        // Absent on the wire (every existing client).
        let old = format!(
            r#"{{"version":{},"prefix_bits":8,"buckets":["{}"]}}"#,
            PROTOCOL_VERSION,
            "00".repeat(32)
        );
        let parsed: BucketRequest = serde_json::from_str(&old).unwrap();
        assert_eq!(parsed.domain, None);

        let mut req = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits: 8,
            buckets: vec!["00".repeat(32)],
            since: None,
            domain: None,
            stream: false,
            resume_at: None,
        };
        assert!(
            !serde_json::to_string(&req).unwrap().contains("domain"),
            "None must be omitted so old repos see today's bytes"
        );

        req.domain = Some("sha256".to_string());
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""domain":"sha256""#), "{json}");
        let back: BucketRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    /// Minimal valid `Caps` with `hash_domain = Blake3` for use in tests below.
    fn base_caps() -> Caps {
        Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::WholeRepo,
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: Vec::new(),
            incremental_domains: None,
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        }
    }

    #[test]
    fn incremental_domains_absent_serialises_identically() {
        let caps = base_caps(); // incremental_domains: None
        let v = serde_json::to_value(&caps).unwrap();
        assert!(
            v.get("incremental_domains").is_none(),
            "None must be omitted on the wire"
        );
    }

    #[test]
    fn incremental_domains_three_states_round_trip() {
        // --- Deserialise all three states ---
        let empty: Caps =
            serde_json::from_str(r#"{"version":7,"mode":"wholerepo","incremental_domains":[]}"#)
                .unwrap();
        assert_eq!(empty.incremental_domains, Some(vec![]));

        let listed: Caps = serde_json::from_str(
            r#"{"version":7,"mode":"wholerepo","incremental_domains":["sha256"]}"#,
        )
        .unwrap();
        assert_eq!(listed.incremental_domains, Some(vec!["sha256".to_string()]));

        let absent: Caps = serde_json::from_str(r#"{"version":7,"mode":"wholerepo"}"#).unwrap();
        assert_eq!(absent.incremental_domains, None);

        // --- Serialise back and assert round-trips ---

        // Some([]) must serialise to `"incremental_domains":[]` (empty array, not absent).
        let v = serde_json::to_value(&empty).unwrap();
        assert_eq!(
            v["incremental_domains"],
            serde_json::json!([]),
            "Some([]) must serialise to an empty array, not be omitted"
        );

        // Some(["sha256"]) must round-trip through the array.
        let v = serde_json::to_value(&listed).unwrap();
        assert_eq!(
            v["incremental_domains"],
            serde_json::json!(["sha256"]),
            "listed state must round-trip its entries through serde"
        );

        // None must be omitted entirely on serialisation (skip_serializing_if).
        let v = serde_json::to_value(&absent).unwrap();
        assert!(
            v.get("incremental_domains").is_none(),
            "None must be omitted on re-serialisation, not written as null or []"
        );
    }

    #[test]
    fn delta_mapping_origin_is_optional_and_round_trips() {
        let manual = DeltaMapping {
            hash: "a".into(),
            tag: "x:y".into(),
            status: MappingStatus::Current,
            seq: 1,
            origin: None,
        };
        let json = serde_json::to_string(&manual).unwrap();
        assert!(!json.contains("\"origin\""), "omitted when None: {json}");
        assert_eq!(serde_json::from_str::<DeltaMapping>(&json).unwrap(), manual);

        let tagged = DeltaMapping {
            origin: Some("hydrus".into()),
            ..manual.clone()
        };
        let json = serde_json::to_string(&tagged).unwrap();
        assert!(json.contains("hydrus"));
        assert_eq!(serde_json::from_str::<DeltaMapping>(&json).unwrap(), tagged);
    }

    #[test]
    fn origin_tag_is_compact_and_round_trips() {
        let manual = OriginTag {
            tag: "x:y".into(),
            origin: None,
        };
        let json = serde_json::to_string(&manual).unwrap();
        assert_eq!(json, r#"{"tag":"x:y"}"#);
        assert_eq!(serde_json::from_str::<OriginTag>(&json).unwrap(), manual);
        let tagged = OriginTag {
            tag: "x:y".into(),
            origin: Some("gelbooru".into()),
        };
        assert_eq!(
            serde_json::from_str::<OriginTag>(&serde_json::to_string(&tagged).unwrap()).unwrap(),
            tagged
        );
    }

    #[test]
    fn snapshot_inferred_truth_table() {
        let mut caps = Caps {
            version: 6,
            mode: PullMode::Bucketed { prefix_bits: 256 },
            relation_incremental: false,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: Some(vec!["blake3".into()]),
            server_version: None,
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        // sha256 served, not incremental -> snapshot-backed
        assert!(caps.snapshot_inferred());
        // sha256 incremental -> materialized domain, not snapshot
        caps.incremental_domains = Some(vec!["blake3".into(), "sha256".into()]);
        assert!(!caps.snapshot_inferred());
        // pre-#142 server (no incremental_domains) but serves sha256 -> inferred
        // (accepted false positive for 0.2.52-0.58 servers; WARN-only consequence)
        caps.incremental_domains = None;
        assert!(caps.snapshot_inferred());
        // no sha256 at all -> never inferred
        caps.hash_domains = vec![HashDomain::Blake3];
        assert!(!caps.snapshot_inferred());
        // pre-dual-domain repo (empty hash_domains) -> never inferred
        caps.hash_domains = vec![];
        assert!(!caps.snapshot_inferred());
    }

    #[test]
    fn serves_deltas_matrix() {
        // Explicit list wins.
        let mut c = base_caps(); // hash_domain = Blake3
        c.incremental_domains = Some(vec!["sha256".to_string()]);
        assert!(c.serves_deltas(HashDomain::Sha256));
        assert!(!c.serves_deltas(HashDomain::Blake3));
        // Absent ⇒ fall back to mapping_incremental gating the native domain only.
        c.incremental_domains = None;
        c.mapping_incremental = true;
        assert!(
            c.serves_deltas(HashDomain::Blake3),
            "native domain served under fallback"
        );
        assert!(
            !c.serves_deltas(HashDomain::Sha256),
            "non-native domain never served under fallback"
        );
        c.mapping_incremental = false;
        assert!(!c.serves_deltas(HashDomain::Blake3));
        // Present-empty ⇒ nothing served anywhere.
        c.incremental_domains = Some(vec![]);
        assert!(!c.serves_deltas(HashDomain::Blake3));
        assert!(!c.serves_deltas(HashDomain::Sha256));
    }

    // --- Wire snapshot tests (insta) ----------------------------------------
    // These pin the exact JSON bytes so any wire-shape change is an explicit,
    // reviewed diff. Run `cargo insta review` after adding a new field.

    /// A `Caps` with no `serve_hint` must serialise byte-identically to a
    /// pre-#173 server — the field is absent, not `"serve_hint":{}`.
    #[test]
    fn caps_serve_hint_absent_when_empty() {
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::Bucketed { prefix_bits: 8 },
            relation_incremental: true,
            mapping_incremental: true,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: Some(vec!["blake3".to_string(), "sha256".to_string()]),
            server_version: Some("0.2.69".to_string()),
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            !json.contains("serve_hint"),
            "serve_hint must be absent from the wire when empty: {json}"
        );
        insta::assert_json_snapshot!("caps_no_serve_hint", caps);
    }

    /// #178: `hint_bits` round-trips when present, is omitted from the wire when
    /// `None`, and a pre-#178 wire form (no key) deserialises to `None`.
    #[test]
    fn serve_hint_hint_bits_wire_shape() {
        let with = ServeHint {
            ms_per_bucket: 0.2,
            hint_bits: Some(32),
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(
            json.contains("\"hint_bits\":32"),
            "Some(32) must emit the key: {json}"
        );
        assert_eq!(serde_json::from_str::<ServeHint>(&json).unwrap(), with);

        let without = ServeHint {
            ms_per_bucket: 0.4,
            hint_bits: None,
        };
        let json = serde_json::to_string(&without).unwrap();
        assert!(
            !json.contains("hint_bits"),
            "None must omit the key: {json}"
        );
        assert_eq!(serde_json::from_str::<ServeHint>(&json).unwrap(), without);

        // Pre-#178 wire form: no hint_bits key at all.
        let old: ServeHint = serde_json::from_str(r#"{"ms_per_bucket":0.4}"#).unwrap();
        assert_eq!(old.hint_bits, None, "absent key must parse as None");
    }

    /// A `Caps` with `serve_hint` populated for two domains pins the BTreeMap
    /// key order (blake3 < sha256 alphabetically) and the exact float encoding.
    #[test]
    fn caps_serve_hint_populated_two_domains() {
        // Insert in reverse-sorted order so the snapshot genuinely pins that
        // BTreeMap serialises by key order (blake3 < sha256), not insertion order.
        let mut hint = BTreeMap::new();
        hint.insert(
            "sha256".to_string(),
            ServeHint {
                ms_per_bucket: 56.5,
                hint_bits: Some(32),
            },
        );
        hint.insert(
            "blake3".to_string(),
            ServeHint {
                ms_per_bucket: 0.4,
                hint_bits: None,
            },
        );
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::Bucketed { prefix_bits: 8 },
            relation_incremental: true,
            mapping_incremental: true,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: Some(vec!["blake3".to_string(), "sha256".to_string()]),
            server_version: Some("0.2.69".to_string()),
            serve_hint: hint,
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            json.contains("serve_hint"),
            "serve_hint must be present when non-empty: {json}"
        );
        insta::assert_json_snapshot!("caps_with_serve_hint", caps);
    }

    // ── #176 Wire snapshot tests (test 16 from spec §5.3) ─────────────────────

    /// A `Caps` with `streaming: true` pins the one new key appended after
    /// `serve_hint` (or absent `serve_hint`). A `Caps` with `streaming: false`
    /// must be byte-identical to pre-#176 — the key is absent.
    #[test]
    fn caps_streaming_true_appends_exactly_one_key() {
        let mut caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::Bucketed { prefix_bits: 8 },
            relation_incremental: true,
            mapping_incremental: true,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: Some(vec!["blake3".to_string(), "sha256".to_string()]),
            server_version: Some("0.2.71".to_string()),
            serve_hint: Default::default(),
            streaming: false,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        // streaming: false → absent on the wire.
        let json_false = serde_json::to_string(&caps).unwrap();
        assert!(
            !json_false.contains("streaming"),
            "streaming: false must be absent from the wire: {json_false}"
        );
        // streaming: true → exactly one new key.
        caps.streaming = true;
        let json_true = serde_json::to_string(&caps).unwrap();
        assert!(
            json_true.contains("\"streaming\":true"),
            "streaming: true must be present on the wire: {json_true}"
        );
        // Round-trip.
        let back: Caps = serde_json::from_str(&json_true).unwrap();
        assert!(back.streaming);
        // Old server (no streaming key) → defaults to false.
        let old = r#"{"version":8,"mode":"bucketed","prefix_bits":8}"#;
        let old_caps: Caps = serde_json::from_str(old).unwrap();
        assert!(
            !old_caps.streaming,
            "absent streaming key defaults to false"
        );
        // Pin the wire form with streaming: true.
        insta::assert_json_snapshot!("caps_streaming_true", caps);
    }

    // ── #179 Wire snapshot tests ───────────────────────────────────────────────

    /// `effective_prefix_bits_floored` decision table — mirrors §6 of the #179
    /// spec.
    #[test]
    fn effective_prefix_bits_floored_decision_table() {
        // (advertised, ceiling, floor) → expected
        for (advertised, ceiling, floor, expected) in [
            // Row 1: floor absent → same as effective_prefix_bits
            (256u32, 24u32, None, 24u32),
            // Row 2: base ≥ floor → no raise
            (256, 24, Some(16), 24),
            // Row 3: base < floor → clamp up
            (256, 12, Some(16), 16),
            // Row 4: floor > advertised (hostile) → .min(advertised) caps it
            (256, 12, Some(300), 256),
            // ceiling above advertised: base = advertised, floor inert
            (20, 24, Some(16), 20),
            // floor > advertised: .min(advertised) caps
            (8, 24, Some(16), 8),
        ] {
            assert_eq!(
                effective_prefix_bits_floored(advertised, ceiling, floor),
                expected,
                "advertised={advertised} ceiling={ceiling} floor={floor:?}"
            );
        }
    }

    /// `Caps.min_query_bits` must be absent from the wire when `None`
    /// (byte-identical to pre-#179 caps).
    #[test]
    fn caps_min_query_bits_absent_when_none() {
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::Bucketed { prefix_bits: 8 },
            relation_incremental: true,
            mapping_incremental: true,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: Some(vec!["blake3".to_string(), "sha256".to_string()]),
            server_version: Some("0.2.73".to_string()),
            serve_hint: Default::default(),
            streaming: true,
            min_query_bits: None,
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            !json.contains("min_query_bits"),
            "min_query_bits must be absent from the wire when None: {json}"
        );
        insta::assert_json_snapshot!("caps_min_query_bits_absent", caps);
    }

    /// `Caps.min_query_bits: Some(16)` appends exactly one key after `streaming`,
    /// and round-trips correctly.
    #[test]
    fn caps_min_query_bits_appends_one_key() {
        let caps = Caps {
            version: PROTOCOL_VERSION,
            mode: PullMode::Bucketed { prefix_bits: 256 },
            relation_incremental: true,
            mapping_incremental: false,
            reports: false,
            repo_key: None,
            hash_domain: HashDomain::Blake3,
            hash_domains: vec![HashDomain::Blake3, HashDomain::Sha256],
            incremental_domains: Some(vec!["blake3".to_string()]),
            server_version: Some("0.2.73".to_string()),
            serve_hint: Default::default(),
            streaming: true,
            min_query_bits: Some(16),
            store_generation: None,
            count: None,
            name: None,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(
            json.contains("\"min_query_bits\":16"),
            "min_query_bits: Some(16) must be present on the wire: {json}"
        );
        // Round-trip.
        let back: Caps = serde_json::from_str(&json).unwrap();
        assert_eq!(back.min_query_bits, Some(16));
        // Old client (no min_query_bits field) deserialises fine.
        let old = r#"{"version":8,"mode":"bucketed","prefix_bits":256}"#;
        let old_caps: Caps = serde_json::from_str(old).unwrap();
        assert_eq!(old_caps.min_query_bits, None, "absent defaults to None");
        // Pin the wire form.
        insta::assert_json_snapshot!("caps_min_query_bits_some_16", caps);
    }

    /// `BucketRequest` with `stream`/`resume_at` set pins the additive request
    /// shape; a request without the new fields is byte-identical to pre-#176.
    #[test]
    fn bucket_request_stream_and_resume_at_additive() {
        let base = BucketRequest {
            version: PROTOCOL_VERSION,
            prefix_bits: 8,
            buckets: vec!["00".repeat(32)],
            since: None,
            domain: None,
            stream: false,
            resume_at: None,
        };
        // stream: false, resume_at: None → neither field on the wire.
        let base_json = serde_json::to_string(&base).unwrap();
        assert!(
            !base_json.contains("stream"),
            "stream: false absent: {base_json}"
        );
        assert!(
            !base_json.contains("resume_at"),
            "resume_at: None absent: {base_json}"
        );

        // stream: true → field present.
        let streaming = BucketRequest {
            stream: true,
            ..base.clone()
        };
        let s_json = serde_json::to_string(&streaming).unwrap();
        assert!(
            s_json.contains("\"stream\":true"),
            "stream: true present: {s_json}"
        );
        assert!(
            !s_json.contains("resume_at"),
            "resume_at still absent: {s_json}"
        );

        // stream: true, resume_at: Some → both fields present.
        let with_cursor = BucketRequest {
            stream: true,
            resume_at: Some("ab".repeat(32)),
            ..base.clone()
        };
        let c_json = serde_json::to_string(&with_cursor).unwrap();
        assert!(
            c_json.contains("\"stream\":true"),
            "stream present: {c_json}"
        );
        assert!(
            c_json.contains("\"resume_at\""),
            "resume_at present: {c_json}"
        );

        // Round-trip both.
        let back: BucketRequest = serde_json::from_str(&c_json).unwrap();
        assert_eq!(back, with_cursor);

        // Old server ignores unknown fields (no deny_unknown_fields).
        let back_base: BucketRequest = serde_json::from_str(&base_json).unwrap();
        assert!(!back_base.stream);
        assert_eq!(back_base.resume_at, None);

        // Pin wire shapes.
        insta::assert_json_snapshot!("bucket_request_stream_only", streaming);
        insta::assert_json_snapshot!("bucket_request_stream_with_cursor", with_cursor);
    }

    // ── #176 NDJSON frame round-trip tests (step 2) ──────────────────────────

    #[test]
    fn stream_header_round_trips() {
        let h = StreamHeader {
            version: 8,
            cursor: 42,
        };
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(
            json, r#"{"version":8,"cursor":42}"#,
            "exact wire form: {json}"
        );
        let back: StreamHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn stream_row_round_trips() {
        let row = StreamRow {
            h: "aa".repeat(32),
            t: vec![
                OriginTag {
                    tag: "character:samus".into(),
                    origin: None,
                },
                OriginTag {
                    tag: "maid".into(),
                    origin: Some("hydrus".into()),
                },
            ],
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"h\":"), "h key present: {json}");
        assert!(json.contains("\"t\":"), "t key present: {json}");
        assert!(
            json.contains("character:samus"),
            "tag value present: {json}"
        );
        assert!(!json.contains("\"h\":\"aa\""), "not a flat key: {json}"); // h is the full hash
        let back: StreamRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn stream_trailer_done_round_trips() {
        let t = StreamTrailer::Done { done: true };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#"{"done":true}"#, "exact wire form: {json}");
        let back: StreamTrailer = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn stream_trailer_more_round_trips() {
        let t = StreamTrailer::More {
            more: "ab".repeat(32),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.starts_with(r#"{"more":"#), "more key: {json}");
        let back: StreamTrailer = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn stream_trailer_err_round_trips() {
        let t = StreamTrailer::Err {
            err: "bucket exceeds budget".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.starts_with(r#"{"err":"#), "err key: {json}");
        let back: StreamTrailer = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn caps_count_is_additive_and_backcompat() {
        let json = r#"{"version":7,"mode":"bucketed","prefix_bits":18}"#;
        let caps: Caps = serde_json::from_str(json).expect("deserialize old caps");
        assert_eq!(caps.count, None);
        let out = serde_json::to_string(&caps).unwrap();
        assert!(!out.contains("count"), "None count must be skipped: {out}");
        let with = Caps {
            count: Some(94_317),
            ..caps
        };
        let s = serde_json::to_string(&with).unwrap();
        assert!(s.contains("\"count\":94317"), "serialized: {s}");
        let back: Caps = serde_json::from_str(&s).unwrap();
        assert_eq!(back.count, Some(94_317));
    }

    /// The three trailer variants are structurally distinct: `done` is a boolean,
    /// `more`/`err` are strings, so the untagged deserialization is unambiguous.
    #[test]
    fn stream_trailer_variants_are_unambiguous() {
        // done: a boolean field
        let done: StreamTrailer = serde_json::from_str(r#"{"done":true}"#).unwrap();
        assert!(matches!(done, StreamTrailer::Done { .. }));
        // more: a string field named "more"
        let more: StreamTrailer = serde_json::from_str(r#"{"more":"aabb"}"#).unwrap();
        assert!(matches!(more, StreamTrailer::More { more: ref m } if m == "aabb"));
        // err: a string field named "err"
        let err: StreamTrailer = serde_json::from_str(r#"{"err":"something failed"}"#).unwrap();
        assert!(matches!(err, StreamTrailer::Err { err: ref e } if e == "something failed"));
    }
}
