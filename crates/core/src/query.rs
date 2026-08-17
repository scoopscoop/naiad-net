//! Search query AST (README §5). A query is a flat list of predicates that are
//! implicitly AND'd together; relation expansion is applied when the query is
//! evaluated (see `crate::relations::match_set` and the `db` layer).

use crate::Error;
use crate::Tag;
use crate::tag::normalize;

/// A supported tag-search wildcard pattern, parsed from a token containing `*`.
/// `*` matches zero or more characters and may appear anywhere in the subtag
/// (`samus*`, `*samus`, `sam*us`, `*samus*`). The namespace side is matched
/// literally — `*` is not (yet) permitted in the namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagPattern {
    /// `namespace:*` — every tag in this namespace.
    NamespaceAny { namespace: String },
    /// `namespace:<glob>` — this namespace, subtag matching `glob` (with `*`s).
    NamespaceGlob { namespace: String, glob: String },
    /// `<glob>` — any namespace, subtag matching `glob` (with `*`s).
    AnyNamespaceGlob { glob: String },
}

impl std::fmt::Display for TagPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TagPattern::NamespaceAny { namespace } => write!(f, "{namespace}:*"),
            TagPattern::NamespaceGlob { namespace, glob } => {
                write!(f, "{namespace}:{glob}")
            }
            TagPattern::AnyNamespaceGlob { glob } => write!(f, "{glob}"),
        }
    }
}

impl TagPattern {
    /// Parse a wildcard pattern. At least one `*` must appear on the subtag side;
    /// it may sit anywhere in the subtag.
    ///
    /// # Errors
    /// Returns [`Error::BadPattern`] for a malformed or unsupported pattern
    /// (no `*` on the subtag side, a `*` in the namespace, an empty namespace
    /// before a `:`, or an all-`*` pattern with no literal text to anchor it).
    pub fn parse(s: &str) -> Result<TagPattern, Error> {
        let bad = || Error::BadPattern(s.to_string());
        let (ns_part, sub_part, namespaced) = match s.split_once(':') {
            Some((ns, sub)) => (ns, sub, true),
            None => ("", s, false),
        };
        // At least one `*` on the (trimmed) subtag side; none in the namespace.
        let sub = sub_part.trim();
        if ns_part.contains('*') || !sub.contains('*') {
            return Err(bad());
        }
        let namespace = normalize(ns_part);
        let glob = normalize(sub); // keeps the `*`s; lowercases/collapses the rest
        // A glob that is nothing but `*`s has no literal text to anchor on: as a
        // namespaced pattern it means the whole namespace, but bare it would match
        // every tag — too broad, so reject it.
        let only_stars = glob.chars().all(|c| c == '*');

        if namespaced {
            if namespace.is_empty() {
                return Err(bad()); // ":*" or ":foo*"
            }
            if only_stars {
                Ok(TagPattern::NamespaceAny { namespace })
            } else {
                Ok(TagPattern::NamespaceGlob { namespace, glob })
            }
        } else if only_stars {
            Err(bad()) // bare "*"
        } else {
            Ok(TagPattern::AnyNamespaceGlob { glob })
        }
    }
}

/// A filterable intrinsic-metadata field. Maps to a `files` column in `db`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysField {
    /// File size in bytes (`files.size`).
    Size,
    /// Pixel width (`files.width`).
    Width,
    /// Pixel height (`files.height`).
    Height,
    /// Duration in milliseconds (`files.duration_ms`).
    Duration,
}

/// A numeric comparison operator for a [`SystemPredicate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `>`
    Gt,
    /// `<`
    Lt,
    /// `>=`
    Ge,
    /// `<=`
    Le,
    /// `=`
    Eq,
}

/// A system (metadata) predicate: a filter on a file's intrinsic metadata rather
/// than its tags. Parsed from the body of a `system:` query token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemPredicate {
    /// `field op value`, with `value` normalized to the field's base unit
    /// (bytes for size, milliseconds for duration, pixels for width/height).
    Compare {
        /// Which metadata field to compare.
        field: SysField,
        /// The comparison operator.
        op: CmpOp,
        /// The right-hand value in base units.
        value: i64,
    },
    /// `type=<mime>` — exact filetype (mime) match.
    FileType {
        /// The mime string, lowercased.
        mime: String,
    },
    /// `origin=<name>` — filter on a tag's generation source (ADR 0026).
    /// `None` = the reserved `manual` value: origin-less rows (`origin_id IS NULL`).
    /// `Some(name)` = a named origin, matched case-insensitively against `origins.name`.
    Origin { name: Option<String> },
}

