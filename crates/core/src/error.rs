use std::path::PathBuf;

/// Errors produced by core operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A hash string was not valid 64-character lowercase hex.
    #[error("invalid hash hex: {0}")]
    InvalidHashHex(String),

    /// A `files.state` string was not one of active/archived/trashed.
    #[error("invalid file state: {0}")]
    InvalidFileState(String),

    /// A tag string had no usable subtag after normalization.
    #[error("empty tag: {0}")]
    EmptyTag(String),

    /// A search wildcard pattern was malformed or unsupported.
    #[error("unsupported wildcard pattern: {0}")]
    BadPattern(String),

    /// A `system:` predicate was malformed or unsupported.
    #[error("unsupported system predicate: {0}")]
    BadSystem(String),

    /// A search token stream was malformed (e.g. a misplaced `or`, or a wildcard
    /// or `system:` token inside an `or` group).
    #[error("malformed query: {0}")]
    BadQuery(String),

    /// An I/O error occurred while hashing a file or reader.
    #[error("io error for {path}: {source}")]
    Io {
        /// The path being processed when the error occurred.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Conservative per-row byte overhead charged on top of `key.len() + tag_len`
/// when accounting a bucket JSON response against its budget (#145, #166).
///
/// Derived from the **bucket_delta** element framing — the stricter shape:
/// ```text
/// {"hash":"<64>","tag":"<tag>","status":"current","seq":<N>},
/// ```
/// Fixed bytes excluding the 64-char hash value and the tag value:
///   `{"hash":"` (9) + `","tag":"` (9) + `","status":"current","seq":` (27)
///   + **20** (max u64 seq digits — u64::MAX has 20 decimal digits) + `},` (2) = **67**.
///
/// Using the maximum seq width (20) rather than the observed width is a
/// static, simple bound that errs high by at most 13 bytes per row on a
/// mature repo — the correct direction for a budget that must be an upper
/// bound, not a tight estimate.
///
/// The flat delta path carries `status` and `seq` fields that the snapshot
/// path omits; the snapshot path has no per-row cushion beyond this constant,
/// so using the larger delta shape as the floor makes the budget conservative
/// for both paths.
///
/// The origin framing (`,"origin":"..."`) is charged **separately** at the
/// drain sites (see [`json_escaped_len`] and its callers) so that rows without
/// origin carry no origin charge.
///
/// The per-response envelope (`{"version":…,"cursor":…,"changes":[…]}`) is
/// charged once at the start of each drain via [`RESPONSE_ENVELOPE_OVERHEAD`].
///
/// Together, `BUCKET_ROW_OVERHEAD` per row + `RESPONSE_ENVELOPE_OVERHEAD`
/// once = the invariant **charged ≥ actual serialized response body**.
pub const BUCKET_ROW_OVERHEAD: usize = 67;

/// One-time charge for the JSON response envelope, applied before any row is
/// drained (#166).
///
/// The delta envelope worst case:
/// ```text
/// {"version":8,"cursor":18446744073709551615,"changes":[]}
/// ```
/// = `{"version":8,"cursor":` (22) + 20 (max u64 digits) + `,"changes":[]}` (14)
/// = **56** bytes. The snapshot envelope (`,"tags":{}`) is 53 bytes. 64 is the
/// next round number above 56, giving a small slack. Charged once at the top
/// of each drain loop so the per-row overhead only covers per-row bytes.
pub const RESPONSE_ENVELOPE_OVERHEAD: usize = 64;

/// Approximate serialized cost of one `(key, tag)` mapping row in the bucket
/// JSON response (#145). Conservative upper bound — see [`BUCKET_ROW_OVERHEAD`].
///
/// `tag_len` should be the raw UTF-8 byte count of the tag value. When origin
/// is present, callers add `json_escaped_len(origin) + 12` to `tag_len` before
/// calling (the `+12` covers the `,"origin":"..."` framing: `,` + `"origin"` +
/// `:` + opening quote + closing quote = 12 bytes around the escaped value).
///
/// The response envelope is NOT included here; it is charged once at drain
/// start via [`RESPONSE_ENVELOPE_OVERHEAD`].
#[must_use]
pub fn approx_row_cost(key_len: usize, tag_len: usize) -> usize {
    key_len + tag_len + BUCKET_ROW_OVERHEAD
}

/// Exact JSON-serialized byte length of a string value, as serde_json would
/// produce it — **excluding** the surrounding quote characters (#166).
///
/// serde_json escaping rules applied:
/// - `"` (0x22) and `\` (0x5C) → 2-byte `\"` / `\\` escape.
/// - Short escapes: `\b` (0x08), `\t` (0x09), `\n` (0x0A), `\f` (0x0C),
///   `\r` (0x0D) → 2 bytes.
/// - Other control chars < 0x20 → `\u00XX` (6 bytes).
/// - DEL (0x7F): serde_json does **not** escape it; counts as 1 byte.
/// - All other bytes (multi-byte UTF-8, printable ASCII) → 1 byte each.
///
/// Verify() (#166 commit 1) blocks control chars at the wire gate, but quotes
/// and backslashes remain legal in origins; this function is correct for all
/// valid inputs and also for any legacy data already in the DB.
#[must_use]
pub fn json_escaped_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            // " and \ each become a 2-byte escape sequence.
            0x22 | 0x5C => 2,
            // Short-escape control chars: \b \t \n \f \r → 2 bytes.
            0x08 | 0x09 | 0x0A | 0x0C | 0x0D => 2,
            // Other control chars → \u00XX (6 bytes).
            b if b < 0x20 => 6,
            // Everything else: printable ASCII, DEL (0x7F), multi-byte UTF-8
            // continuation/leading bytes → emitted verbatim (1 byte each).
            _ => 1,
        })
        .sum()
}

