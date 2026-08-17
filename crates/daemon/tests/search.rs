//! End-to-end: search through the `naiad_cli` library surface, with relations.

use naiad_daemon::{add_parent, add_sibling, add_tags, import_path, search};
use naiad_db::{Expansion, FileListing, ReadScope};
use naiad_test_support::{fixture_dir, temp_db};

/// Sorted file names of a result set, for order-independent assertions.
fn names(results: &[FileListing]) -> Vec<String> {
    let mut v: Vec<String> = results
        .iter()
        .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn toks(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn search_through_cli_surface() {
    let (db, _db_dir) = temp_db();

    let files = fixture_dir(&[("a.png", b"alpha-bytes"), ("b.png", b"beta-bytes")]);
    import_path(&db, files.path(), |_| {}).unwrap();
    let a = files.path().join("a.png");
    let a = a.to_str().unwrap();
    let b = files.path().join("b.png");
    let b = b.to_str().unwrap();

    // a: raw alias `samus`; b: `creator:nintendo`.
    add_tags(&db, a, &["samus".to_string()]).unwrap();
    add_tags(&db, b, &["creator:nintendo".to_string()]).unwrap();
    // samus -> character:samus aran -> series:metroid
    add_sibling(&db, "samus", "character:samus aran").unwrap();
    add_parent(&db, "character:samus aran", "series:metroid").unwrap();

    // Parent search finds the child-tagged file via relations.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["series:metroid"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["a.png"]
    );
    // Alias search matches the same as the canonical.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["samus"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["a.png"]
    );

    // AND of disjoint tags -> nothing.
    assert!(
        search(
            &db,
            &toks(&["series:metroid", "creator:nintendo"]),
            ReadScope::Merged,
            Expansion::Expanded
        )
        .unwrap()
        .is_empty()
    );

    // OR-group -> both files.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["series:metroid", "or", "creator:nintendo"]),
                ReadScope::Merged,
                Expansion::Expanded,
            )
            .unwrap()
        ),
        vec!["a.png", "b.png"]
    );

    // Negation: NOT the metroid file -> b only.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["-series:metroid"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["b.png"]
    );

    // Empty query errors.
    assert!(search(&db, &[], ReadScope::Merged, Expansion::Expanded).is_err());
}

#[test]
fn wildcard_search_through_cli() {
    let (db, _db_dir) = temp_db();
    let files = fixture_dir(&[("a.png", b"alpha-bytes"), ("b.png", b"beta-bytes")]);
    import_path(&db, files.path(), |_| {}).unwrap();
    let a = files.path().join("a.png");
    let a = a.to_str().unwrap();
    let b = files.path().join("b.png");
    let b = b.to_str().unwrap();

    // a: alias `samus` -> character:samus aran ; b: creator:nintendo
    add_tags(&db, a, &["samus".to_string()]).unwrap();
    add_tags(&db, b, &["creator:nintendo".to_string()]).unwrap();
    add_sibling(&db, "samus", "character:samus aran").unwrap();

    // Namespace wildcard, relation-aware -> a only.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["character:*"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["a.png"]
    );
    // Prefix wildcard on the ideal subtag -> a.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["character:samus*"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["a.png"]
    );
    // Negated wildcard -> everything without a character tag -> b.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["-character:*"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["b.png"]
    );
    // Wildcard in an or-group errors.
    assert!(
        search(
            &db,
            &toks(&["a", "or", "character:*"]),
            ReadScope::Merged,
            Expansion::Expanded
        )
        .is_err()
    );
}

#[test]
fn system_predicate_search_through_cli() {
    let (db, _db_dir) = temp_db();
    // "small" = 5 bytes, "a-larger-blob" = 13 bytes.
    let files = fixture_dir(&[("small.png", b"small"), ("big.png", b"a-larger-blob")]);
    import_path(&db, files.path(), |_| {}).unwrap();

    // size > 5 bytes -> only the larger file.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["system:size>5"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["big.png"]
    );
    // size <= 5 -> only the small file.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["system:size<=5"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["small.png"]
    );
    // Negation: NOT (size > 5) -> the small file.
    assert_eq!(
        names(
            &search(
                &db,
                &toks(&["-system:size>5"]),
                ReadScope::Merged,
                Expansion::Expanded
            )
            .unwrap()
        ),
        vec!["small.png"]
    );
    // Malformed system predicate errors.
    assert!(
        search(
            &db,
            &toks(&["system:bogus>1"]),
            ReadScope::Merged,
            Expansion::Expanded
        )
        .is_err()
    );
    // System predicate inside an or-group errors.
    assert!(
        search(
            &db,
            &toks(&["a", "or", "system:size>5"]),
            ReadScope::Merged,
            Expansion::Expanded
        )
        .is_err()
    );
}