impl SystemPredicate {
    /// Parse the body of a `system:` token (the text after the `system:` prefix).
    ///
    /// Grammar: either `type=<mime>` (exact filetype) or
    /// `<field><op><number><unit?>` where `field` ∈ {size,width,height,duration},
    /// `op` ∈ {`>=`,`<=`,`>`,`<`,`=`}, the magnitude is a non-negative integer, and
    /// the optional unit is field-appropriate (size: b/kb/mb/gb 1024-based;
    /// duration: ms/s/m/h; width/height: none). Matching is case-insensitive.
    ///
    /// # Errors
    /// Returns [`Error::BadSystem`] for any malformed or unsupported body.
    pub fn parse(body: &str) -> Result<SystemPredicate, Error> {
        let original = body;
        let bad = || Error::BadSystem(original.to_string());
        let body = body.trim().to_lowercase();

        // Filetype: `type=<mime>`.
        if let Some(rest) = body.strip_prefix("type=") {
            let mime = rest.trim();
            if mime.is_empty() {
                return Err(bad());
            }
            return Ok(SystemPredicate::FileType {
                mime: mime.to_string(),
            });
        }

        // Origin: `origin=<name>` (only `=`). Placed before the numeric field split.
        //
        // Case note: the shared `to_lowercase()` above is Unicode-aware, but the
        // executor matches with SQLite `COLLATE NOCASE` (ASCII-only fold). A
        // non-ASCII origin name with uppercase letters (e.g. `Ünïcode`) is thus
        // unmatchable even by an exact-case query — accepted limitation while
        // producers are ASCII tool names (ADR 0026 addendum, #165).
        if let Some(rest) = body.strip_prefix("origin") {
            let value = rest.strip_prefix('=').ok_or_else(bad)?; // any non-`=` op → BadSystem
            let value = value.trim();
            if value.is_empty() {
                return Err(bad()); // `origin=` with no value
            }
            let name = if value == "manual" {
                None
            } else {
                Some(value.to_string())
            };
            return Ok(SystemPredicate::Origin { name });
        }

        // Numeric comparison: split off the field keyword, then the operator.
        let (field, rest) = if let Some(r) = body.strip_prefix("size") {
            (SysField::Size, r)
        } else if let Some(r) = body.strip_prefix("width") {
            (SysField::Width, r)
        } else if let Some(r) = body.strip_prefix("height") {
            (SysField::Height, r)
        } else if let Some(r) = body.strip_prefix("duration") {
            (SysField::Duration, r)
        } else {
            return Err(bad());
        };

        // Two-char operators must be tried before the one-char prefixes.
        let (op, rest) = if let Some(r) = rest.strip_prefix(">=") {
            (CmpOp::Ge, r)
        } else if let Some(r) = rest.strip_prefix("<=") {
            (CmpOp::Le, r)
        } else if let Some(r) = rest.strip_prefix('>') {
            (CmpOp::Gt, r)
        } else if let Some(r) = rest.strip_prefix('<') {
            (CmpOp::Lt, r)
        } else if let Some(r) = rest.strip_prefix('=') {
            (CmpOp::Eq, r)
        } else {
            return Err(bad());
        };

        // Split leading ASCII digits from a trailing unit.
        let split = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (digits, unit) = rest.split_at(split);
        if digits.is_empty() {
            return Err(bad());
        }
        let magnitude: i64 = digits.parse().map_err(|_| bad())?;
        let multiplier = unit_multiplier(field, unit).ok_or_else(bad)?;
        let value = magnitude.checked_mul(multiplier).ok_or_else(bad)?;
        Ok(SystemPredicate::Compare { field, op, value })
    }
}

/// The base-unit multiplier for `unit` under `field`, or `None` if the unit is not
/// valid for that field. An empty unit means "base unit" (bytes/ms/px).
fn unit_multiplier(field: SysField, unit: &str) -> Option<i64> {
    match field {
        SysField::Size => match unit {
            "" | "b" => Some(1),
            "kb" => Some(1024),
            "mb" => Some(1024 * 1024),
            "gb" => Some(1024 * 1024 * 1024),
            _ => None,
        },
        SysField::Duration => match unit {
            "" | "ms" => Some(1),
            "s" => Some(1000),
            "m" => Some(60_000),
            "h" => Some(3_600_000),
            _ => None,
        },
        // Dimensions are always bare pixels: no unit permitted.
        SysField::Width | SysField::Height => match unit {
            "" => Some(1),
            _ => None,
        },
    }
}