/// A bucket response exceeded the server's per-request size budget (#145).
///
/// Carries the approximate byte budget for diagnostics only; the accounting is
/// not exact (see [`approx_row_cost`]). The HTTP layer maps this to `413` with a
/// remedy body that names no filesystem path (#159).
#[derive(Debug, thiserror::Error)]
#[error("bucket response exceeded the {budget}-byte per-request budget")]
pub struct BudgetExceeded {
    /// The per-request budget in bytes.
    pub budget: usize,
}

#[cfg(test)]
mod budget_tests {
    use super::{BUCKET_ROW_OVERHEAD, BudgetExceeded, approx_row_cost, json_escaped_len};

    #[test]
    fn approx_row_cost_is_key_plus_tag_plus_overhead() {
        assert_eq!(approx_row_cost(10, 5), 15 + BUCKET_ROW_OVERHEAD);
        assert_eq!(approx_row_cost(0, 0), BUCKET_ROW_OVERHEAD);
        // A 64-char sha with a short tag — the shape the bucket loops charge.
        assert_eq!(approx_row_cost(64, 4), 68 + BUCKET_ROW_OVERHEAD);
    }

    #[test]
    fn budget_accumulator_trips_exactly_at_the_boundary() {
        // Two identical rows; the budget admits the first exactly and rejects
        // the second. This mirrors the drain loop: charge, then compare.
        let cost = approx_row_cost(64, 4);
        let budget = cost; // room for exactly one row
        let mut spent: usize = 0;
        let mut admitted = 0;
        for _ in 0..2 {
            spent = spent.saturating_add(cost);
            if spent > budget {
                break;
            }
            admitted += 1;
        }
        assert_eq!(admitted, 1, "exactly one row fits a one-row budget");
    }

    #[test]
    fn budget_exceeded_display_names_the_budget() {
        let e = BudgetExceeded { budget: 64 };
        assert_eq!(
            e.to_string(),
            "bucket response exceeded the 64-byte per-request budget"
        );
    }

    #[test]
    fn json_escaped_len_matches_serde_json_output() {
        // Each case: assert our function equals serde_json's actual escaped
        // length (serde_json::to_string wraps in quotes, so subtract 2).
        let cases: &[&str] = &[
            // Basic printable ASCII — no escaping.
            "hello",
            // Quote and backslash → 2-byte escapes.
            r#"has "quotes""#,
            r"has \backslash",
            r#"both \" together"#,
            // Short-escape control chars.
            "back\x08space",
            "tab\x09char",
            "new\x0Aline",
            "form\x0Cfeed",
            "carr\x0Dret",
            // Non-short control char → \u00XX (6 bytes per char).
            "\x01control",
            // Multi-byte UTF-8 — emitted verbatim; no per-byte expansion.
            "日本語",
            // DEL (0x7F): serde_json does NOT escape it.
            "del\x7Fchar",
            // Mix of everything.
            "mix \"quotes\" and \\slashes and \n newlines and \x01 ctrl",
        ];
        for s in cases {
            let expected = serde_json::to_string(s).unwrap().len() - 2; // strip surrounding ""
            assert_eq!(
                json_escaped_len(s),
                expected,
                "json_escaped_len mismatch for {s:?}: got {}, want {expected}",
                json_escaped_len(s)
            );
        }
    }
}
