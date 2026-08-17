//! Property-based test (README §10): `Db::search` agrees with a naive
//! reference evaluator for arbitrary small tag universes and queries.
//! Raw expansion + Exact match keeps the semantics purely set-theoretic
//! (no relation graph, no wildcard SQL) so the reference is trivially right.

use naiad_core::{FileRecord, Hash, MatchMode, Predicate, Query, Tag, hash_bytes};
use naiad_db::{BlockKind, Db, Expansion, ReadScope};
use proptest::prelude::*;

const TAGS: usize = 6;
const MAX_FILES: usize = 8;

fn tag(i: usize) -> Tag {
    Tag::parse(&format!("t{i}")).unwrap()
}

/// A generatable predicate over the fixed tag universe.
#[derive(Clone, Debug)]
enum PSpec {
    Has(usize),
    Not(usize),
    Or(usize, usize),
}

fn arb_pspec() -> impl Strategy<Value = PSpec> {
    prop_oneof![
        (0..TAGS).prop_map(PSpec::Has),
        (0..TAGS).prop_map(PSpec::Not),
        (0..TAGS, 0..TAGS).prop_map(|(a, b)| PSpec::Or(a, b)),
    ]
}

impl PSpec {
    fn to_predicate(&self) -> Predicate {
        match *self {
            PSpec::Has(i) => Predicate::Tag(tag(i), MatchMode::Exact),
            PSpec::Not(i) => Predicate::Not(tag(i), MatchMode::Exact),
            PSpec::Or(a, b) => {
                Predicate::Or(vec![(tag(a), MatchMode::Exact), (tag(b), MatchMode::Exact)])
            }
        }
    }