/// Whether a single predicate matches through relation expansion (the default)
/// or literally — the per-term `=` exact operator (issue #9). Mirrors
/// `naiad_db::Expansion`, but scoped to one predicate rather than the whole query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    /// Apply sibling (alias) and parent (implication) relations (the default).
    Expanded,
    /// Match only literally-stored tags (the `=tag` operator).
    Exact,
}

/// One predicate in a search query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    /// A file must have this tag (effectively, per `mode`).
    Tag(Tag, MatchMode),
    /// A file must NOT have this tag (effectively, per `mode`).
    Not(Tag, MatchMode),
    /// A file must have at least one of these tags; each member carries its own
    /// match mode. An empty group matches nothing (the identity of OR); the
    /// parser only ever produces a group of two or more.
    Or(Vec<(Tag, MatchMode)>),
    /// A file must have some tag matching this pattern (effectively, per `mode`).
    Wild(TagPattern, MatchMode),
    /// A file must NOT have any tag matching this pattern (effectively, per `mode`).
    NotWild(TagPattern, MatchMode),
    /// A file must match this system (metadata) predicate. Unaffected by
    /// expansion, so it carries no mode.
    System(SystemPredicate),
    /// A file must NOT match this system (metadata) predicate.
    NotSystem(SystemPredicate),
}

/// A search query: `predicates` are implicitly AND'd together.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Query {
    /// The conjoined predicates.
    pub predicates: Vec<Predicate>,
}

/// If `tok` begins with a case-insensitive `system:` prefix, return the body that
/// follows it. Uses `str::get` so a multibyte token (e.g. `もぐらもぐら`) whose
/// byte length straddles `PREFIX.len()` returns `None` instead of panicking on a
/// non-char-boundary slice.
fn strip_system_prefix(tok: &str) -> Option<&str> {
    const PREFIX: &str = "system:";
    let head = tok.get(..PREFIX.len())?;
    if head.eq_ignore_ascii_case(PREFIX) {
        Some(&tok[PREFIX.len()..])
    } else {
        None
    }
}

/// Strip a single leading `=` exact-marker from a term token, returning the
/// remaining text and the resulting match mode. No leading `=` ⇒ `Expanded`.
fn strip_exact(tok: &str) -> (&str, MatchMode) {
    match tok.strip_prefix('=') {
        Some(rest) => (rest, MatchMode::Exact),
        None => (tok, MatchMode::Expanded),
    }
}

