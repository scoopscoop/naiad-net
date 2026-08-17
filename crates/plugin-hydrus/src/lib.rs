//! `naiad-plugin-hydrus` â€” import tags and tag relations from a Hydrus database.
//!
//! Implements `Tagger` (per-file lookup) and `Source` (bulk import of the
//! tag-relation graph plus mappings for owned files). Reads read-only; never
//! modifies the Hydrus DB.
//!
//! The one writer in the crate is `fixture`, which builds *synthetic*
//! snapshots for tests and is behind the off-by-default `fixture` feature, so
//! it is absent from any normal build.

#[cfg(feature = "fixture")]
pub mod fixture;
pub mod schema;

use std::collections::HashMap;
use std::path::PathBuf;

use naiad_core::Tag;
use naiad_plugin::{
    Capabilities, FileRef, ImportStats, MappingRecord, Plugin, PluginError, RecordStatus,
    RelationKind, RelationRecord, Result, Sink, Source, Tagger,
};

pub use schema::HydrusDb;

use schema::DEFAULT_FILE_SERVICE;

/// The Hydrus importer plugin.
pub struct HydrusPlugin {
    dir: PathBuf,
    tag_services: Vec<i64>,
    file_service: i64,
}

impl HydrusPlugin {
    /// Bind the plugin to a Hydrus DB directory. `tag_services` empty = all.
    #[must_use]
    pub fn new(dir: PathBuf, tag_services: Vec<i64>) -> Self {
        Self {
            dir,
            tag_services,
            file_service: DEFAULT_FILE_SERVICE,
        }
    }

    fn services(&self, db: &HydrusDb) -> Result<Vec<i64>> {
        if self.tag_services.is_empty() {
            db.tag_service_ids()
        } else {
            Ok(self.tag_services.clone())
        }
    }
}

impl Plugin for HydrusPlugin {
    fn id(&self) -> &str {
        "hydrus"
    }

    fn name(&self) -> &str {
        "Hydrus importer"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tagger: true,
            processor: false,
            source: true,
        }
    }
}

impl Tagger for HydrusPlugin {
    fn tags_for(&self, file: &FileRef) -> Result<Vec<Tag>> {
        let Some(sha) = &file.sha256 else {
            return Err(PluginError("file has no sha256 yet".into()));
        };
        let db = HydrusDb::open(&self.dir)?;
        let mut out = Vec::new();
        for svc in self.services(&db)? {
            out.extend(db.tags_for_sha256(sha, svc)?);
        }
        out.sort_by(|a, b| {
            (a.namespace.as_str(), a.subtag.as_str())
                .cmp(&(b.namespace.as_str(), b.subtag.as_str()))
        });
        out.dedup();
        Ok(out)
    }
}

impl Source for HydrusPlugin {
    fn bulk_import(&self, sink: &mut dyn Sink) -> Result<ImportStats> {
        let db = HydrusDb::open(&self.dir)?;
        let mut stats = ImportStats::default();
        for svc in self.services(&db)? {
            stats.siblings += import_relations(&db, svc, RelationKind::Sibling, sink)?;
            stats.parents += import_relations(&db, svc, RelationKind::Parent, sink)?;
            stats.mappings += import_mappings(&db, svc, self.file_service, sink)?;
        }
        Ok(stats)
    }
}

impl HydrusPlugin {
    /// Open the Hydrus DB once and resolve the tag services to query, returning a
    /// [`HydrusReader`] for repeated per-file lookups. The library-scoped import
    /// drives this file-by-file (it owns the files, so it applies tags directly
    /// rather than staging â€” relations belong to the full [`Source::bulk_import`]).
    ///
    /// # Errors
    /// Returns an error if the Hydrus DB cannot be opened or service discovery fails.
    pub fn reader(&self) -> Result<HydrusReader> {
        let db = HydrusDb::open(&self.dir)?;
        let services = self.services(&db)?;
        Ok(HydrusReader { db, services })
    }

    /// Total relation rows (siblings + parents, current + deleted prefixes)
    /// across the configured services â€” the determinate `total` for a
    /// relations-only import's progress bar. Missing per-service tables
    /// count 0.
    ///
    /// # Errors
    /// Returns an error if the Hydrus DB cannot be opened or service
    /// discovery fails.
    pub fn count_relations(&self) -> Result<u64> {
        let db = HydrusDb::open(&self.dir)?;
        let mut total: u64 = 0;
        for svc in self.services(&db)? {
            for base in ["tag_siblings", "tag_parents"] {
                for prefix in ["current", "deleted"] {
                    let sql = format!("SELECT COUNT(*) FROM {prefix}_{base}_{svc}");
                    if let Ok(n) = db.conn().query_row(&sql, [], |r| r.get::<_, i64>(0)) {
                        total += u64::try_from(n).unwrap_or(0);
                    }
                }
            }
        }
        Ok(total)
    }

