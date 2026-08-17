//! Builds a tiny Hydrus-shaped DB across three files and exercises the plugin.

use std::path::Path;

use naiad_core::Tag;
use naiad_plugin::{
    FileRef, ImportStats, MappingRecord, RelationKind, RelationRecord, Sink, Source, Tagger,
};
use naiad_plugin_hydrus::HydrusPlugin;
use rusqlite::Connection;

#[derive(Default)]
struct VecSink {
    mappings: Vec<MappingRecord>,
    relations: Vec<RelationRecord>,
}
impl Sink for VecSink {
    fn mapping(&mut self, rec: MappingRecord) -> naiad_plugin::Result<()> {
        self.mappings.push(rec);
        Ok(())
    }
    fn relation(&mut self, rec: RelationRecord) -> naiad_plugin::Result<()> {
        self.relations.push(rec);
        Ok(())
    }
}

const SHA_HEX: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";

fn build_fixture(dir: &Path) {
    let master = Connection::open(dir.join("client.master.db")).unwrap();
    master
        .execute_batch(
            "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
             CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
             CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
             CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);",
        )
        .unwrap();
    master
        .execute(
            "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1)",
            [hex::decode(SHA_HEX).unwrap()],
        )
        .unwrap();
    master
        .execute_batch(
            "INSERT INTO namespaces VALUES (1, ''), (2, 'character');
             INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'samus_aran');
             INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3);",
        )
        .unwrap();

    let client = Connection::open(dir.join("client.db")).unwrap();
    client
        .execute_batch(
            "CREATE TABLE current_files_4 (hash_id INTEGER, timestamp_ms INTEGER);
             INSERT INTO current_files_4 VALUES (1, 0);
             CREATE TABLE current_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
             INSERT INTO current_tag_siblings_9 VALUES (3, 2);
             CREATE TABLE current_tag_parents_9 (child_tag_id INTEGER, parent_tag_id INTEGER);
             INSERT INTO current_tag_parents_9 VALUES (2, 1);",
        )
        .unwrap();

    let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
    mappings
        .execute_batch(
            "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
             INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);",
        )
        .unwrap();
}

#[test]
fn tagger_returns_tags_for_owned_file() {
    let dir = tempfile::tempdir().unwrap();
    build_fixture(dir.path());
    let plugin = HydrusPlugin::new(dir.path().to_path_buf(), vec![9]);
    let tags = plugin
        .tags_for(&FileRef {
            blake3: "00".repeat(32),
            sha256: Some(SHA_HEX.to_string()),
        })
        .unwrap();
    let strs: Vec<String> = tags.iter().map(Tag::to_string).collect();
    assert!(strs.contains(&"maid".to_string()));
    assert!(strs.contains(&"character:samus".to_string()));
}

#[test]
fn bulk_import_streams_relations_and_owned_mappings() {
    let dir = tempfile::tempdir().unwrap();
    build_fixture(dir.path());
    let plugin = HydrusPlugin::new(dir.path().to_path_buf(), vec![9]);
    let mut sink = VecSink::default();
    let stats: ImportStats = plugin.bulk_import(&mut sink).unwrap();
    assert_eq!(stats.siblings, 1);
    assert_eq!(stats.parents, 1);
    assert_eq!(stats.mappings, 2);
    assert!(
        sink.relations
            .iter()
            .any(|r| r.kind == RelationKind::Sibling
                && r.from.to_string() == "character:samus_aran"
                && r.to.to_string() == "character:samus")
    );
    assert!(sink.mappings.iter().all(|m| m.sha256 == SHA_HEX));
}

#[test]
fn reader_returns_owned_file_tags_and_nothing_for_unknown() {
    let dir = tempfile::tempdir().unwrap();
    build_fixture(dir.path());
    let plugin = HydrusPlugin::new(dir.path().to_path_buf(), vec![9]);
    let reader = plugin.reader().unwrap();

    // The owned file resolves its two tags...
    let tags = reader.tags_for_sha(SHA_HEX).unwrap();
    let strs: Vec<String> = tags.iter().map(Tag::to_string).collect();
    // Sorted by (namespace, subtag): the empty namespace ("maid") sorts first.
    assert_eq!(
        strs,
        vec!["maid".to_string(), "character:samus".to_string()]
    );

    // ...and an unrelated sha contributes nothing (no relations involved).
    assert!(reader.tags_for_sha(&"aa".repeat(32)).unwrap().is_empty());
}
