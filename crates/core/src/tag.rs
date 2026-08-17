//! Namespaced tag vocabulary: `namespace:subtag`, normalized to a canonical form.
//!
//! Parsing rules (Hydrus convention, issue #77):
//!
//! 1. The input is trimmed to `t`.
//! 2. If `t` starts with `:`, let `rest = &t[1..]`:
//!    - `rest` has **no** further `:` → unnamespaced tag whose subtag text is
//!      `normalize(t)` (the leading colon is kept: `:)` → subtag `":)"`).
//!    - `rest` has at least one `:` → unnamespaced tag whose subtag text is
//!      `normalize(rest)` (`::)` → `":)"`, `:a:b` → `"a:b"`, `::` → `":"`).
//! 3. Otherwise `split_once(':')` gives `(namespace, subtag)`; no colon at all
//!    means an empty namespace.
//! 4. [`Error::EmptyTag`] is returned if the resulting subtag is empty.
//!
//! **Display / canonical form** — round-trip guarantee:
//! - Unnamespaced tag whose subtag contains `:` is printed with a leading `:`
//!   (subtag `":)"` → `"::)"`, subtag `"a:b"` → `":a:b"`).
//! - Unnamespaced tag with no `:` in subtag → subtag as-is.
//! - Namespaced tag → `namespace:subtag` (unchanged).
//!
//! Previously valid canonical tags (`shield`, `character:samus`) parse and
//! display exactly as before — no stored-data compatibility break.

use std::fmt;
use std::str::FromStr;

use crate::Error;

/// A namespaced tag. An empty `namespace` means the tag is unnamespaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// Namespace (`character`, `creator`, ...); empty string if unnamespaced.
    pub namespace: String,
    /// The tag value. Never empty (parsing rejects an empty subtag).
    pub subtag: String,
}

impl Tag {
    /// Parse and normalize a tag string. See the module docs for the rules.
    ///
    /// Leading-colon tags are unnamespaced, with the colon as part of the
    /// subtag content. The canonical string form doubles the leading colon:
    /// `:)` parses to subtag `":)"` and displays as `"::)"`.
    ///
    /// # Errors
    /// Returns [`Error::EmptyTag`] if there is no usable subtag.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let t = s.trim();
        let (namespace, subtag) = if let Some(rest) = t.strip_prefix(':') {
            if rest.contains(':') {
                // `::)` → subtag `:)`, `:a:b` → subtag `a:b`, `::` → subtag `:`
                (String::new(), normalize(rest))
            } else {
                // `:)` → subtag `:)`, `:shield` → subtag `:shield`, `:` → subtag `:`
                (String::new(), normalize(t))
            }
        } else {
            match t.split_once(':') {
                Some((ns, sub)) => (normalize(ns), normalize(sub)),
                None => (String::new(), normalize(t)),
            }
        };
        if subtag.is_empty() {
            return Err(Error::EmptyTag(s.to_string()));
        }
        Ok(Tag { namespace, subtag })
    }
}