    /// Run only the relation half of [`Source::bulk_import`]: stream the
    /// sibling/parent graph for the configured services into `sink`. No
    /// mapping records (issue #41's standalone "Pull tag relations").
    ///
    /// # Errors
    /// Returns an error if the Hydrus DB cannot be opened or a query fails.
    pub fn import_relations_only(&self, sink: &mut dyn Sink) -> Result<ImportStats> {
        let db = HydrusDb::open(&self.dir)?;
        let mut stats = ImportStats::default();
        for svc in self.services(&db)? {
            stats.siblings += import_relations(&db, svc, RelationKind::Sibling, sink)?;
            stats.parents += import_relations(&db, svc, RelationKind::Parent, sink)?;
        }
        Ok(stats)
    }
}

/// An opened Hydrus DB plus the resolved tag services, for repeated per-file tag
/// lookups during a library-scoped import.
pub struct HydrusReader {
    db: HydrusDb,
    services: Vec<i64>,
}

impl HydrusReader {
    /// All Hydrus tags for one file (by SHA-256 hex), merged across the configured
    /// tag services, sorted and de-duplicated.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn tags_for_sha(&self, sha256: &str) -> Result<Vec<Tag>> {
        let mut out = Vec::new();
        for &svc in &self.services {
            out.extend(self.db.tags_for_sha256(sha256, svc)?);
        }
        out.sort_by(|a, b| {
            (a.namespace.as_str(), a.subtag.as_str())
                .cmp(&(b.namespace.as_str(), b.subtag.as_str()))
        });
        out.dedup();
        Ok(out)
    }

    /// Hydrus tags for many files (by SHA-256 hex) in one batch, merged across the
    /// configured tag services, each file's tags sorted and de-duplicated. The
    /// library-import path drives this in chunks instead of `tags_for_sha`
    /// per file.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn batch_tags(&self, shas: &[&str]) -> Result<HashMap<String, Vec<Tag>>> {
        let mut out: HashMap<String, Vec<Tag>> = HashMap::new();
        for &svc in &self.services {
            for (sha, tags) in self.db.batch_tags_for_shas(shas, svc)? {
                out.entry(sha).or_default().extend(tags);
            }
        }
        for tags in out.values_mut() {
            tags.sort_by(|a, b| {
                (a.namespace.as_str(), a.subtag.as_str())
                    .cmp(&(b.namespace.as_str(), b.subtag.as_str()))
            });
            tags.dedup();
        }
        Ok(out)
    }
}

fn import_relations(
    db: &HydrusDb,
    svc: i64,
    kind: RelationKind,
    sink: &mut dyn Sink,
) -> Result<u64> {
    let started = std::time::Instant::now();
    let kind_str = match kind {
        RelationKind::Sibling => "sibling",
        RelationKind::Parent => "parent",
    };
    let (base, from_col, to_col) = match kind {
        RelationKind::Sibling => ("tag_siblings", "bad_tag_id", "good_tag_id"),
        RelationKind::Parent => ("tag_parents", "child_tag_id", "parent_tag_id"),
    };
    let mut count = 0;
    for (prefix, status) in [
        ("current", RecordStatus::Current),
        ("deleted", RecordStatus::Deleted),
    ] {
        let sql = format!("SELECT {from_col}, {to_col} FROM {prefix}_{base}_{svc}");
        let mut stmt = match db.conn().prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let pairs: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| PluginError(format!("hydrus relations: {e}")))?
            .filter_map(|x| x.ok())
            .collect();
        for (from_id, to_id) in pairs {
            let (Some(from), Some(to)) = (db.resolve_tag(from_id)?, db.resolve_tag(to_id)?) else {
                continue;
            };
            sink.relation(RelationRecord {
                kind,
                from,
                to,
                status,
            })?;
            count += 1;
        }
    }
    tracing::debug!(
        target: "hydrus",
        svc,
        kind = kind_str,
        rows = count,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "hydrus import: relations for service"
    );
    Ok(count)
}