    /// Naive reference semantics over one file's tag membership.
    fn matches(&self, tags_of_file: &[bool; TAGS]) -> bool {
        match *self {
            PSpec::Has(i) => tags_of_file[i],
            PSpec::Not(i) => !tags_of_file[i],
            PSpec::Or(a, b) => tags_of_file[a] || tags_of_file[b],
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn search_agrees_with_naive_reference(
        files in prop::collection::vec(prop::array::uniform6(any::<bool>()), 1..=MAX_FILES),
        specs in prop::collection::vec(arb_pspec(), 1..=3),
    ) {
        let db = Db::open_in_memory().unwrap();
        let svc = db.local_service_id().unwrap();
        let tag_ids: Vec<i64> = (0..TAGS).map(|i| db.intern_tag(&tag(i)).unwrap()).collect();

        // Real files in a real (in-memory) SQLite db, one hash per index.
        let mut hashes = Vec::new();
        for (f, membership) in files.iter().enumerate() {
            let h = hash_bytes(format!("qp-file-{f}").as_bytes());
            hashes.push(h);
            let path = format!("/qp/{f}.bin");
            db.insert_file(&FileRecord::new(h, path.clone().into(), 1, None), 1).unwrap();
            let fid = db
                .file_id_by_path(std::path::Path::new(&path))
                .unwrap()
                .unwrap();
            for (t, has) in membership.iter().enumerate() {
                if *has {
                    db.add_mapping(fid, tag_ids[t], svc).unwrap();
                }
            }
        }

        let query = Query { predicates: specs.iter().map(PSpec::to_predicate).collect() };

        // Reference: a file matches iff every predicate holds (implicit AND).
        let mut expected: Vec<String> = files
            .iter()
            .enumerate()
            .filter(|(_, m)| specs.iter().all(|s| s.matches(m)))
            .map(|(f, _)| hashes[f].to_hex())
            .collect();
        expected.sort();

        let mut got: Vec<String> = db
            .search(&query, ReadScope::LocalOnly, Expansion::Raw)
            .unwrap()
            .into_iter()
            .map(|listing| listing.hash.to_hex())
            .collect();
        got.sort();

        prop_assert_eq!(got, expected);
    }
}

// ─── display-surface agreement tests (spec §5 / Task 10) ────────────────────

/// The three display surfaces (`display_tags_of`, `display_tags_detailed`,
/// `search` with `Expansion::Expanded`) must agree on visibility for a pulled
/// tag, respecting rejection and block rules.
///
/// Sequence: visible → rejected (hidden in all three) → rejection removed
/// (visible again in all three). Then: block rule added (hidden) → removed
/// (visible).
#[test]
fn display_surfaces_agree_on_visibility() {
    let db = Db::open_in_memory().unwrap();
    let _local_svc = db.local_service_id().unwrap();
    let shared_svc = db
        .add_shared_service("testpeer", "http://peer/", None)
        .unwrap();

    let t = Tag::parse("art:drawing").unwrap();
    let tag_id = db.intern_tag(&t).unwrap();

    let h = hash_bytes(b"surface-agreement-file");
    db.insert_file(&FileRecord::new(h, "/sa/f.bin".into(), 1, None), 1)
        .unwrap();
    let file_id = db
        .file_id_by_path(std::path::Path::new("/sa/f.bin"))
        .unwrap()
        .unwrap();
    db.add_mapping(file_id, tag_id, shared_svc).unwrap();

    let scope = ReadScope::Merged;
    let q = Query {
        predicates: vec![Predicate::Tag(t.clone(), MatchMode::Exact)],
    };

    // helper: is tag visible in each surface?
    let visible = |db: &Db| -> (bool, bool, bool) {
        let of = db.display_tags_of(file_id, scope).unwrap();
        let det = db.display_tags_detailed(file_id, scope).unwrap();
        let srch = db.search(&q, scope, Expansion::Expanded).unwrap();
        (
            of.iter().any(|x| x.tag == t),
            det.iter().any(|x| x.tag == t),
            srch.iter().any(|l| l.hash == h),
        )
    };

    // Initially visible in all three surfaces.
    assert_eq!(
        visible(&db),
        (true, true, true),
        "tag must be visible before any rejection"
    );

    // Reject the mapping — all three must hide it.
    db.add_rejection(shared_svc, file_id, tag_id, None).unwrap();
    assert_eq!(
        visible(&db),
        (false, false, false),
        "tag must be hidden after rejection"
    );

    // Undo the rejection — all three must show it again.
    db.remove_rejection(shared_svc, file_id, tag_id).unwrap();
    assert_eq!(
        visible(&db),
        (true, true, true),
        "tag must reappear after rejection removed"
    );

    // Block rule on the exact tag — all three must hide it.
    let rule_id = db
        .add_block_rule(BlockKind::Tag, "art:drawing", None)
        .unwrap();
    assert_eq!(
        visible(&db),
        (false, false, false),
        "tag must be hidden by block rule"
    );

    // Remove the block rule — all three must show it again.
    db.remove_block_rule(rule_id).unwrap();
    assert_eq!(
        visible(&db),
        (true, true, true),
        "tag must reappear after block rule removed"
    );
}

/// Local-service tags are never hidden by rejections or block rules (local-exempt
/// rule, ADR 0006). All three surfaces must show local tags regardless.
#[test]
fn local_tags_never_hidden_by_predicate() {
    let db = Db::open_in_memory().unwrap();
    let local_svc = db.local_service_id().unwrap();
    let shared_svc = db.add_shared_service("peer", "http://peer/", None).unwrap();

    let t = Tag::parse("local:tag").unwrap();
    let tag_id = db.intern_tag(&t).unwrap();

    let h = hash_bytes(b"local-exempt-file");
    db.insert_file(&FileRecord::new(h, "/le/f.bin".into(), 1, None), 1)
        .unwrap();
    let file_id = db
        .file_id_by_path(std::path::Path::new("/le/f.bin"))
        .unwrap()
        .unwrap();
    db.add_mapping(file_id, tag_id, local_svc).unwrap();

    let scope = ReadScope::Merged;
    let q = Query {
        predicates: vec![Predicate::Tag(t.clone(), MatchMode::Exact)],
    };

    let visible = |db: &Db| -> (bool, bool, bool) {
        let of = db.display_tags_of(file_id, scope).unwrap();
        let det = db.display_tags_detailed(file_id, scope).unwrap();
        let srch = db.search(&q, scope, Expansion::Expanded).unwrap();
        (
            of.iter().any(|x| x.tag == t),
            det.iter().any(|x| x.tag == t),
            srch.iter().any(|l| l.hash == h),
        )
    };

    // Add a block rule — local tags must still be visible.
    db.add_block_rule(BlockKind::Tag, "local:tag", None)
        .unwrap();
    assert_eq!(
        visible(&db),
        (true, true, true),
        "local tags must ignore block rules"
    );

    // Add a rejection keyed on the *shared* service — local mapping unaffected.
    db.add_rejection(shared_svc, file_id, tag_id, None).unwrap();
    assert_eq!(
        visible(&db),
        (true, true, true),
        "local tags must ignore rejections on other services"
    );
}

/// Raw-expansion search bypasses rejection filtering (spec §7 raw-path maxim):
/// a rejected mapping still matches in raw mode. Block rules, however, apply in
/// all expansion modes (they are a user preference, not a per-file decision).
#[test]
fn raw_expansion_bypasses_rejection_not_blocks() {
    let db = Db::open_in_memory().unwrap();
    let _local_svc = db.local_service_id().unwrap();
    let shared_svc = db.add_shared_service("peer", "http://peer/", None).unwrap();

    let t = Tag::parse("raw:tag").unwrap();
    let tag_id = db.intern_tag(&t).unwrap();

    let h = hash_bytes(b"raw-bypass-file");
    db.insert_file(&FileRecord::new(h, "/rb/f.bin".into(), 1, None), 1)
        .unwrap();
    let file_id = db
        .file_id_by_path(std::path::Path::new("/rb/f.bin"))
        .unwrap()
        .unwrap();
    db.add_mapping(file_id, tag_id, shared_svc).unwrap();

    let scope = ReadScope::Merged;
    let q = Query {
        predicates: vec![Predicate::Tag(t.clone(), MatchMode::Exact)],
    };

    // Add a rejection — expanded search must hide the file.
    db.add_rejection(shared_svc, file_id, tag_id, None).unwrap();

    let expanded = db.search(&q, scope, Expansion::Expanded).unwrap();
    assert!(
        !expanded.iter().any(|l| l.hash == h),
        "expanded search must hide rejected tag"
    );

    // Raw search: rejection is bypassed, file is visible.
    let raw = db.search(&q, scope, Expansion::Raw).unwrap();
    assert!(
        raw.iter().any(|l| l.hash == h),
        "raw search must bypass rejection filter"
    );

    // Remove rejection; add a block rule — block rules apply in raw mode too.
    db.remove_rejection(shared_svc, file_id, tag_id).unwrap();
    db.add_block_rule(BlockKind::Tag, "raw:tag", None).unwrap();

    let raw_blocked = db.search(&q, scope, Expansion::Raw).unwrap();
    assert!(
        !raw_blocked.iter().any(|l| l.hash == h),
        "raw search must still apply block rules"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// For arbitrary sets of (file, pulled-tag, rejection?) states, the two
    /// display-tag surfaces agree with each other and with `Expansion::Expanded`
    /// search on whether each tag is visible.
    #[test]
    fn display_surfaces_agree_for_arbitrary_state(
        entries in prop::collection::vec(
            (any::<bool>(), any::<bool>()), // (has_tag, is_rejected)
            1..=6,
        ),
    ) {
        let db = Db::open_in_memory().unwrap();
        let _local_svc = db.local_service_id().unwrap();
        let shared_svc = db.add_shared_service("peer", "http://p/", None).unwrap();

        let t = Tag::parse("prop:t").unwrap();
        let tag_id = db.intern_tag(&t).unwrap();
        let scope = ReadScope::Merged;
        let q = Query {
            predicates: vec![Predicate::Tag(t.clone(), MatchMode::Exact)],
        };

        let mut file_hashes: Vec<(Hash, bool)> = Vec::new();

        for (f_idx, (has_tag, is_rejected)) in entries.iter().enumerate() {
            let h = hash_bytes(format!("prop-agree-{f_idx}").as_bytes());
            let path = format!("/pa/{f_idx}.bin");
            db.insert_file(&FileRecord::new(h, path.clone().into(), 1, None), 1)
                .unwrap();
            let fid = db
                .file_id_by_path(std::path::Path::new(&path))
                .unwrap()
                .unwrap();

            if *has_tag {
                db.add_mapping(fid, tag_id, shared_svc).unwrap();
                if *is_rejected {
                    db.add_rejection(shared_svc, fid, tag_id, None).unwrap();
                }
            }

            let expected_visible = *has_tag && !*is_rejected;
            file_hashes.push((h, expected_visible));

            // Check the display-tag surfaces for this specific file.
            let of = db.display_tags_of(fid, scope).unwrap();
            let det = db.display_tags_detailed(fid, scope).unwrap();
            let in_of = of.iter().any(|x| x.tag == t);
            let in_det = det.iter().any(|x| x.tag == t);

            prop_assert_eq!(
                in_of, expected_visible,
                "display_tags_of disagrees for file {} (has_tag={}, is_rejected={})",
                f_idx, has_tag, is_rejected
            );
            prop_assert_eq!(
                in_det, expected_visible,
                "display_tags_detailed disagrees for file {} (has_tag={}, is_rejected={})",
                f_idx, has_tag, is_rejected
            );
            prop_assert_eq!(
                in_of, in_det,
                "display_tags_of and display_tags_detailed diverge for file {}",
                f_idx
            );
        }

        // Check search results agree for every file at the end.
        let search_res = db.search(&q, scope, Expansion::Expanded).unwrap();
        for (h, expected_visible) in &file_hashes {
            let in_search = search_res.iter().any(|l| l.hash == *h);
            prop_assert_eq!(
                in_search,
                *expected_visible,
                "search disagrees with display surfaces for hash {:?}",
                h
            );
        }
    }
}

// ─── end display-surface agreement tests ─────────────────────────────────────

/// Deterministic companion to the property: a pure-`Not` query must return a
/// file with zero mappings (the `all_file_ids()` seed path in `search`).
/// `display_tags_detailed` surfaces `origin: Some("wd14-tagger")` for a pulled
/// tag whose mapping has a populated `origin_id`, and `origin: None` for a local
/// (manual) tag — verifying the projection added in #162 Task 16.
///
/// Extended cases:
/// - **Aliased tag**: the repo pulls the "bad" alias spelling with an origin;
///   after the sibling relation is registered the canonical spelling's display
///   row must surface the origin (raw→canon fold in the scoped SELECT).
/// - **Tombstone isolation**: a row with `status != 'current'` (out-of-scope
///   service) must not supply an origin to the display row; only `current`
///   rows within the read scope contribute.
#[test]
fn display_tags_detailed_resolves_generation_origin() {
    let db = Db::open_in_memory().unwrap();
    // Local service is seeded by open_in_memory.
    let local = db.local_service_id().unwrap();
    let svc = db.add_shared_service("ptr", "http://repo/", None).unwrap();

    let h = hash_bytes(b"origin-display-test-file");
    let marker = db.next_scan_marker().unwrap();
    db.insert_file(&FileRecord::new(h, "/lib/od.txt".into(), 1, None), marker)
        .unwrap();

    // ── Case 1: direct pulled tag with origin vs. manual (None) ──────────────
    // Pull one tag with a tagger origin and one without (manual None).
    db.merge_pulled_mappings_in_domain(
        svc,
        "blake3",
        &[(
            h,
            vec![
                (
                    Tag::parse("creator:botpic").unwrap(),
                    Some("wd14-tagger".to_string()),
                ),
                (Tag::parse("rating:safe").unwrap(), None),
            ],
        )],
    )
    .unwrap();

    let file_id = db.file_id_by_hash(&h).unwrap().unwrap();
    let details = db
        .display_tags_detailed(file_id, ReadScope::Merged)
        .unwrap();

    let botpic = details
        .iter()
        .find(|d| d.tag.to_string() == "creator:botpic")
        .expect("creator:botpic must be in detailed list");
    assert_eq!(
        botpic.origin.as_deref(),
        Some("wd14-tagger"),
        "pulled tag with origin_id must surface the origin name"
    );

    let safe = details
        .iter()
        .find(|d| d.tag.to_string() == "rating:safe")
        .expect("rating:safe must be in detailed list");
    assert_eq!(
        safe.origin, None,
        "pulled tag with NULL origin_id must surface None"
    );

    // ── Case 2: aliased (bad→ideal) pulled tag surfaces origin on canonical ──
    // Register sibling: "bad_alias" → "creator:botpic" (uses the local svc
    // so the relation is merged into the display graph).
    let bad = db.intern_tag(&Tag::parse("bad_alias").unwrap()).unwrap();
    let ideal = db
        .intern_tag(&Tag::parse("creator:botpic").unwrap())
        .unwrap();
    db.add_sibling(bad, ideal, local).unwrap();

    // Pull the file again, this time tagging it with the alias spelling and a
    // distinct origin name. After the sibling relation is resolved the canonical
    // ("creator:botpic") display row must carry this origin.
    let h2 = hash_bytes(b"origin-alias-test-file");
    let marker2 = db.next_scan_marker().unwrap();
    db.insert_file(
        &FileRecord::new(h2, "/lib/od2.txt".into(), 1, None),
        marker2,
    )
    .unwrap();
    db.merge_pulled_mappings_in_domain(
        svc,
        "blake3",
        &[(
            h2,
            vec![(
                Tag::parse("bad_alias").unwrap(),
                Some("alias-tagger".to_string()),
            )],
        )],
    )
    .unwrap();

    let file_id2 = db.file_id_by_hash(&h2).unwrap().unwrap();
    let details2 = db
        .display_tags_detailed(file_id2, ReadScope::Merged)
        .unwrap();

    // The display list must show the canonical spelling, not the alias.
    assert!(
        details2.iter().all(|d| d.tag.to_string() != "bad_alias"),
        "alias spelling must not appear in display list (should be canonicalized)"
    );
    let canonical = details2
        .iter()
        .find(|d| d.tag.to_string() == "creator:botpic")
        .expect("canonical tag must appear in display list for aliased pull");
    assert_eq!(
        canonical.origin.as_deref(),
        Some("alias-tagger"),
        "aliased pulled tag: origin must fold through raw→canon and surface on canonical"
    );

    // ── Case 3: out-of-scope service row must not supply origin ───────────────
    // Add a second shared service that is NOT in the Merged scope (simulate
    // isolation by using LocalOnly scope, which excludes all shared services).
    let _svc2 = db
        .add_shared_service("out-of-scope", "http://other/", None)
        .unwrap();
    let h3 = hash_bytes(b"origin-scope-test-file");
    let marker3 = db.next_scan_marker().unwrap();
    db.insert_file(
        &FileRecord::new(h3, "/lib/od3.txt".into(), 1, None),
        marker3,
    )
    .unwrap();
    // Add a manual local mapping (no origin).
    let local_tag_id = db.intern_tag(&Tag::parse("rating:safe").unwrap()).unwrap();
    let local_file_id = db.file_id_by_hash(&h3).unwrap().unwrap();
    db.add_mapping(local_file_id, local_tag_id, local).unwrap();
    // The shared service also has this tag with an origin — but in LocalOnly
    // scope the shared service row is out-of-scope and must not contribute origin.
    db.merge_pulled_mappings_in_domain(
        svc,
        "blake3",
        &[(
            h3,
            vec![(
                Tag::parse("rating:safe").unwrap(),
                Some("scope-tagger".to_string()),
            )],
        )],
    )
    .unwrap();

    let details3 = db
        .display_tags_detailed(local_file_id, ReadScope::LocalOnly)
        .unwrap();
    let scoped = details3
        .iter()
        .find(|d| d.tag.to_string() == "rating:safe")
        .expect("rating:safe must appear under LocalOnly scope (local mapping present)");
    assert_eq!(
        scoped.origin, None,
        "LocalOnly scope must not surface origin from out-of-scope shared service row"
    );
}

#[test]
fn search_not_only_returns_untagged_file() {
    let db = Db::open_in_memory().unwrap();
    let _svc = db.local_service_id().unwrap();
    let t = tag(0);
    db.intern_tag(&t).unwrap();
    let h = hash_bytes(b"untagged-sentinel");
    db.insert_file(&FileRecord::new(h, "/not-test/f.bin".into(), 1, None), 1)
        .unwrap();
    // No add_mapping call: the file carries zero tags.
    let query = Query {
        predicates: vec![Predicate::Not(t, MatchMode::Exact)],
    };
    let got = db
        .search(&query, ReadScope::LocalOnly, Expansion::Raw)
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].hash, h);
}