/// Trim, collapse internal whitespace to single spaces, and lowercase.
pub(crate) fn normalize(part: &str) -> String {
    part.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl FromStr for Tag {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Tag::parse(s)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.namespace.is_empty() {
            if self.subtag.contains(':') {
                write!(f, ":{}", self.subtag)
            } else {
                write!(f, "{}", self.subtag)
            }
        } else {
            write!(f, "{}:{}", self.namespace, self.subtag)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_tag_parses() {
        let t = Tag::parse("character:samus").unwrap();
        assert_eq!(t.namespace, "character");
        assert_eq!(t.subtag, "samus");
        assert_eq!(t.to_string(), "character:samus");
    }

    #[test]
    fn bare_tag_has_empty_namespace() {
        let t = Tag::parse("blue_sky").unwrap();
        assert_eq!(t.namespace, "");
        assert_eq!(t.subtag, "blue_sky");
        assert_eq!(t.to_string(), "blue_sky");
    }

    #[test]
    fn whitespace_is_collapsed_and_lowercased() {
        let t = Tag::parse("Character:  Samus   Aran ").unwrap();
        assert_eq!(t.namespace, "character");
        assert_eq!(t.subtag, "samus aran");
    }

    #[test]
    fn empty_subtag_is_rejected() {
        assert!(matches!(
            Tag::parse("character:").unwrap_err(),
            Error::EmptyTag(_)
        ));
        assert!(matches!(Tag::parse("   ").unwrap_err(), Error::EmptyTag(_)));
    }

    #[test]
    fn non_ascii_round_trips() {
        let t = Tag::parse("名前:日本語").unwrap();
        assert_eq!(t.namespace, "名前");
        assert_eq!(t.subtag, "日本語");
        assert_eq!(t.to_string(), "名前:日本語");
    }

    // --- Leading-colon (Hydrus convention, issue #77) ---

    #[test]
    fn leading_colon_emoticon_round_trips() {
        // Single leading colon, no further colons: keep the colon in the subtag.
        let t = Tag::parse(":)").unwrap();
        assert_eq!(t.namespace, "");
        assert_eq!(t.subtag, ":)");
        // Canonical form doubles the leading colon so the subtag colon is preserved.
        assert_eq!(t.to_string(), "::)");
        // Round-trip: parse the canonical form gives the same tag.
        let back = Tag::parse("::)").unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn leading_colon_is_not_equal_to_bare_tag() {
        // `:shield` and `shield` are distinct tags.
        let with_colon = Tag::parse(":shield").unwrap();
        let without_colon = Tag::parse("shield").unwrap();
        assert_eq!(with_colon.subtag, ":shield");
        assert_ne!(with_colon, without_colon);
    }

    #[test]
    fn leading_colon_lowercased() {
        // `:D` → starts with `:`, rest `D` has no `:` → subtag = normalize(":D") = ":d"
        let t = Tag::parse(":D").unwrap();
        assert_eq!(t.namespace, "");
        assert_eq!(t.subtag, ":d");
        // subtag contains `:` → display prepends another `:`
        assert_eq!(t.to_string(), "::d");
    }

    #[test]
    fn double_leading_colon_shapes() {
        // `::` → rest = `:`, rest contains `:` → subtag = normalize(`:`) = `:`
        let t = Tag::parse("::").unwrap();
        assert_eq!(t.namespace, "");
        assert_eq!(t.subtag, ":");
        assert_eq!(t.to_string(), "::");

        // `:::` → rest = `::`, rest contains `:` → subtag = normalize(`::`) = `::`
        let t2 = Tag::parse(":::").unwrap();
        assert_eq!(t2.subtag, "::");
        assert_eq!(t2.to_string(), ":::");
    }

    #[test]
    fn leading_colon_with_namespace_like_form() {
        // `:a:b` → rest = `a:b`, rest contains `:` → subtag = `a:b`, displays `:a:b`
        let t = Tag::parse(":a:b").unwrap();
        assert_eq!(t.namespace, "");
        assert_eq!(t.subtag, "a:b");
        assert_eq!(t.to_string(), ":a:b");
        let back = Tag::parse(&t.to_string()).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn bare_colon_only_is_subtag_colon() {
        // `:` alone → rest = `""`, no `:` in rest → subtag = normalize(`:`) = `:`
        let t = Tag::parse(":").unwrap();
        assert_eq!(t.namespace, "");
        assert_eq!(t.subtag, ":");
        assert_eq!(t.to_string(), "::");
    }

    // --- Spec §6a: table test for parse fields, Display, and round-trip ---

    struct Case {
        input: &'static str,
        ns: Option<&'static str>, // None means Err(EmptyTag)
        sub: &'static str,
        display: &'static str,
    }

    #[test]
    fn spec_table_parse_display_roundtrip() {
        let cases: &[Case] = &[
            Case {
                input: ":)",
                ns: Some(""),
                sub: ":)",
                display: "::)",
            },
            Case {
                input: "::)",
                ns: Some(""),
                sub: ":)",
                display: "::)",
            },
            Case {
                input: "a:b",
                ns: Some("a"),
                sub: "b",
                display: "a:b",
            },
            Case {
                input: ":a:b",
                ns: Some(""),
                sub: "a:b",
                display: ":a:b",
            },
            Case {
                input: "",
                ns: None,
                sub: "",
                display: "",
            },
            Case {
                input: "   ",
                ns: None,
                sub: "",
                display: "",
            },
        ];
        for case in cases {
            match case.ns {
                None => {
                    assert!(
                        Tag::parse(case.input).is_err(),
                        "expected Err(EmptyTag) for {:?}",
                        case.input
                    );
                }
                Some(expected_ns) => {
                    let t = Tag::parse(case.input).unwrap_or_else(|e| {
                        panic!("unexpected parse error for {:?}: {:?}", case.input, e)
                    });
                    assert_eq!(
                        t.namespace, expected_ns,
                        "namespace mismatch for {:?}",
                        case.input
                    );
                    assert_eq!(t.subtag, case.sub, "subtag mismatch for {:?}", case.input);
                    assert_eq!(
                        t.to_string(),
                        case.display,
                        "Display mismatch for {:?}",
                        case.input
                    );
                    // Round-trip: parse(Display(x)) == x
                    let back = Tag::parse(&t.to_string()).unwrap();
                    assert_eq!(t, back, "round-trip failed for {:?}", case.input);
                }
            }
        }
    }

    // --- Spec §6a: fixed-point sweep over old-build-storable pairs ---

    #[test]
    fn fixed_point_sweep_old_build_storable_pairs() {
        // Every (namespace, subtag) pair that an old build could store should
        // already be a fixed point of parse∘Display: parse(Display(pair)) == pair.
        // This means migration 0031 is a no-op on real data — none of these change.
        let pairs: &[(&str, &str)] = &[
            ("", "smile"),
            ("", ")"),
            ("character", "samus"),
            ("a", ":b"),
        ];
        for &(ns, sub) in pairs {
            let tag = Tag {
                namespace: ns.to_string(),
                subtag: sub.to_string(),
            };
            let display = tag.to_string();
            let parsed = Tag::parse(&display).unwrap_or_else(|e| {
                panic!(
                    "fixed-point: parse(Display({:?}:{:?})) failed: {:?}",
                    ns, sub, e
                )
            });
            assert_eq!(
                parsed, tag,
                "fixed-point violated for ({:?}, {:?}): Display={:?}, re-parsed={:?}",
                ns, sub, display, parsed
            );
        }
    }
}