/// Split a raw query string into tokens, honoring double-quoted phrases so a tag
/// containing spaces survives as one token (`"zero mission"` ⇒ `zero mission`,
/// `character:"zero mission"` ⇒ `character:zero mission`). Quotes are removed and
/// only group whitespace — every other character (including `*`, `-`, `:`) passes
/// through untouched, so wildcards and operators still work inside a phrase. An
/// unterminated quote runs to the end of the string. Empty tokens are dropped.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Parse a Hydrus-style token stream into a [`Query`]. Tokens are AND'd; a leading
/// `-` negates; the bareword `or` (case-insensitive) joins adjacent positive tags
/// into one OR-group; a token containing `*` is a wildcard pattern (standalone, not
/// allowed inside an OR-group); a `system:` token is a metadata predicate (also
/// standalone). No nested grouping.
///
/// # Errors
/// Returns [`Error::BadQuery`] on an empty token list, a misplaced `or` (leading,
/// trailing, or adjacent to a negated tag), or a wildcard/`system:` token inside an
/// OR-group. Returns the underlying parse error ([`Error::EmptyTag`],
/// [`Error::BadPattern`], [`Error::BadSystem`]) for a malformed tag, pattern, or
/// system predicate.
pub fn parse_query(tokens: &[String]) -> Result<Query, Error> {
    if tokens.is_empty() {
        return Err(Error::BadQuery("no search predicates".into()));
    }
    let is_or = |t: &str| t.eq_ignore_ascii_case("or");
    let is_wild = |t: &str| t.contains('*');

    let mut predicates = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if is_or(tok) {
            return Err(Error::BadQuery("dangling 'or' in query".into()));
        }
        if let Some(rest) = tok.strip_prefix('-') {
            // Negation cannot participate in an OR-group.
            if i + 1 < tokens.len() && is_or(tokens[i + 1].as_str()) {
                return Err(Error::BadQuery("dangling 'or' in query".into()));
            }
            let (body, mode) = strip_exact(rest);
            if let Some(sys) = strip_system_prefix(body) {
                if mode == MatchMode::Exact {
                    return Err(Error::BadQuery(
                        "system predicates cannot be marked exact".into(),
                    ));
                }
                predicates.push(Predicate::NotSystem(SystemPredicate::parse(sys)?));
            } else if is_wild(body) {
                predicates.push(Predicate::NotWild(TagPattern::parse(body)?, mode));
            } else {
                predicates.push(Predicate::Not(Tag::parse(body)?, mode));
            }
            i += 1;
            continue;
        }
        // Positive term: strip a single leading `=` first, so `=char:*` routes to
        // the wildcard arm and `=system:` is caught and rejected below.
        let (body, mode) = strip_exact(tok);
        if body.starts_with('-') {
            // `=-tag`: `-` must lead a term; only `-=tag` is valid.
            return Err(Error::BadQuery(
                "'-' must lead a term; '=-tag' is not valid".into(),
            ));
        }
        // A positive system predicate is standalone — it may not join an OR-group.
        if let Some(sys) = strip_system_prefix(body) {
            if i + 1 < tokens.len() && is_or(tokens[i + 1].as_str()) {
                return Err(Error::BadQuery(
                    "system predicates cannot appear in an 'or' group".into(),
                ));
            }
            if mode == MatchMode::Exact {
                return Err(Error::BadQuery(
                    "system predicates cannot be marked exact".into(),
                ));
            }
            predicates.push(Predicate::System(SystemPredicate::parse(sys)?));
            i += 1;
            continue;
        }
        // A positive wildcard is standalone — it may not start or join an OR-group.
        if is_wild(body) {
            if i + 1 < tokens.len() && is_or(tokens[i + 1].as_str()) {
                return Err(Error::BadQuery(
                    "wildcards cannot appear in an 'or' group".into(),
                ));
            }
            predicates.push(Predicate::Wild(TagPattern::parse(body)?, mode));
            i += 1;
            continue;
        }
        // Positive exact/expanded tag, possibly the head of an `a or b or c` group.
        let mut group = vec![(Tag::parse(body)?, mode)];
        while i + 1 < tokens.len() && is_or(tokens[i + 1].as_str()) {
            let next = tokens
                .get(i + 2)
                .ok_or_else(|| Error::BadQuery("dangling 'or' in query".into()))?;
            if is_or(next.as_str()) || next.starts_with('-') {
                return Err(Error::BadQuery("dangling 'or' in query".into()));
            }
            let (nbody, nmode) = strip_exact(next.as_str());
            if nbody.starts_with('-') {
                return Err(Error::BadQuery(
                    "'-' must lead a term; '=-tag' is not valid".into(),
                ));
            }
            if strip_system_prefix(nbody).is_some() {
                return Err(Error::BadQuery(
                    "system predicates cannot appear in an 'or' group".into(),
                ));
            }
            if is_wild(nbody) {
                return Err(Error::BadQuery(
                    "wildcards cannot appear in an 'or' group".into(),
                ));
            }
            group.push((Tag::parse(nbody)?, nmode));
            i += 2;
        }
        if group.len() == 1 {
            let (t, m) = group.remove(0);
            predicates.push(Predicate::Tag(t, m));
        } else {
            predicates.push(Predicate::Or(group));
        }
        i += 1;
    }
    Ok(Query { predicates })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokenize a query string the same way the daemon does (quote-aware).
    fn toks(s: &str) -> Vec<String> {
        tokenize(s)
    }

    #[test]
    fn query_holds_predicates() {
        let q = Query {
            predicates: vec![
                Predicate::Tag(Tag::parse("character:samus").unwrap(), MatchMode::Expanded),
                Predicate::Not(Tag::parse("meta:wip").unwrap(), MatchMode::Expanded),
                Predicate::Or(vec![
                    (Tag::parse("series:metroid").unwrap(), MatchMode::Expanded),
                    (
                        Tag::parse("series:zero mission").unwrap(),
                        MatchMode::Expanded,
                    ),
                ]),
            ],
        };
        assert_eq!(q.predicates.len(), 3);
        assert_eq!(
            q.predicates[0],
            Predicate::Tag(Tag::parse("character:samus").unwrap(), MatchMode::Expanded)
        );
    }

    #[test]
    fn default_query_is_empty() {
        assert!(Query::default().predicates.is_empty());
    }

    #[test]
    fn parses_namespace_any() {
        assert_eq!(
            TagPattern::parse("character:*").unwrap(),
            TagPattern::NamespaceAny {
                namespace: "character".into()
            }
        );
    }

    #[test]
    fn parses_namespace_glob() {
        assert_eq!(
            TagPattern::parse("Character:Samus*").unwrap(),
            TagPattern::NamespaceGlob {
                namespace: "character".into(),
                glob: "samus*".into()
            }
        );
    }

    #[test]
    fn parses_any_namespace_glob() {
        assert_eq!(
            TagPattern::parse("samus*").unwrap(),
            TagPattern::AnyNamespaceGlob {
                glob: "samus*".into()
            }
        );
    }

    #[test]
    fn parses_wildcards_anywhere_in_subtag() {
        // Leading, interior, and surrounding wildcards all parse now.
        assert_eq!(
            TagPattern::parse("*samus").unwrap(),
            TagPattern::AnyNamespaceGlob {
                glob: "*samus".into()
            }
        );
        assert_eq!(
            TagPattern::parse("sam*us").unwrap(),
            TagPattern::AnyNamespaceGlob {
                glob: "sam*us".into()
            }
        );
        assert_eq!(
            TagPattern::parse("character:*aran").unwrap(),
            TagPattern::NamespaceGlob {
                namespace: "character".into(),
                glob: "*aran".into()
            }
        );
        assert_eq!(
            TagPattern::parse("*samus*").unwrap(),
            TagPattern::AnyNamespaceGlob {
                glob: "*samus*".into()
            }
        );
    }

    #[test]
    fn normalizes_pattern_like_a_tag() {
        // Outer whitespace trimmed, interior runs collapsed, lowercased — but a
        // space adjacent to `*` is a literal space in the glob (it is not eaten).
        assert_eq!(
            TagPattern::parse("  Series :  Zero  Mission* ").unwrap(),
            TagPattern::NamespaceGlob {
                namespace: "series".into(),
                glob: "zero mission*".into()
            }
        );
        assert_eq!(
            TagPattern::parse("zero *").unwrap(),
            TagPattern::AnyNamespaceGlob {
                glob: "zero *".into()
            }
        );
    }

    #[test]
    fn rejects_unsupported_patterns() {
        for bad in [
            "*",       // bare wildcard: matches everything
            "**",      // all-stars: still matches everything
            "char*:x", // wildcard in the namespace
            ":foo*",   // empty namespace before ':'
            "no-star", // no wildcard at all
        ] {
            assert!(
                TagPattern::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn tag_pattern_display_is_canonical() {
        assert_eq!(
            TagPattern::parse("  Meme : Bad* ").unwrap().to_string(),
            "meme:bad*"
        );
        assert_eq!(
            TagPattern::parse("Character:*").unwrap().to_string(),
            "character:*"
        );
        assert_eq!(TagPattern::parse("Samus*").unwrap().to_string(), "samus*");
        assert_eq!(TagPattern::parse("*Samus").unwrap().to_string(), "*samus");
        assert_eq!(TagPattern::parse("Sam*us").unwrap().to_string(), "sam*us");
    }

    #[test]
    fn parses_size_with_units() {
        assert_eq!(
            SystemPredicate::parse("size>=5mb").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Size,
                op: CmpOp::Ge,
                value: 5 * 1024 * 1024,
            }
        );
        // No unit ⇒ bytes.
        assert_eq!(
            SystemPredicate::parse("size>1024").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Size,
                op: CmpOp::Gt,
                value: 1024,
            }
        );
        // kb/gb, case-insensitive.
        assert_eq!(
            SystemPredicate::parse("SIZE<2KB").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Size,
                op: CmpOp::Lt,
                value: 2048,
            }
        );
        assert_eq!(
            SystemPredicate::parse("size<=1gb").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Size,
                op: CmpOp::Le,
                value: 1024 * 1024 * 1024,
            }
        );
    }

    #[test]
    fn parses_duration_with_units() {
        assert_eq!(
            SystemPredicate::parse("duration>30s").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Duration,
                op: CmpOp::Gt,
                value: 30_000,
            }
        );
        assert_eq!(
            SystemPredicate::parse("duration<500ms").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Duration,
                op: CmpOp::Lt,
                value: 500,
            }
        );
        assert_eq!(
            SystemPredicate::parse("duration>=2m").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Duration,
                op: CmpOp::Ge,
                value: 120_000,
            }
        );
        assert_eq!(
            SystemPredicate::parse("duration=1h").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Duration,
                op: CmpOp::Eq,
                value: 3_600_000,
            }
        );
    }

    #[test]
    fn parses_dimensions_bare_pixels() {
        assert_eq!(
            SystemPredicate::parse("width>1920").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Width,
                op: CmpOp::Gt,
                value: 1920,
            }
        );
        assert_eq!(
            SystemPredicate::parse("height<=1080").unwrap(),
            SystemPredicate::Compare {
                field: SysField::Height,
                op: CmpOp::Le,
                value: 1080,
            }
        );
    }

    #[test]
    fn parses_filetype_lowercased() {
        assert_eq!(
            SystemPredicate::parse("type=image/PNG").unwrap(),
            SystemPredicate::FileType {
                mime: "image/png".into()
            }
        );
    }

    #[test]
    fn rejects_malformed_system_predicates() {
        for bad in [
            "bogus>1",    // unknown field
            "type>x",     // type with a non-`=` op
            "width>10px", // unit on a dimension
            "size>5xb",   // bad unit
            "size>5.5mb", // non-integer magnitude
            "width>-1",   // negative magnitude
            "size>",      // missing magnitude
            "type=",      // empty mime
            "size",       // no operator
            "duration5s", // no operator
        ] {
            assert!(
                SystemPredicate::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    // --- parse_query (token stream -> Query) -------------------------------

    #[test]
    fn parses_single_tag() {
        let q = parse_query(&toks("character:samus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Tag(
                Tag::parse("character:samus").unwrap(),
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parses_and_of_several() {
        let q = parse_query(&toks("a b c")).unwrap();
        assert_eq!(q.predicates.len(), 3);
        assert!(
            q.predicates
                .iter()
                .all(|p| matches!(p, Predicate::Tag(_, _)))
        );
    }

    #[test]
    fn parses_negation() {
        let q = parse_query(&toks("a -b")).unwrap();
        assert_eq!(
            q.predicates,
            vec![
                Predicate::Tag(Tag::parse("a").unwrap(), MatchMode::Expanded),
                Predicate::Not(Tag::parse("b").unwrap(), MatchMode::Expanded),
            ]
        );
    }

    #[test]
    fn parses_or_group() {
        let q = parse_query(&toks("a or b or c")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Or(vec![
                (Tag::parse("a").unwrap(), MatchMode::Expanded),
                (Tag::parse("b").unwrap(), MatchMode::Expanded),
                (Tag::parse("c").unwrap(), MatchMode::Expanded),
            ])]
        );
    }

    #[test]
    fn or_is_case_insensitive_and_combines_with_and() {
        let q = parse_query(&toks("a OR b c")).unwrap();
        assert_eq!(
            q.predicates,
            vec![
                Predicate::Or(vec![
                    (Tag::parse("a").unwrap(), MatchMode::Expanded),
                    (Tag::parse("b").unwrap(), MatchMode::Expanded),
                ]),
                Predicate::Tag(Tag::parse("c").unwrap(), MatchMode::Expanded),
            ]
        );
    }

    #[test]
    fn or_group_followed_by_negation() {
        let q = parse_query(&toks("a or b -c")).unwrap();
        assert_eq!(
            q.predicates,
            vec![
                Predicate::Or(vec![
                    (Tag::parse("a").unwrap(), MatchMode::Expanded),
                    (Tag::parse("b").unwrap(), MatchMode::Expanded),
                ]),
                Predicate::Not(Tag::parse("c").unwrap(), MatchMode::Expanded),
            ]
        );
    }

    #[test]
    fn two_separate_or_groups() {
        let q = parse_query(&toks("a or b c or d")).unwrap();
        assert_eq!(
            q.predicates,
            vec![
                Predicate::Or(vec![
                    (Tag::parse("a").unwrap(), MatchMode::Expanded),
                    (Tag::parse("b").unwrap(), MatchMode::Expanded),
                ]),
                Predicate::Or(vec![
                    (Tag::parse("c").unwrap(), MatchMode::Expanded),
                    (Tag::parse("d").unwrap(), MatchMode::Expanded),
                ]),
            ]
        );
    }

    #[test]
    fn empty_tokens_is_error() {
        assert!(parse_query(&[]).is_err());
    }

    #[test]
    fn dangling_or_is_error() {
        assert!(parse_query(&toks("or a")).is_err());
        assert!(parse_query(&toks("a or")).is_err());
        assert!(parse_query(&toks("-a or b")).is_err());
    }

    #[test]
    fn empty_subtag_token_is_error() {
        assert!(parse_query(&toks("-")).is_err());
    }

    #[test]
    fn parse_query_namespace_wildcard() {
        let q = parse_query(&toks("character:*")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Wild(
                TagPattern::NamespaceAny {
                    namespace: "character".into()
                },
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parse_query_prefix_wildcard() {
        let q = parse_query(&toks("samus*")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Wild(
                TagPattern::AnyNamespaceGlob {
                    glob: "samus*".into()
                },
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parse_query_leading_wildcard() {
        let q = parse_query(&toks("*samus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Wild(
                TagPattern::AnyNamespaceGlob {
                    glob: "*samus".into()
                },
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parse_query_negated_wildcard() {
        let q = parse_query(&toks("-character:*")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::NotWild(
                TagPattern::NamespaceAny {
                    namespace: "character".into()
                },
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parses_mixed_wildcard_exact_negation() {
        let q = parse_query(&toks("character:* series:metroid -meta:wip")).unwrap();
        assert_eq!(q.predicates.len(), 3);
        assert!(matches!(q.predicates[0], Predicate::Wild(_, _)));
        assert!(matches!(q.predicates[1], Predicate::Tag(_, _)));
        assert!(matches!(q.predicates[2], Predicate::Not(_, _)));
    }

    #[test]
    fn wildcard_in_or_group_is_error() {
        assert!(parse_query(&toks("a or character:*")).is_err());
        assert!(parse_query(&toks("character:* or a")).is_err());
    }

    #[test]
    fn malformed_wildcard_is_error() {
        assert!(parse_query(&toks("char*cter:samus")).is_err()); // `*` in namespace
        assert!(parse_query(&toks("*")).is_err()); // bare wildcard
    }

    #[test]
    fn parses_system_predicate() {
        let q = parse_query(&toks("system:size>=5mb")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::System(
                SystemPredicate::parse("size>=5mb").unwrap()
            )]
        );
    }

    #[test]
    fn parses_negated_system_predicate() {
        let q = parse_query(&toks("-system:duration<10s")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::NotSystem(
                SystemPredicate::parse("duration<10s").unwrap()
            )]
        );
    }

    #[test]
    fn system_prefix_is_case_insensitive() {
        let q = parse_query(&toks("SYSTEM:type=image/png")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::System(
                SystemPredicate::parse("type=image/png").unwrap()
            )]
        );
    }

    #[test]
    fn multibyte_tag_does_not_panic_on_system_prefix_check() {
        // `もぐらもぐら` is 18 bytes; the `system:` prefix probe (`tok[..7]`) would
        // land mid-character and panic on a byte slice. It must parse as a plain
        // tag instead.
        let q = parse_query(&toks("もぐらもぐら")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Tag(
                Tag::parse("もぐらもぐら").unwrap(),
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parses_mixed_system_tag_wildcard() {
        let q = parse_query(&toks("character:* system:size>1mb -meta:wip")).unwrap();
        assert_eq!(q.predicates.len(), 3);
        assert!(matches!(q.predicates[0], Predicate::Wild(_, _)));
        assert!(matches!(q.predicates[1], Predicate::System(_)));
        assert!(matches!(q.predicates[2], Predicate::Not(_, _)));
    }

    #[test]
    fn system_in_or_group_is_error() {
        assert!(parse_query(&toks("a or system:size>1mb")).is_err());
        assert!(parse_query(&toks("system:size>1mb or a")).is_err());
    }

    #[test]
    fn malformed_system_predicate_is_error() {
        assert!(parse_query(&toks("system:bogus>1")).is_err());
        assert!(parse_query(&toks("system:size>5.5mb")).is_err());
    }

    // --- per-term `=tag` exact operator (#9) -------------------------------

    #[test]
    fn parses_exact_tag() {
        let q = parse_query(&toks("=character:samus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Tag(
                Tag::parse("character:samus").unwrap(),
                MatchMode::Exact
            )]
        );
    }

    #[test]
    fn bare_tag_is_expanded_by_default() {
        let q = parse_query(&toks("character:samus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Tag(
                Tag::parse("character:samus").unwrap(),
                MatchMode::Expanded
            )]
        );
    }

    #[test]
    fn parses_negated_exact_tag() {
        let q = parse_query(&toks("-=character:samus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Not(
                Tag::parse("character:samus").unwrap(),
                MatchMode::Exact
            )]
        );
    }

    #[test]
    fn exact_is_per_member_in_or_group() {
        let q = parse_query(&toks("=a or b")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Or(vec![
                (Tag::parse("a").unwrap(), MatchMode::Exact),
                (Tag::parse("b").unwrap(), MatchMode::Expanded),
            ])]
        );
        let q2 = parse_query(&toks("=a or =b")).unwrap();
        assert_eq!(
            q2.predicates,
            vec![Predicate::Or(vec![
                (Tag::parse("a").unwrap(), MatchMode::Exact),
                (Tag::parse("b").unwrap(), MatchMode::Exact),
            ])]
        );
    }

    #[test]
    fn parses_exact_wildcard() {
        let q = parse_query(&toks("=character:*")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Wild(
                TagPattern::NamespaceAny {
                    namespace: "character".into()
                },
                MatchMode::Exact
            )]
        );
    }

    #[test]
    fn exact_system_predicate_is_error() {
        assert!(parse_query(&toks("=system:size>1mb")).is_err());
        assert!(parse_query(&toks("-=system:duration<10s")).is_err());
    }

    #[test]
    fn equals_dash_ordering_is_error() {
        assert!(parse_query(&toks("=-character:samus")).is_err());
        assert!(parse_query(&toks("=a or =-b")).is_err());
    }

    #[test]
    fn tokenize_splits_on_unquoted_whitespace() {
        assert_eq!(tokenize("a b  c"), vec!["a", "b", "c"]);
        assert_eq!(tokenize("   "), Vec::<String>::new());
    }

    #[test]
    fn tokenize_keeps_quoted_phrases_whole() {
        assert_eq!(tokenize("\"zero mission\""), vec!["zero mission"]);
        assert_eq!(
            tokenize("character:\"zero mission\""),
            vec!["character:zero mission"]
        );
        assert_eq!(
            tokenize("\"zero mission\" -\"prime 3\""),
            vec!["zero mission", "-prime 3"]
        );
    }

    #[test]
    fn tokenize_preserves_operators_inside_quotes() {
        // `*` and `:` survive a phrase, so quoted multi-word wildcards still work.
        assert_eq!(tokenize("\"zero mission*\""), vec!["zero mission*"]);
        assert_eq!(
            tokenize("an unterminated\"quote"),
            vec!["an", "unterminatedquote"]
        );
    }

    #[test]
    fn quoted_multiword_tag_parses_as_one_predicate() {
        let q = parse_query(&toks("character:\"zero mission\"")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::Tag(
                Tag::parse("character:zero mission").unwrap(),
                MatchMode::Expanded
            )]
        );
    }

    // --- system:origin=<name> predicates (#165) ----------------------------

    #[test]
    fn parses_origin_named() {
        // Positive named origin.
        let q = parse_query(&toks("system:origin=hydrus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::System(SystemPredicate::Origin {
                name: Some("hydrus".into())
            })]
        );
        // Negated named origin.
        let q2 = parse_query(&toks("-system:origin=hydrus")).unwrap();
        assert_eq!(
            q2.predicates,
            vec![Predicate::NotSystem(SystemPredicate::Origin {
                name: Some("hydrus".into())
            })]
        );
    }

    #[test]
    fn parses_origin_manual_reserved() {
        // `manual` (case-insensitive) → None.
        assert_eq!(
            SystemPredicate::parse("origin=manual").unwrap(),
            SystemPredicate::Origin { name: None }
        );
        assert_eq!(
            SystemPredicate::parse("origin=MANUAL").unwrap(),
            SystemPredicate::Origin { name: None }
        );
        assert_eq!(
            SystemPredicate::parse("origin=Manual").unwrap(),
            SystemPredicate::Origin { name: None }
        );
    }

    #[test]
    fn parses_origin_case_insensitive_prefix() {
        // The `system:` prefix is already case-insensitive via existing machinery;
        // the body is lowercased, so `ORIGIN=Hydrus` parses with lowercased value.
        let q = parse_query(&toks("SYSTEM:ORIGIN=Hydrus")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::System(SystemPredicate::Origin {
                name: Some("hydrus".into())
            })]
        );
    }

    #[test]
    fn parses_origin_quoted_spaces() {
        // `system:origin="wd14 tagger"` arrives as one token via tokenize.
        let q = parse_query(&tokenize("system:origin=\"wd14 tagger\"")).unwrap();
        assert_eq!(
            q.predicates,
            vec![Predicate::System(SystemPredicate::Origin {
                name: Some("wd14 tagger".into())
            })]
        );
    }

    #[test]
    fn parses_origin_value_containing_equals() {
        // Split at first `=` only: `origin=a=b` → name `a=b`.
        assert_eq!(
            SystemPredicate::parse("origin=a=b").unwrap(),
            SystemPredicate::Origin {
                name: Some("a=b".into())
            }
        );
    }

    #[test]
    fn rejects_origin_non_eq_operators() {
        // Only `=` is legal; any other operator → BadSystem.
        assert!(SystemPredicate::parse("origin>foo").is_err());
        assert!(SystemPredicate::parse("origin<foo").is_err());
        assert!(SystemPredicate::parse("origin>=foo").is_err());
        assert!(SystemPredicate::parse("origin").is_err()); // no op
        assert!(SystemPredicate::parse("origin=").is_err()); // empty value
    }

    #[test]
    fn rejects_origin_in_or_group() {
        assert!(parse_query(&toks("a or system:origin=x")).is_err());
        assert!(parse_query(&toks("system:origin=x or a")).is_err());
    }

    #[test]
    fn rejects_origin_with_exact_marker() {
        assert!(parse_query(&toks("=system:origin=x")).is_err());
        assert!(parse_query(&toks("-=system:origin=x")).is_err());
    }
}