fn import_mappings(db: &HydrusDb, tag_svc: i64, file_svc: i64, sink: &mut dyn Sink) -> Result<u64> {
    let started = std::time::Instant::now();
    let mut count = 0;
    for (prefix, status) in [
        ("current", RecordStatus::Current),
        ("deleted", RecordStatus::Deleted),
    ] {
        let sql = format!(
            "SELECT hex(h.hash), m.tag_id
             FROM current_files_{file_svc} cf
             JOIN master.hashes h ON h.hash_id = cf.hash_id
             JOIN mappings.{prefix}_mappings_{tag_svc} m ON m.hash_id = cf.hash_id"
        );
        let mut stmt = match db.conn().prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| PluginError(format!("hydrus mappings: {e}")))?
            .filter_map(|x| x.ok())
            .collect();
        for (sha_hex, tag_id) in rows {
            let Some(tag) = db.resolve_tag(tag_id)? else {
                continue;
            };
            sink.mapping(MappingRecord {
                sha256: sha_hex.to_lowercase(),
                tag,
                status,
            })?;
            count += 1;
        }
    }
    tracing::debug!(
        target: "hydrus",
        tag_svc,
        file_svc,
        rows = count,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "hydrus import: mappings for service"
    );
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const SHA_A: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";

    fn build_fixture(dir: &std::path::Path) {
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
                [hex::decode(SHA_A).unwrap()],
            )
            .unwrap();
        master
            .execute_batch(
                "INSERT INTO namespaces VALUES (1, ''), (2, 'character');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2);",
            )
            .unwrap();
        Connection::open(dir.join("client.db")).unwrap();
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);",
            )
            .unwrap();
    }

    #[test]
    fn reader_batch_merges_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let plugin = HydrusPlugin::new(dir.path().to_path_buf(), vec![9]);
        let reader = plugin.reader().unwrap();

        let map = reader.batch_tags(&[SHA_A]).unwrap();
        let tags = map.get(SHA_A).expect("present sha");
        // Sorted by (namespace, subtag): "" < "character", so "maid" then "character:samus".
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].subtag, "maid");
        assert_eq!(tags[1].namespace, "character");
    }

    fn build_relations_fixture(dir: &std::path::Path) {
        use rusqlite::Connection;
        let master = Connection::open(dir.join("client.master.db")).unwrap();
        master
            .execute_batch(
                "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
                 INSERT INTO namespaces VALUES (1, ''), (2, 'character');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'samus_aran');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3);",
            )
            .unwrap();
        let client = Connection::open(dir.join("client.db")).unwrap();
        client
            .execute_batch(
                "CREATE TABLE current_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
                 INSERT INTO current_tag_siblings_9 VALUES (3, 2);
                 CREATE TABLE current_tag_parents_9 (child_tag_id INTEGER, parent_tag_id INTEGER);
                 INSERT INTO current_tag_parents_9 VALUES (2, 1);
                 CREATE TABLE deleted_tag_siblings_9 (bad_tag_id INTEGER, good_tag_id INTEGER);
                 INSERT INTO deleted_tag_siblings_9 VALUES (1, 2);",
            )
            .unwrap();
        // Must exist for HydrusDb::open; content irrelevant here.
        Connection::open(dir.join("client.mappings.db")).unwrap();
    }

    struct RecordingSink {
        relations: Vec<RelationRecord>,
        mappings: u64,
    }

    impl Sink for RecordingSink {
        fn mapping(&mut self, _rec: MappingRecord) -> Result<()> {
            self.mappings += 1;
            Ok(())
        }
        fn relation(&mut self, rec: RelationRecord) -> Result<()> {
            self.relations.push(rec);
            Ok(())
        }
    }

    #[test]
    fn count_relations_includes_deleted_and_missing_tables() {
        let dir = tempfile::tempdir().unwrap();
        build_relations_fixture(dir.path());
        let plugin = HydrusPlugin::new(dir.path().to_path_buf(), vec![9]);
        // 1 current sibling + 1 current parent + 1 deleted sibling; the absent
        // deleted_tag_parents_9 table counts 0 instead of erroring.
        assert_eq!(plugin.count_relations().unwrap(), 3);
    }

    #[test]
    fn relations_only_streams_relations_not_mappings() {
        let dir = tempfile::tempdir().unwrap();
        build_relations_fixture(dir.path());
        let plugin = HydrusPlugin::new(dir.path().to_path_buf(), vec![9]);

        let mut sink = RecordingSink {
            relations: Vec::new(),
            mappings: 0,
        };
        let stats = plugin.import_relations_only(&mut sink).unwrap();

        assert_eq!(sink.mappings, 0, "relations-only must feed no mappings");
        assert_eq!(
            sink.relations.len(),
            3,
            "current sibling + parent + deleted sibling"
        );
        assert_eq!(
            stats.siblings, 2,
            "streamed sibling records (current + deleted)"
        );
        assert_eq!(stats.parents, 1);
        assert_eq!(stats.mappings, 0);
        assert!(
            sink.relations
                .iter()
                .any(|r| r.status == RecordStatus::Deleted),
            "deleted rows are streamed (the db sink is what skips them)"
        );
    }
}
