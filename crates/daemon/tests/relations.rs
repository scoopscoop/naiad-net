//! End-to-end: relations applied through the `naiad_cli` library surface that
//! the `naiad` binary wraps.

use naiad_daemon::{
    add_parent, add_sibling, add_tags, display_tags, import_path, list_parents, list_siblings,
    list_tags, remove_parent, remove_sibling,
};
use naiad_db::{Db, ReadScope};
use naiad_test_support::{fixture_dir, temp_db};

/// A file's display tags as plain strings.
fn display(db: &Db, reference: &str) -> Vec<String> {
    display_tags(db, reference, ReadScope::Merged).unwrap()
}

#[test]
fn relations_apply_through_cli_surface() {
    let (db, _db_dir) = temp_db();

    // Fixture files in a separate dir so import_path doesn't touch the db file.
    let files = fixture_dir(&[("samus.png", b"fake-image-bytes")]);
    import_path(&db, files.path(), |_| {}).unwrap();
    let path = files.path().join("samus.png");
    let path = path.to_str().unwrap();

    // Raw-tag with the bad alias only.
    add_tags(&db, path, &["samus".to_string()]).unwrap();

    // Define relations: samus -> character:samus aran -> series:metroid.
    add_sibling(&db, "samus", "character:samus aran").unwrap();
    add_parent(&db, "character:samus aran", "series:metroid").unwrap();

    // Default list is the computed set; --raw is the literal mapping.
    assert_eq!(
        display(&db, path),
        vec!["character:samus aran", "series:metroid"]
    );
    let raw: Vec<String> = list_tags(&db, path)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(raw, vec!["samus"]);

    // Listing relations: both the sibling and the parent are defined here.
    let sibs = list_siblings(&db).unwrap();
    assert_eq!(sibs.len(), 1);
    assert_eq!(sibs[0].0.to_string(), "samus");
    assert_eq!(sibs[0].1.to_string(), "character:samus aran");

    let pars = list_parents(&db).unwrap();
    assert_eq!(pars.len(), 1);
    assert_eq!(pars[0].0.to_string(), "character:samus aran");
    assert_eq!(pars[0].1.to_string(), "series:metroid");

    // Removing the parent drops the implication; sibling still canonicalizes.
    remove_parent(&db, "character:samus aran", "series:metroid").unwrap();
    assert_eq!(display(&db, path), vec!["character:samus aran"]);

    // Removing the sibling falls back to the raw tag.
    remove_sibling(&db, "samus").unwrap();
    assert_eq!(display(&db, path), vec!["samus"]);
}
