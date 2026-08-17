//! Read-only access to a Hydrus database directory.
//!
//! Hydrus shards across `client.db`, `client.master.db`, `client.mappings.db`.
//! We open the main file read-only + immutable and ATTACH the other two so the
//! cross-file joins run inside SQLite.

use std::collections::HashMap;
use std::path::Path;

use naiad_core::{BudgetExceeded, Hash, Tag, approx_row_cost, bucket_key, bucket_upper};
use rusqlite::{Connection, OpenFlags, params};

use naiad_plugin::{PluginError, Result};

fn err(e: impl std::fmt::Display) -> PluginError {
    PluginError(format!("hydrus db: {e}"))
}

/// Hydrus `services.service_type` for a remote tag repository (e.g. the PTR).
///
/// These match Hydrus' `HydrusConstants` service-type enum: a remote tag
/// repository is `0`, a local tag service ("my tags") is `5`. On a full Hydrus
/// **client** database the low `service_id`s are local services; the tag
/// repository sits at a higher id — which is exactly why structural
/// auto-discovery (lowest `current_mappings_<id>`) picks the wrong, empty
/// service (#167).
pub const SERVICE_TYPE_TAG_REPOSITORY: i64 = 0;
/// Hydrus `services.service_type` for a local tag service (e.g. "my tags").
pub const SERVICE_TYPE_LOCAL_TAG: i64 = 5;

/// An opened, read-only Hydrus database (main file + ATTACHed master/mappings).
pub struct HydrusDb {
    conn: Connection,
}

impl HydrusDb {
    /// Open the Hydrus DB rooted at directory `dir` read-only and immutable.
    ///
    /// # Errors
    /// Returns an error if the files are missing or cannot be opened.
    pub fn open(dir: &Path) -> Result<Self> {
        let client = dir.join("client.db");
        let master = dir.join("client.master.db");
        let mappings = dir.join("client.mappings.db");
        for f in [&client, &master, &mappings] {
            if !f.is_file() {
                return Err(PluginError(format!("missing Hydrus file: {}", f.display())));
            }
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
        let uri = format!("file:{}?immutable=1", client.display());
        let conn = Connection::open_with_flags(uri, flags).map_err(err)?;
        conn.execute_batch(&format!(
            "ATTACH DATABASE 'file:{}?immutable=1' AS master;
             ATTACH DATABASE 'file:{}?immutable=1' AS mappings;",
            master.display(),
            mappings.display()
        ))
        .map_err(err)?;
        Ok(Self { conn })
    }

    /// Tag-service ids that have a `current_mappings_<id>` table.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn tag_service_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name FROM mappings.sqlite_master
                 WHERE type='table' AND name LIKE 'current_mappings_%'",
            )
            .map_err(err)?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(err)?
            .filter_map(|n| n.ok())
            .filter_map(|n| n.rsplit('_').next().and_then(|s| s.parse::<i64>().ok()))
            .collect();
        Ok(ids)
    }

    /// The Hydrus `services` table as a `service_id -> service_type` map.
    ///
    /// Returns `Ok(None)` when the snapshot has no `services` table at all —
    /// older exports, and the synthetic single-service fixtures that model only
    /// `client.master.db`/`client.mappings.db`. Callers then fall back to
    /// structural discovery. When present, this is the source of truth for a
    /// service's *type* (see [`SERVICE_TYPE_TAG_REPOSITORY`]).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn service_types(&self) -> Result<Option<HashMap<i64, i64>>> {
        let has_services: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='services')",
                [],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !has_services {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT service_id, service_type FROM services")
            .map_err(err)?;
        let map = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .map_err(err)?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(Some(map))
    }

    /// Number of rows in `current_mappings_<svc>`.
    ///
    /// Returns `0` when the table is absent (so callers can compare candidate
    /// services without special-casing missing tables). Used to reject the
    /// #167 trap where auto-discovery lands on a zero-row local service while a
    /// populated repository sits under another id.
    ///
    /// # Errors
    /// Returns an error only if a present table cannot be counted.
    pub fn current_mapping_count(&self, svc: i64) -> Result<i64> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mappings.sqlite_master \
                 WHERE type='table' AND name=?1)",
                [format!("current_mappings_{svc}")],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !exists {
            return Ok(0);
        }
        let sql = format!("SELECT COUNT(*) FROM mappings.current_mappings_{svc}");
        self.conn.query_row(&sql, [], |r| r.get(0)).map_err(err)
    }

    fn tag_by_id(&self, tag_id: i64) -> Result<Option<Tag>> {
        let row = self
            .conn
            .query_row(
                "SELECT n.namespace, s.subtag
                 FROM master.tags t
                 JOIN master.namespaces n ON t.namespace_id = n.namespace_id
                 JOIN master.subtags s ON t.subtag_id = s.subtag_id
                 WHERE t.tag_id = ?1",
                params![tag_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        let Some((ns, sub)) = row else {
            return Ok(None);
        };
        let raw = if ns.is_empty() {
            sub
        } else {
            format!("{ns}:{sub}")
        };
        Ok(Tag::parse(&raw).ok())
    }

    /// Tags for the file with `sha256` (hex) in tag service `svc`.
    ///
    /// One join over `current_mappings_<svc>` â†’ `tags` â†’ `namespaces`/`subtags`,
    /// so a file with N tags costs one query, not N+1 (it formerly resolved each
    /// `tag_id` separately â€” catastrophic per-file over a library against the PTR).
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn tags_for_sha256(&self, sha256: &str, svc: i64) -> Result<Vec<Tag>> {
        let bytes = hex::decode(sha256).map_err(err)?;
        let hash_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT hash_id FROM master.hashes WHERE hash = ?1",
                params![bytes],
                |r| r.get(0),
            )
            .ok();
        let Some(hash_id) = hash_id else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT n.namespace, s.subtag
             FROM mappings.current_mappings_{svc} m
             JOIN master.tags t ON t.tag_id = m.tag_id
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id
             JOIN master.subtags s ON s.subtag_id = t.subtag_id
             WHERE m.hash_id = ?1"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![hash_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(err)?
            .filter_map(|x| x.ok())
            .collect();
        let mut tags = Vec::new();
        for (ns, sub) in rows {
            let raw = if ns.is_empty() {
                sub
            } else {
                format!("{ns}:{sub}")
            };
            if let Ok(t) = Tag::parse(&raw) {
                tags.push(t);
            }
        }
        Ok(tags)
    }

    /// Tags for many files (by SHA-256 hex) in tag service `svc`, in one join per
    /// chunk of â‰¤500 hashes (under SQLite's ~999 variable limit). Returns a map
    /// keyed by **lowercase** SHA-256 hex; files with no tags are simply absent.
    /// Malformed hex inputs are skipped rather than failing the batch.
    ///
    /// This is the library-import path: it replaces a per-file `tags_for_sha256`
    /// round-trip (N+1 against the PTR) with one query per ~500 files.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn batch_tags_for_shas(
        &self,
        shas: &[&str],
        svc: i64,
    ) -> Result<HashMap<String, Vec<Tag>>> {
        let mut out: HashMap<String, Vec<Tag>> = HashMap::new();
        for chunk in shas.chunks(500) {
            let mut by_bytes: Vec<Vec<u8>> = Vec::with_capacity(chunk.len());
            for sha in chunk {
                if let Ok(bytes) = hex::decode(sha) {
                    by_bytes.push(bytes);
                }
            }
            if by_bytes.is_empty() {
                continue;
            }
            let placeholders = vec!["?"; by_bytes.len()].join(", ");
            let sql = format!(
                "SELECT hex(h.hash), n.namespace, s.subtag
                 FROM master.hashes h
                 JOIN mappings.current_mappings_{svc} m ON m.hash_id = h.hash_id
                 JOIN master.tags t ON t.tag_id = m.tag_id
                 JOIN master.namespaces n ON n.namespace_id = t.namespace_id
                 JOIN master.subtags s ON s.subtag_id = t.subtag_id
                 WHERE h.hash IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql).map_err(err)?;
            let params = rusqlite::params_from_iter(by_bytes.iter());
            let rows: Vec<(String, String, String)> = stmt
                .query_map(params, |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(err)?
                .filter_map(|x| x.ok())
                .collect();
            for (sha_hex, ns, sub) in rows {
                let key = sha_hex.to_lowercase();
                let raw = if ns.is_empty() {
                    sub
                } else {
                    format!("{ns}:{sub}")
                };
                if let Ok(t) = Tag::parse(&raw) {
                    out.entry(key).or_default().push(t);
                }
            }
        }
        Ok(out)
    }

    /// Every `(sha256_hex, tag)` mapping in tag service `svc` whose SHA-256
    /// falls in the k-anonymity bucket identified by `lo_hex` at `prefix_bits`
    /// — the one new query snapshot mode needs (design §"Query shape").
    ///
    /// Implemented as a **range scan on the hash blob**
    /// (`hash >= lo AND hash < hi`) so SQLite can walk the unique index on
    /// `master.hashes.hash` instead of scanning it, then joins
    /// `hash_id → current_mappings_<svc> → tag_id → namespaces/subtags` exactly
    /// as [`HydrusDb::batch_tags_for_shas`] does.
    ///
    /// `prefix_bits` is clamped to `[0, 256]`; `0` (one all-covering bucket)
    /// and the final all-ones bucket both make [`bucket_upper`] return its `"g"`
    /// sentinel, which is translated here into a 33-byte `0xff` blob that sorts
    /// strictly after every 32-byte hash — so the query **always** carries both
    /// bounds (`hash >= ?1 AND hash < ?2`) on every code path. This ensures
    /// SQLite drives the hashes index even on un-ANALYZEd databases (e.g. freshly
    /// downloaded 140 GB PTR snapshots). `256` is an exact-hash lookup.
    ///
    /// Returned hex is lowercase. Tags that fail [`Tag::parse`] are skipped,
    /// matching every other method on this reader. Only *current* mappings are
    /// returned: `deleted_mappings_<svc>` is a mirror-mode delta concern and
    /// has no meaning for a static snapshot read.
    ///
    /// # Errors
    /// Returns an error if `lo_hex` is not 64-char hex or if a query fails.
    pub fn mappings_for_prefix(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        svc: i64,
        budget: usize,
    ) -> anyhow::Result<(Vec<(String, String)>, usize)> {
        let lo: Hash = lo_hex
            .parse()
            .map_err(|e| PluginError(format!("hydrus db: bad bucket lo-bound {lo_hex:?}: {e}")))?;
        let bits = prefix_bits.min(256);
        let lo_blob = hex::decode(bucket_key(&lo, bits)).map_err(err)?;
        let hi_hex = bucket_upper(&lo, bits);
        // A 33-byte sentinel BLOB sorts strictly after every 32-byte hash blob in
        // SQLite (BLOBs compare byte-by-byte; longer wins on equal prefix).
        // Passing it as the upper bound means the query always has `hash < ?2`,
        // keeping the hashes-index drive intact on un-ANALYZEd databases.
        const SENTINEL: &[u8] = &[0xff_u8; 33];
        let hi_blob: Vec<u8> = if hi_hex == "g" {
            SENTINEL.to_vec()
        } else {
            hex::decode(hi_hex).map_err(err)?
        };

        let sql = format!(
            "SELECT lower(hex(h.hash)), n.namespace, s.subtag
             FROM master.hashes h
             JOIN mappings.current_mappings_{svc} m ON m.hash_id = h.hash_id
             JOIN master.tags t ON t.tag_id = m.tag_id
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id
             JOIN master.subtags s ON s.subtag_id = t.subtag_id
             WHERE h.hash >= ?1 AND h.hash < ?2"
        );

        // Drain lazily (`query`, not `query_map(..).collect()`) and charge the
        // budget per EMITTED row: on exceedance we stop here instead of
        // materialising the whole ~1 GB bucket, then rejecting it upstream (#145).
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let mut cursor = stmt.query(params![lo_blob, hi_blob]).map_err(err)?;
        let mut out: Vec<(String, String)> = Vec::new();
        let mut spent: usize = 0;
        while let Some(r) = cursor.next().map_err(err)? {
            let sha_hex: String = r.get(0).map_err(err)?;
            let ns: String = r.get(1).map_err(err)?;
            let sub: String = r.get(2).map_err(err)?;
            let display = if ns.is_empty() {
                sub
            } else {
                format!("{ns}:{sub}")
            };
            // Tags that fail to parse are skipped and cost nothing — they never
            // appear in the response, so they must not be charged.
            let Ok(tag) = Tag::parse(&display) else {
                continue;
            };
            let tag = tag.to_string();
            spent = spent.saturating_add(approx_row_cost(sha_hex.len(), tag.len()));
            if spent > budget {
                return Err(BudgetExceeded { budget }.into());
            }
            out.push((sha_hex, tag));
        }
        Ok((out, spent))
    }

    /// Streaming `(count, blake3)` digest of current `(hash, tag)` pairs in a
    /// hash band on a Hydrus snapshot, for the mirror parity audit (#184).
    /// Renders tags with `Tag::parse + skip` to match the seed's stored form,
    /// groups by hash and sorts each hash's tags in Rust for byte-identical
    /// ordering with the mirror side.  `prefix_bits == 0` audits the full range.
    ///
    /// # Errors
    /// Returns an error if `lo_hex` cannot be parsed or if a query fails.
    pub fn audit_band_digest(
        &self,
        lo_hex: &str,
        prefix_bits: u32,
        svc: i64,
    ) -> anyhow::Result<(u64, [u8; 32])> {
        use naiad_core::PairDigest;
        let lo: Hash = lo_hex
            .parse()
            .map_err(|e| PluginError(format!("hydrus db: bad bucket lo-bound {lo_hex:?}: {e}")))?;
        let bits = prefix_bits.min(256);
        let lo_blob = hex::decode(bucket_key(&lo, bits)).map_err(err)?;
        let hi_hex = bucket_upper(&lo, bits);
        const SENTINEL: &[u8] = &[0xff_u8; 33];
        let hi_blob: Vec<u8> = if hi_hex == "g" {
            SENTINEL.to_vec()
        } else {
            hex::decode(hi_hex).map_err(err)?
        };

        let sql = format!(
            "SELECT h.hash, n.namespace, s.subtag
             FROM master.hashes h
             JOIN mappings.current_mappings_{svc} m ON m.hash_id = h.hash_id
             JOIN master.tags t ON t.tag_id = m.tag_id
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id
             JOIN master.subtags s ON s.subtag_id = t.subtag_id
             WHERE h.hash >= ?1 AND h.hash < ?2
             ORDER BY h.hash"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let mut cursor = stmt.query(params![lo_blob, hi_blob]).map_err(err)?;

        let mut digest = PairDigest::new();
        let mut cur_hash: Option<[u8; 32]> = None;
        let mut cur_tags: Vec<String> = Vec::new();
        while let Some(r) = cursor.next().map_err(err)? {
            let hash_blob: Vec<u8> = r.get(0).map_err(err)?;
            let ns: String = r.get(1).map_err(err)?;
            let sub: String = r.get(2).map_err(err)?;
            let raw = if ns.is_empty() {
                sub
            } else {
                format!("{ns}:{sub}")
            };
            // Skip unparseable tags to match stream_ptr_mappings_pass / seed.
            let Ok(tag) = Tag::parse(&raw) else { continue };
            let tag = tag.to_string();
            let hash: [u8; 32] = hash_blob
                .as_slice()
                .try_into()
                .map_err(|_| err("hydrus hash blob not 32 bytes"))?;
            match cur_hash {
                Some(h) if h == hash => cur_tags.push(tag),
                _ => {
                    flush_hash_hydrus(&mut digest, &cur_hash, &mut cur_tags);
                    cur_hash = Some(hash);
                    cur_tags.push(tag);
                }
            }
        }
        flush_hash_hydrus(&mut digest, &cur_hash, &mut cur_tags);
        Ok(digest.finalize())
    }

    /// True iff **both** `current_mappings_{svc}` and `deleted_mappings_{svc}`
    /// have a hash-led index (leftmost column = `hash_id`).
    ///
    /// Used by the deferred seed path (spec §4.1, condition 3) to confirm that
    /// both passes are hash-major-ordered, which is the pre-condition that makes
    /// the no-SELECT append resolver safe. Returns `false` when either table is
    /// absent or lacks such an index — causing the seed to fall back to the
    /// normal indexed path.
    ///
    /// # Errors
    /// Returns an error if a PRAGMA query fails.
    pub fn mappings_hash_ordered(&self, svc: i64) -> Result<bool> {
        let current = format!("current_mappings_{svc}");
        let deleted = format!("deleted_mappings_{svc}");
        Ok(hash_led_index_exists(&self.conn, "mappings", &current)?
            && hash_led_index_exists(&self.conn, "mappings", &deleted)?)
    }

    /// Largest `hash_id` in the snapshot's `master.hashes`, or `None` when empty.
    /// `hash_id` is that table's `INTEGER PRIMARY KEY`, so this is an index
    /// max-scan — O(log n), effectively O(1). Read-only.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn master_hash_max(&self) -> Result<Option<u64>> {
        let max: Option<i64> = self
            .conn
            .query_row("SELECT MAX(hash_id) FROM master.hashes", [], |r| r.get(0))
            .map_err(err)?;
        Ok(max.map(|m| m as u64))
    }

    /// Stream one pass of PTR mappings — either the current pass
    /// (`deleted = false`) or the deleted pass (`deleted = true`) — for
    /// service `svc`.
    ///
    /// If the table does not exist the method returns `Ok(true)` immediately
    /// (treated as having yielded all zero rows). If the table has a hash-led
    /// index, rows arrive in ascending `hash_id` order; otherwise unordered
    /// (with a `warn!` log). The `bool` field passed to the sink always equals
    /// the `deleted` parameter.
    ///
    /// Returns `true` when all rows were consumed, `false` when the sink
    /// returned `false` (early stop). On early stop no further rows are fetched.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn stream_ptr_mappings_pass(
        &self,
        svc: i64,
        deleted: bool,
        start_after_hash_id: Option<u64>,
        sink: &mut dyn FnMut(u64, &str, &str, bool) -> bool,
    ) -> Result<bool> {
        let table_name = if deleted {
            format!("deleted_mappings_{svc}")
        } else {
            format!("current_mappings_{svc}")
        };
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mappings.sqlite_master \
                 WHERE type='table' AND name=?1)",
                [&table_name],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !exists {
            return Ok(true);
        }
        let order_clause = if hash_led_index_exists(&self.conn, "mappings", &table_name)? {
            tracing::info!(
                target: "hydrus",
                table = %table_name,
                "seed: hash-led index present — ordering ingest by hash_id"
            );
            " ORDER BY m.hash_id"
        } else {
            tracing::warn!(
                target: "hydrus",
                table = %table_name,
                "seed: NO hash-led index on source — streaming unordered to avoid \
                 a full external sort; seed throughput will be write-random. \
                 Rebuild the Hydrus service index or use a Format-A snapshot."
            );
            ""
        };
        // The resume predicate is only ever passed Some(_) on the Format-A path,
        // where the hash-led index holds ORDER BY m.hash_id, so the filtered scan
        // stays index-ordered and monotone (I3). It is only meaningful with a
        // hash-led index; the WHERE stays correct regardless of order.
        let where_clause = if start_after_hash_id.is_some() {
            " WHERE m.hash_id > ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT m.hash_id, lower(hex(h.hash)), n.namespace, s.subtag
             FROM mappings.{table_name} m
             JOIN master.hashes h ON h.hash_id = m.hash_id
             JOIN master.tags t ON t.tag_id = m.tag_id
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id
             JOIN master.subtags s ON s.subtag_id = t.subtag_id{where_clause}{order_clause}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let mut rows = match start_after_hash_id {
            Some(h) => stmt.query([h as i64]).map_err(err)?,
            None => stmt.query([]).map_err(err)?,
        };
        while let Some(row) = rows.next().map_err(err)? {
            let hash_id: i64 = row.get(0).map_err(err)?;
            let sha: String = row.get(1).map_err(err)?;
            let ns: String = row.get(2).map_err(err)?;
            let sub: String = row.get(3).map_err(err)?;
            let raw = if ns.is_empty() {
                sub
            } else {
                format!("{ns}:{sub}")
            };
            if let Ok(tag) = Tag::parse(&raw) {
                if !sink(hash_id as u64, &sha, &tag.to_string(), deleted) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Stream every `(sha256_hex, tag_string, is_deleted)` row from BOTH
    /// `current_mappings_{svc}` and `deleted_mappings_{svc}`, with NO
    /// restriction to locally-present files. Tables that do not exist are
    /// silently skipped. Tags that fail to parse are silently skipped (matching
    /// the existing import path's behaviour).
    ///
    /// When the source table has a hash-led index (i.e. an index whose leftmost
    /// column is `hash_id`), rows are streamed in ascending `hash_id` order —
    /// enabling right-edge-append writes in `repo_mappings` during bulk ingest.
    /// When no such index exists the stream is unordered (fallback), avoiding a
    /// full external sort on Format-B snapshots.
    ///
    /// The sink returns `true` to continue or `false` to stop early. When the
    /// sink returns `false` no further rows are fetched and the method returns
    /// `Ok(())` immediately.
    ///
    /// This is a thin wrapper that calls [`Self::stream_ptr_mappings_pass`]
    /// twice (current, then deleted). Existing callers and tests are unaffected.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn stream_all_ptr_mappings(
        &self,
        svc: i64,
        sink: &mut dyn FnMut(&str, &str, bool) -> bool,
    ) -> Result<()> {
        let mut adapt = |_hid: u64, sha: &str, tag: &str, del: bool| sink(sha, tag, del);
        if self.stream_ptr_mappings_pass(svc, false, None, &mut adapt)? {
            self.stream_ptr_mappings_pass(svc, true, None, &mut adapt)?;
        }
        Ok(())
    }

    /// Stream every `(service_hash_id, sha256_hex)` row from
    /// `repository_hash_id_map_{svc}` row by row without buffering.
    ///
    /// The sink returns `true` to continue or `false` to stop early. When the
    /// sink returns `false` no further rows are fetched and the method returns
    /// `Ok(())` immediately. This avoids loading the full ~196M-row hash map
    /// (measured 196,418,074 rows on the 2026-07 PTR snapshot, ≈18 GB as hex
    /// strings) into memory before inserting.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn stream_ptr_hash_id_map(
        &self,
        svc: i64,
        sink: &mut dyn FnMut(u64, &str) -> bool,
    ) -> Result<()> {
        let sql = format!(
            "SELECT m.service_hash_id, lower(hex(h.hash))
             FROM master.repository_hash_id_map_{svc} m
             JOIN master.hashes h ON h.hash_id = m.hash_id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let mut rows = stmt.query([]).map_err(err)?;
        while let Some(row) = rows.next().map_err(err)? {
            let id: i64 = row.get(0).map_err(err)?;
            let sha: String = row.get(1).map_err(err)?;
            if !sink(id as u64, &sha) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Return the service-local id -> tag definition map for `svc`, sourced from
    /// `repository_tag_id_map_{svc}` in the master database.
    ///
    /// The hash-id map is intentionally NOT returned here: at PTR scale it holds
    /// ~196M rows and must be streamed via `stream_ptr_hash_id_map` to avoid
    /// OOM. The tag map is ~53M rows (measured 52,996,539 on the 2026-07 PTR
    /// snapshot, ≈1.6 GB of text before container overhead) — collecting it is
    /// tolerable but borderline; revisit if memory pressure appears.
    ///
    /// Tags that fail to parse are silently skipped. Row-read errors are
    /// propagated (unlike the old `filter_map` collector).
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn repository_tag_id_map(&self, svc: i64) -> Result<Vec<(u64, String)>> {
        let mut out = Vec::new();
        self.stream_ptr_tag_id_map(svc, &mut |id, tag| {
            out.push((id, tag.to_string()));
            true
        })?;
        Ok(out)
    }

    /// Stream every `(service_tag_id, parsed_tag)` from `repository_tag_id_map_{svc}`
    /// row by row, applying [`Tag::parse`] (skipping unparseable tags exactly as
    /// [`Self::repository_tag_id_map`] does). Avoids buffering the ~53M-row map
    /// (measured 52,996,539 rows on the 2026-07 PTR snapshot) into a `Vec`.
    ///
    /// The sink returns `true` to continue or `false` to stop early.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn stream_ptr_tag_id_map(
        &self,
        svc: i64,
        sink: &mut dyn FnMut(u64, &str) -> bool,
    ) -> Result<()> {
        let sql = format!(
            "SELECT m.service_tag_id, n.namespace, s.subtag
             FROM master.repository_tag_id_map_{svc} m
             JOIN master.tags t ON t.tag_id = m.tag_id
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id
             JOIN master.subtags s ON s.subtag_id = t.subtag_id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let mut rows = stmt.query([]).map_err(err)?;
        while let Some(row) = rows.next().map_err(err)? {
            let id: i64 = row.get(0).map_err(err)?;
            let ns: String = row.get(1).map_err(err)?;
            let sub: String = row.get(2).map_err(err)?;
            let raw = if ns.is_empty() {
                sub
            } else {
                format!("{ns}:{sub}")
            };
            let Ok(tag) = Tag::parse(&raw) else {
                continue; // skip unparseable, same as repository_tag_id_map
            };
            if !sink(id as u64, &tag.to_string()) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Stream `([u8;32] hash, internal master tag_id)` for current mappings whose
    /// `hash_id` is in the band `[lo_hash_id, hi_hash_id)`, ORDERED BY
    /// `(hash_id, tag_id)`. The covering index `(hash_id, tag_id)` is driven
    /// directly, so no transient per-band index build occurs. Yields the raw
    /// **internal `tag_id`** — F10 filtering (unparseable tags) is the seeder's
    /// responsibility via the in-memory translation map built by
    /// [`Self::stream_ptr_tag_translation`].
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn stream_ptr_idband_mappings(
        &self,
        svc: i64,
        lo_hash_id: u64,
        hi_hash_id: u64,
        sink: &mut dyn FnMut([u8; 32], u64) -> bool,
    ) -> Result<()> {
        let sql = format!(
            "SELECT h.hash, m.tag_id
             FROM mappings.current_mappings_{svc} m
             JOIN master.hashes h ON h.hash_id = m.hash_id
             WHERE m.hash_id >= ?1 AND m.hash_id < ?2
             ORDER BY m.hash_id, m.tag_id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        // SQLite stores integers as i64; clamp to i64::MAX so callers can pass
        // u64::MAX as "no upper bound" without the value wrapping to -1.
        let lo = lo_hash_id.min(i64::MAX as u64) as i64;
        let hi = hi_hash_id.min(i64::MAX as u64) as i64;
        let mut rows = stmt.query(params![lo, hi]).map_err(err)?;
        while let Some(r) = rows.next().map_err(err)? {
            let hash_vec: Vec<u8> = r.get(0).map_err(err)?;
            let tag_id: i64 = r.get(1).map_err(err)?;
            let Ok(hash) = <[u8; 32]>::try_from(hash_vec.as_slice()) else {
                continue; // a non-32-byte hash blob cannot be a sha256 key
            };
            if !sink(hash, tag_id as u64) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Stream `(internal master tag_id, service_tag_id)` for every tag that
    /// passes [`Tag::parse`] in `repository_tag_id_map_{svc}`. This is a single
    /// full-table scan (one cost for all bands), using the same 4-way join as
    /// [`Self::stream_ptr_tag_id_map`]. Tags that fail `Tag::parse` (the F10
    /// filter) are simply not emitted; their `tag_id` is absent from the
    /// caller's translation map and their mappings are dropped at apply time.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn stream_ptr_tag_translation(
        &self,
        svc: i64,
        sink: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<()> {
        let sql = format!(
            "SELECT rtm.tag_id, rtm.service_tag_id, n.namespace, s.subtag
             FROM master.repository_tag_id_map_{svc} rtm
             JOIN master.tags t ON t.tag_id = rtm.tag_id
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id
             JOIN master.subtags s ON s.subtag_id = t.subtag_id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let mut rows = stmt.query([]).map_err(err)?;
        while let Some(row) = rows.next().map_err(err)? {
            let tag_id: i64 = row.get(0).map_err(err)?;
            let sid: i64 = row.get(1).map_err(err)?;
            let ns: String = row.get(2).map_err(err)?;
            let sub: String = row.get(3).map_err(err)?;
            let raw = if ns.is_empty() {
                sub
            } else {
                format!("{ns}:{sub}")
            };
            if Tag::parse(&raw).is_err() {
                continue; // F10: skip unparseable tags
            }
            if !sink(tag_id as u64, sid as u64) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Return the maximum `tag_id` in `master.repository_tag_id_map_{svc}`, or
    /// `None` if the table is empty. Used by the seeder to size the direct-index
    /// translation array (see `stream_ptr_tag_translation`).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn max_rtm_tag_id(&self, svc: i64) -> Result<Option<u64>> {
        let sql = format!("SELECT max(tag_id) FROM master.repository_tag_id_map_{svc}");
        let val: Option<i64> = self.conn.query_row(&sql, [], |r| r.get(0)).map_err(err)?;
        Ok(val.map(|v| v as u64))
    }

    /// Return the maximum `hash_id` in `mappings.current_mappings_{svc}`, or
    /// `None` if the table is empty. O(1) index seek to the end of the covering
    /// index. Used by the seeder to compute the number of `hash_id` bands.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn max_current_hash_id(&self, svc: i64) -> Result<Option<u64>> {
        let sql = format!("SELECT max(hash_id) FROM mappings.current_mappings_{svc}");
        let val: Option<i64> = self.conn.query_row(&sql, [], |r| r.get(0)).map_err(err)?;
        Ok(val.map(|v| v as u64))
    }

    /// Return the highest `update_index` in `repository_updates_{svc}` for
    /// which every hash_id has all `processed = 1` rows in
    /// `repository_updates_processed_{svc}`.
    ///
    /// Returns `None` when the updates table is absent, empty, or every index
    /// has at least one unprocessed row. The seed should set
    /// `next_update_index = watermark.map(|w| w + 1).unwrap_or(0)`.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn recover_watermark(&self, svc: i64) -> Result<Option<u64>> {
        let updates_table = format!("repository_updates_{svc}");
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name=?1)",
                [&updates_table],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !exists {
            return Ok(None);
        }
        let sql = format!(
            "SELECT MAX(idx) FROM (
                 SELECT u.update_index AS idx
                 FROM repository_updates_{svc} u
                 LEFT JOIN repository_updates_processed_{svc} p
                   ON p.hash_id = u.hash_id
                 GROUP BY u.update_index
                 HAVING COUNT(p.hash_id) > 0
                    AND SUM(CASE WHEN p.processed = 0 THEN 1 ELSE 0 END) = 0
                    AND SUM(CASE WHEN p.hash_id IS NULL THEN 1 ELSE 0 END) = 0
             )"
        );
        let val: Option<i64> = self.conn.query_row(&sql, [], |r| r.get(0)).map_err(err)?;
        Ok(val.map(|v| v as u64))
    }

    /// Discover repository service ids by probing `repository_updates_*` tables
    /// (excluding `repository_updates_processed_*`).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn repository_service_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type='table'
                   AND name LIKE 'repository_updates_%'
                   AND name NOT LIKE 'repository_updates_processed_%'",
            )
            .map_err(err)?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(err)?
            .filter_map(|n| n.ok())
            .filter_map(|n| n.rsplit('_').next().and_then(|s| s.parse::<i64>().ok()))
            .collect();
        Ok(ids)
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn resolve_tag(&self, tag_id: i64) -> Result<Option<Tag>> {
        self.tag_by_id(tag_id)
    }
}

/// True iff `schema.table` has an index whose **leftmost** column (`seqno = 0`)
/// is `hash_id`.
///
/// Uses `PRAGMA <schema>.index_list(<table>)` and
/// `PRAGMA <schema>.index_info(<index>)` to inspect the index metadata without
/// parsing any DDL strings.  Returns `true` as soon as the first matching index
/// is found; returns `false` when the table has no indexes or none of them lead
/// with `hash_id`.
///
/// This is the probe that `stream_all_ptr_mappings` uses to decide whether
/// `ORDER BY m.hash_id` is served by a covering-index scan (free) or would
/// require a full external sort (prohibited on Format-B snapshots).
fn hash_led_index_exists(conn: &Connection, schema: &str, table: &str) -> Result<bool> {
    // PRAGMA index_list columns: seq | name | unique | origin | partial
    let index_list_sql = format!("PRAGMA {schema}.index_list({table})");
    let mut stmt = conn.prepare(&index_list_sql).map_err(err)?;
    let index_names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(err)?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt); // release the borrow on `conn` before issuing the next PRAGMA

    for idx_name in &index_names {
        // PRAGMA index_info columns: seqno | cid | name
        // seqno 0 is the leftmost (first) column in the index definition.
        let index_info_sql = format!("PRAGMA {schema}.index_info({idx_name})");
        let mut stmt2 = conn.prepare(&index_info_sql).map_err(err)?;
        let leftmost_col: Option<String> = stmt2
            .query_map([], |row| {
                let seqno: i64 = row.get(0)?;
                let col_name: String = row.get(2)?;
                Ok((seqno, col_name))
            })
            .map_err(err)?
            .filter_map(|r| r.ok())
            .find(|(seqno, _)| *seqno == 0)
            .map(|(_, name)| name);

        if leftmost_col.as_deref() == Some("hash_id") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Flush one hash group into `digest`: sort tags in Rust then feed each pair.
/// Called by [`HydrusDb::audit_band_digest`] after every hash transition.
fn flush_hash_hydrus(
    digest: &mut naiad_core::PairDigest,
    hash: &Option<[u8; 32]>,
    tags: &mut Vec<String>,
) {
    if let Some(h) = hash {
        tags.sort_unstable();
        // Dedup collapses distinct raw Hydrus tags that normalize to the same naiad
        // string. For example, subtags "Maid" and "maid" both pass through
        // Tag::parse (which lowercases via normalize()) to produce "maid"; without
        // dedup each raw row adds one update() call, inflating the count relative
        // to the mirror which stores only the single normalized form. This was the
        // documented "false mismatch #2" (operating-a-repo.md). Real corruption
        // (a mapping present on one side but not the other, or with a genuinely
        // different normalized tag string) still mismatches: dedup only removes
        // exact-duplicate normalized strings within one hash group.
        tags.dedup();
        for t in tags.iter() {
            digest.update(h, t);
        }
    }
    tags.clear();
}

/// Default Hydrus file service holding owned files (`hydrus local file storage`).
pub const DEFAULT_FILE_SERVICE: i64 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const SHA_A: &str = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
    const SHA_B: &str = "aabbccddeeff00112233445566778899aabbccddeeff001122334455667788ff";
    // SHA_C starts 0x33 — a higher 8-bit bucket than SHA_A (0x11) — and carries
    // one current mapping.  Used to verify upper-bound pinning (a query bounded
    // to SHA_A's 8-bit bucket must exclude SHA_C).
    const SHA_C: &str = "3344556677889900aabbccddeeff001122334455667788990011223344556677";
    // SHA_FF is the maximum possible 32-byte hash (all 0xff).  It sits in the
    // final 8-bit bucket which makes bucket_upper return "g" → the 33-byte
    // sentinel.  A 32-byte sentinel (= SHA_FF itself) would exclude it under
    // `hash < ?2`; the 33-byte sentinel admits it, pinning the sentinel size.
    const SHA_FF: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    /// Extended fixture for PTR bridge seed path tests.  Service 9 throughout.
    ///
    /// Hashes: SHA_A (hash_id=1, two current mappings: maid + character:samus),
    /// SHA_B (hash_id=2, no mappings — “present hash with no tags” case),
    /// SHA_C (hash_id=3, one current mapping: series:metroid; higher 8-bit bucket
    /// than SHA_A — used to verify upper-bound exclusion),
    /// SHA_FF (hash_id=4, one current mapping: series:endgame; the all-ff hash in
    /// the final 8-bit bucket — pins the 33-byte sentinel size).
    ///
    /// Tables (on top of the plain fixture structure):
    /// - `mappings.deleted_mappings_9`  — one deleted row (hash_id=1, meta:badtag)
    /// - `master.repository_hash_id_map_9` — 500→hash_id 1 (SHA_A), 501→hash_id 2 (SHA_B)
    /// - `master.repository_tag_id_map_9` — 800→tag_id 2 (character:samus),
    ///   801→tag_id 1 (maid, empty-namespace), 802→tag_id 6 (unparseable empty subtag)
    /// - `client.repository_updates_9` — index 0 (fully processed), index 1
    ///   (partially processed)
    /// - `client.repository_updates_processed_9` — matching rows
    fn build_ptr_fixture(dir: &std::path::Path) {
        // â”€â”€ master â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        let master = Connection::open(dir.join("client.master.db")).unwrap();
        master
            .execute_batch(
                "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
                 CREATE TABLE repository_hash_id_map_9 (service_hash_id INTEGER PRIMARY KEY, hash_id INTEGER);
                 CREATE TABLE repository_tag_id_map_9 (service_tag_id INTEGER PRIMARY KEY, tag_id INTEGER);",
            )
            .unwrap();
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (1, ?1)",
                [hex::decode(SHA_A).unwrap()],
            )
            .unwrap();
        // SHA_B exists in master.hashes but is referenced by no mappings row —
        // the spec §7 "present hash with no mappings" case.
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (2, ?1)",
                [hex::decode(SHA_B).unwrap()],
            )
            .unwrap();
        // SHA_C is in a higher 8-bit bucket than SHA_A (0x33 > 0x11) and carries
        // one current mapping — used to verify the upper bound is actually applied.
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (3, ?1)",
                [hex::decode(SHA_C).unwrap()],
            )
            .unwrap();
        // SHA_FF is the all-ff maximum hash; its 8-bit bucket causes bucket_upper
        // to return "g" (the sentinel), pinning the 33-byte sentinel size.
        master
            .execute(
                "INSERT INTO hashes (hash_id, hash) VALUES (4, ?1)",
                [hex::decode(SHA_FF).unwrap()],
            )
            .unwrap();
        master
            .execute_batch(
                // namespaces: 1='', 2='character', 3='meta', 4='series'
                // subtags:    1='maid', 2='samus', 3='badtag', 4='metroid', 5='endgame',
                //             6='' (empty — Tag::parse rejects empty subtag)
                // tags:       1=maid (ns ''), 2=character:samus, 3=meta:badtag,
                //             4=series:metroid, 5=series:endgame, 6='' (unparseable)
                // repository_hash_id_map_9: 500→hash 1 (SHA_A), 501→hash 2 (SHA_B)
                // repository_tag_id_map_9:
                //   800→tag 2 (character:samus, namespaced)
                //   801→tag 1 (maid, empty-namespace branch)
                //   802→tag 6 ('' empty string, Tag::parse skips it)
                "INSERT INTO namespaces VALUES (1, ''), (2, 'character'), (3, 'meta'), (4, 'series');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, 'badtag'), (4, 'metroid'), (5, 'endgame'), (6, '');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3), (4, 4, 4), (5, 4, 5), (6, 1, 6);
                 INSERT INTO repository_hash_id_map_9 VALUES (500, 1), (501, 2);
                 INSERT INTO repository_tag_id_map_9 VALUES (800, 2), (801, 1), (802, 6);",
            )
            .unwrap();

        // â”€â”€ client.db (main) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        // index 0: hash_id=100, all processed=1 â†’ fully processed
        // index 1: hash_id=101, one processed=0 â†’ partially processed
        let client = Connection::open(dir.join("client.db")).unwrap();
        client
            .execute_batch(
                "CREATE TABLE repository_updates_9 (update_index INTEGER, hash_id INTEGER);
                 INSERT INTO repository_updates_9 VALUES (0, 100), (1, 101), (2, 100), (2, 102);
                 CREATE TABLE repository_updates_processed_9
                     (hash_id INTEGER, content_type INTEGER, processed INTEGER);
                 INSERT INTO repository_updates_processed_9
                     VALUES (100, 1, 1), (101, 1, 1), (101, 2, 0);",
            )
            .unwrap();

        // â”€â”€ client.mappings.db â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1), (4, 3), (5, 4);
                 CREATE TABLE deleted_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO deleted_mappings_9 VALUES (3, 1);",
            )
            .unwrap();
    }

    #[test]
    fn stream_all_ptr_mappings_includes_deleted_and_ignores_local_presence() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        let mut rows: Vec<(String, String, bool)> = Vec::new();
        db.stream_all_ptr_mappings(9, &mut |sha, tag, del| {
            rows.push((sha.to_string(), tag.to_string(), del));
            true
        })
        .unwrap();
        assert!(
            rows.iter().any(|(_, t, d)| t == "character:samus" && !*d),
            "current mapping character:samus must appear"
        );
        assert!(
            rows.iter().any(|(_, t, d)| t == "meta:badtag" && *d),
            "deleted mapping meta:badtag must appear"
        );
    }

    #[test]
    fn repository_tag_id_map_resolves_service_tags() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        let tags = db.repository_tag_id_map(9).unwrap();
        assert!(
            tags.iter()
                .any(|(tid, tag)| *tid == 800 && tag == "character:samus"),
            "service_tag_id 800 must resolve to character:samus"
        );
    }

    #[test]
    fn stream_ptr_hash_id_map_yields_correct_rows() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        let mut rows: Vec<(u64, String)> = Vec::new();
        db.stream_ptr_hash_id_map(9, &mut |id, sha| {
            rows.push((id, sha.to_string()));
            true
        })
        .unwrap();
        assert!(
            rows.iter().any(|(id, sha)| *id == 500 && sha.len() == 64),
            "service_hash_id 500 must resolve to a 64-char sha256 hex"
        );
    }

    #[test]
    fn stream_ptr_hash_id_map_stops_on_false() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        let mut count = 0u32;
        db.stream_ptr_hash_id_map(9, &mut |_id, _sha| {
            count += 1;
            false // stop after first row
        })
        .unwrap();
        assert_eq!(
            count, 1,
            "sink returning false must stop after exactly one row"
        );
    }

    #[test]
    fn stream_ptr_tag_id_map_matches_collected() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        let mut streamed: Vec<(u64, String)> = Vec::new();
        db.stream_ptr_tag_id_map(9, &mut |id, tag| {
            streamed.push((id, tag.to_string()));
            true
        })
        .unwrap();
        streamed.sort();
        let mut collected = db.repository_tag_id_map(9).unwrap();
        collected.sort();
        assert_eq!(streamed, collected, "streamed set must equal collected set");
        // Namespaced tag: exercises the `format!("{ns}:{sub}")` branch.
        assert!(
            streamed
                .iter()
                .any(|(id, t)| *id == 800 && t == "character:samus"),
            "service_tag_id 800 must resolve to character:samus"
        );
        // Empty-namespace tag: exercises the `ns.is_empty()` branch (raw = sub).
        assert!(
            streamed.iter().any(|(id, t)| *id == 801 && t == "maid"),
            "service_tag_id 801 must resolve to maid (empty-namespace branch)"
        );
        // Unparseable row (802 -> empty subtag) must be absent from both sets.
        assert!(
            !streamed.iter().any(|(id, _)| *id == 802),
            "service_tag_id 802 (empty subtag, unparseable) must be skipped"
        );
        // Exactly two parseable rows: 800 and 801.
        assert_eq!(streamed.len(), 2, "only two parseable rows expected");
    }

    #[test]
    fn stream_ptr_tag_id_map_stops_on_false() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        // Fixture has 2 parseable rows (800→character:samus, 801→maid) and one
        // unparseable (802→empty subtag, skipped). Returning false on the first
        // call must stop before the second parseable row is yielded.
        let mut count = 0u32;
        db.stream_ptr_tag_id_map(9, &mut |_id, _tag| {
            count += 1;
            false
        })
        .unwrap();
        assert_eq!(count, 1, "sink returning false stops after exactly one row");
    }

    #[test]
    fn recover_watermark_is_highest_fully_processed_index() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        // index 0 fully processed, index 1 partially -> watermark = Some(0).
        assert_eq!(db.recover_watermark(9).unwrap(), Some(0));
    }

    /// Regression: an update_index whose hash list contains a hash_id that has
    /// ZERO rows in `repository_updates_processed_{svc}` must NOT be treated as
    /// fully processed.
    ///
    /// Bug: with a LEFT JOIN, missing processed rows yield p.hash_id=NULL.
    /// COUNT(p.hash_id) skips NULLs, so another hash’s row kept COUNT > 0,
    /// and CASE WHEN NULL=0 fell to ELSE 0 making SUM stay 0. The HAVING
    /// wrongly passed, treating the never-processed index as a watermark candidate.
    ///
    /// Fixture: index 2 has hash_ids 100 (processed) and 102 (never-processed).
    /// The watermark must remain Some(0).
    #[test]
    fn recover_watermark_excludes_never_processed_hash() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        // index 2: hash_id=100 (has processed rows) AND hash_id=102 (no rows in
        // processed table at all). The never-processed hash must block index 2.
        let wm = db.recover_watermark(9).unwrap();
        assert_eq!(
            wm,
            Some(0),
            "watermark must be 0, not 2 (never-processed hash must block index 2)"
        );
    }

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
        // client.db must exist for HydrusDb::open; no tables needed here.
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
    fn batch_tags_present_and_absent() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let map = db.batch_tags_for_shas(&[SHA_A, SHA_B], 9).unwrap();

        let a = map.get(SHA_A).expect("present sha has tags");
        assert_eq!(a.len(), 2, "two tags for the present sha");
        assert!(
            a.iter()
                .any(|t| t.namespace == "character" && t.subtag == "samus"),
            "namespaced tag present"
        );
        assert!(
            a.iter()
                .any(|t| t.namespace.is_empty() && t.subtag == "maid"),
            "unnamespaced tag present"
        );
        assert!(!map.contains_key(SHA_B), "absent sha not in map");
    }

    #[test]
    fn batch_tags_skips_malformed_hex() {
        let dir = tempfile::tempdir().unwrap();
        build_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let map = db.batch_tags_for_shas(&["zz", SHA_A], 9).unwrap();
        assert!(map.contains_key(SHA_A));
    }

    /// Emoticon-style tags survive the Hydrus import path losslessly.
    ///
    /// In Hydrus, `:)` is stored as namespace="" and subtag=":)" (the leading
    /// colon is part of the subtag, not the namespace).  The canonical naiad
    /// string form doubles the leading colon (`"::)"`).  Both the raw form
    /// `":)"` and the canonical form `"::)"` must parse to the same tag with
    /// subtag `":)"` â€” i.e. neither drops nor reinterprets the colon.
    #[test]
    fn emoticon_tag_imports_losslessly() {
        // Build a fixture where the subtag is ":)" (Hydrus style).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();

        let master = Connection::open(p.join("client.master.db")).unwrap();
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
                // namespace="" and subtag=":)" â€” the Hydrus emoticon convention.
                "INSERT INTO namespaces VALUES (1, '');
                 INSERT INTO subtags VALUES (1, ':)');
                 INSERT INTO tags VALUES (1, 1, 1);",
            )
            .unwrap();
        Connection::open(p.join("client.db")).unwrap();
        let mappings = Connection::open(p.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1);",
            )
            .unwrap();

        let db = HydrusDb::open(p).unwrap();
        let map = db.batch_tags_for_shas(&[SHA_A], 9).unwrap();
        let tags = map.get(SHA_A).expect("emoticon tag should be present");
        assert_eq!(tags.len(), 1);
        let t = &tags[0];

        // Subtag must preserve the leading colon â€” not be silently stripped.
        assert_eq!(t.namespace, "", "should be unnamespaced");
        assert_eq!(t.subtag, ":)", "colon must be part of subtag");

        // Both raw and canonical forms parse to the same tag.
        let from_raw = Tag::parse(":)").unwrap();
        let from_canonical = Tag::parse("::)").unwrap();
        assert_eq!(*t, from_raw, "raw form matches");
        assert_eq!(*t, from_canonical, "canonical form matches");

        // The canonical display form doubles the leading colon.
        assert_eq!(t.to_string(), "::)");
    }

    /// The lo-bound hex of the bucket containing `sha` at `bits`.
    fn lo_of(sha: &str, bits: u32) -> String {
        let h: naiad_core::Hash = sha.parse().expect("test sha parses");
        naiad_core::bucket_key(&h, bits)
    }

    #[test]
    fn mappings_for_prefix_hits_and_includes_bare_subtag() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // SHA_A starts 0x11; an 8-bit bucket at 0x11 must contain it.
        let rows = db
            .mappings_for_prefix(&lo_of(SHA_A, 8), 8, 9, usize::MAX)
            .unwrap()
            .0;
        assert_eq!(
            rows.len(),
            2,
            "SHA_A has exactly two current mappings: {rows:?}"
        );
        assert!(
            rows.iter().all(|(sha, _)| sha == SHA_A),
            "every row is keyed by the in-bucket sha: {rows:?}"
        );
        assert!(
            rows.iter().any(|(_, tag)| tag == "character:samus"),
            "namespaced tag present: {rows:?}"
        );
        assert!(
            rows.iter().any(|(_, tag)| tag == "maid"),
            "bare-subtag (namespace-less) tag present: {rows:?}"
        );
    }

    #[test]
    fn mappings_for_prefix_misses_absent_bucket() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // No fixture hash starts with 0xee; the 0xff bucket is occupied by SHA_FF.
        let lo = "ee".to_string() + &"00".repeat(31);
        let rows = db.mappings_for_prefix(&lo, 8, 9, usize::MAX).unwrap().0;
        assert!(
            rows.is_empty(),
            "absent bucket must yield nothing: {rows:?}"
        );
    }

    #[test]
    fn mappings_for_prefix_exact_hash() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let rows = db.mappings_for_prefix(SHA_A, 256, 9, usize::MAX).unwrap().0;
        assert_eq!(
            rows.len(),
            2,
            "exact-hash query returns SHA_A's tags: {rows:?}"
        );

        let rows = db.mappings_for_prefix(SHA_B, 256, 9, usize::MAX).unwrap().0;
        assert!(rows.is_empty(), "SHA_B has no mappings: {rows:?}");
    }

    #[test]
    fn mappings_for_prefix_hash_without_mappings_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // SHA_B starts 0xaa and IS in master.hashes, but has no mappings row.
        let rows = db
            .mappings_for_prefix(&lo_of(SHA_B, 8), 8, 9, usize::MAX)
            .unwrap()
            .0;
        assert!(
            rows.is_empty(),
            "a present hash with no mappings yields no rows, not an error: {rows:?}"
        );
    }

    #[test]
    fn mappings_for_prefix_zero_bits_and_final_bucket_use_sentinel_upper_bound() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // 0 bits = one all-covering bucket; bucket_upper returns the "g"
        // sentinel → 33-byte 0xff blob.  SHA_A:2 + SHA_C:1 + SHA_FF:1 = 4.
        // SHA_B has no mappings and does not contribute.
        let rows = db
            .mappings_for_prefix(&"00".repeat(32), 0, 9, usize::MAX)
            .unwrap()
            .0;
        assert_eq!(
            rows.len(),
            4,
            "zero-bit scan covers all mapped hashes: {rows:?}"
        );

        // The final 8-bit bucket (lo = "ff"*32) also overflows to the sentinel.
        // SHA_FF lives in this bucket and must be returned.
        let rows = db
            .mappings_for_prefix(&"ff".repeat(32), 8, 9, usize::MAX)
            .unwrap()
            .0;
        assert_eq!(
            rows.len(),
            1,
            "final bucket contains exactly SHA_FF's mapping: {rows:?}"
        );
        assert!(
            rows.iter().any(|(_, tag)| tag == "series:endgame"),
            "SHA_FF's series:endgame tag must appear: {rows:?}"
        );
    }

    /// Pins the 33-byte sentinel: SHA_FF (`"ff"*32`) is the maximum 32-byte hash
    /// and lives in the final 8-bit bucket, which makes `bucket_upper` return "g"
    /// → the 33-byte `0xff` sentinel blob.  With a 32-byte sentinel the bound
    /// becomes `hash < SHA_FF`, excluding it.  With 33 bytes it is admitted.
    #[test]
    fn mappings_for_prefix_sentinel_covers_final_bucket_hash() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // SHA_FF lives in the final 8-bit bucket (0xff…); its mapping must appear.
        // A 32-byte sentinel would make the bound `hash < SHA_FF`, excluding it.
        let rows = db
            .mappings_for_prefix(&"ff".repeat(32), 8, 9, usize::MAX)
            .unwrap()
            .0;
        assert_eq!(
            rows.len(),
            1,
            "SHA_FF's mapping must appear in the final bucket: {rows:?}"
        );
        assert!(
            rows.iter().any(|(sha, _)| sha == SHA_FF),
            "SHA_FF is the returned hash: {rows:?}"
        );

        // SHA_C's 8-bit bucket (0x33…) has a normal hex upper bound; SHA_FF
        // must not bleed in from the higher bucket.
        let rows = db
            .mappings_for_prefix(&lo_of(SHA_C, 8), 8, 9, usize::MAX)
            .unwrap()
            .0;
        assert!(
            rows.iter().all(|(sha, _)| sha != SHA_FF),
            "SHA_FF must not appear in SHA_C's 0x33 bucket: {rows:?}"
        );
    }

    #[test]
    fn mappings_for_prefix_budget_admits_all_rows_when_generous() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let (rows, spent) = db
            .mappings_for_prefix(&lo_of(SHA_A, 8), 8, 9, usize::MAX)
            .unwrap();
        assert_eq!(rows.len(), 2, "generous budget returns every row: {rows:?}");
        assert!(spent > 0, "a non-empty bucket charges some bytes");
    }

    #[test]
    fn mappings_for_prefix_budget_trips_and_returns_budget_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // A 1-byte budget cannot admit even the first row: the drain must stop
        // mid-bucket and surface a downcastable BudgetExceeded.
        let err = db
            .mappings_for_prefix(&lo_of(SHA_A, 8), 8, 9, 1)
            .expect_err("a 1-byte budget must trip");
        assert!(
            err.downcast_ref::<naiad_core::BudgetExceeded>().is_some(),
            "must be a BudgetExceeded in the chain: {err:?}"
        );
    }

    // ── mappings_hash_ordered and stream_ptr_mappings_pass tests ──────────

    #[test]
    fn mappings_hash_ordered_true_on_dual_indexed_fixture() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        assert!(
            db.mappings_hash_ordered(9).unwrap(),
            "dual-indexed fixture must return true"
        );
    }

    #[test]
    fn mappings_hash_ordered_false_when_neither_pass_indexed() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path()); // no hash-led indexes
        let db = HydrusDb::open(dir.path()).unwrap();
        assert!(
            !db.mappings_hash_ordered(9).unwrap(),
            "unindexed fixture must return false"
        );
    }

    #[test]
    fn mappings_hash_ordered_false_when_only_current_indexed() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        // Add index to current only — deleted lacks it.
        let mappings = Connection::open(dir.path().join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE UNIQUE INDEX current_mappings_9_hash_id_tag_id_index \
                     ON current_mappings_9 (hash_id, tag_id);",
            )
            .unwrap();
        let db = HydrusDb::open(dir.path()).unwrap();
        assert!(
            !db.mappings_hash_ordered(9).unwrap(),
            "only current indexed must return false (deleted lacks an index)"
        );
    }

    #[test]
    fn stream_ptr_mappings_pass_current_only_yields_is_deleted_false() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let mut rows: Vec<(String, String, bool)> = Vec::new();
        db.stream_ptr_mappings_pass(9, false, None, &mut |_hid, sha, tag, del| {
            rows.push((sha.to_string(), tag.to_string(), del));
            true
        })
        .unwrap();

        assert!(
            !rows.is_empty(),
            "current pass must yield rows on indexed fixture"
        );
        assert!(
            rows.iter().all(|(_, _, del)| !del),
            "all rows from current pass must have is_deleted=false"
        );
        assert!(
            rows.iter().any(|(_, t, _)| t == "character:samus"),
            "current pass must include character:samus"
        );
    }

    #[test]
    fn stream_ptr_mappings_pass_deleted_only_yields_is_deleted_true() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let mut rows: Vec<(String, String, bool)> = Vec::new();
        db.stream_ptr_mappings_pass(9, true, None, &mut |_hid, sha, tag, del| {
            rows.push((sha.to_string(), tag.to_string(), del));
            true
        })
        .unwrap();

        assert!(
            !rows.is_empty(),
            "deleted pass must yield rows on indexed fixture"
        );
        assert!(
            rows.iter().all(|(_, _, del)| *del),
            "all rows from deleted pass must have is_deleted=true"
        );
        assert!(
            rows.iter().any(|(_, t, _)| t == "meta:badtag"),
            "deleted pass must include meta:badtag"
        );
    }

    /// Per-pass rows combined equal the output of `stream_all_ptr_mappings`.
    #[test]
    fn stream_ptr_mappings_pass_combined_matches_stream_all() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // Per-pass
        let mut per_pass: Vec<(String, String, bool)> = Vec::new();
        db.stream_ptr_mappings_pass(9, false, None, &mut |_hid, sha, tag, del| {
            per_pass.push((sha.to_string(), tag.to_string(), del));
            true
        })
        .unwrap();
        db.stream_ptr_mappings_pass(9, true, None, &mut |_hid, sha, tag, del| {
            per_pass.push((sha.to_string(), tag.to_string(), del));
            true
        })
        .unwrap();

        // Combined
        let mut combined: Vec<(String, String, bool)> = Vec::new();
        db.stream_all_ptr_mappings(9, &mut |sha, tag, del| {
            combined.push((sha.to_string(), tag.to_string(), del));
            true
        })
        .unwrap();

        let mut pp = per_pass.clone();
        let mut cb = combined.clone();
        pp.sort();
        cb.sort();
        assert_eq!(
            pp, cb,
            "per-pass streams combined must yield the same rows as stream_all_ptr_mappings"
        );
    }

    // ── hash-led-index probe and hash-major ordering tests ─────────────────

    /// Build the same data as `build_ptr_fixture` but add a hash-led covering
    /// index on both `current_mappings_9` and `deleted_mappings_9`, making this
    /// a Format-A fixture. The index mirrors the name Hydrus uses:
    /// `<table>_hash_id_tag_id_index (hash_id, tag_id)`.
    fn build_ptr_fixture_indexed(dir: &std::path::Path) {
        build_ptr_fixture(dir);
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE UNIQUE INDEX current_mappings_9_hash_id_tag_id_index \
                     ON current_mappings_9 (hash_id, tag_id);
                 CREATE UNIQUE INDEX deleted_mappings_9_hash_id_tag_id_index \
                     ON deleted_mappings_9 (hash_id, tag_id);",
            )
            .unwrap();
    }

    /// `hash_led_index_exists` returns `true` when the named index exists on an
    /// attached schema table and `false` when the table has no indexes.
    #[test]
    fn hash_led_index_exists_detects_presence_and_absence() {
        // Without index.
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        assert!(
            !hash_led_index_exists(db.conn(), "mappings", "current_mappings_9").unwrap(),
            "unindexed fixture must return false"
        );

        // With index.
        let dir2 = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir2.path());
        let db2 = HydrusDb::open(dir2.path()).unwrap();
        assert!(
            hash_led_index_exists(db2.conn(), "mappings", "current_mappings_9").unwrap(),
            "indexed fixture must return true"
        );
    }

    /// On a Format-A fixture (hash-led index present):
    /// (a) `EXPLAIN QUERY PLAN` of the generated ordered SQL must not contain
    ///     `USE TEMP B-TREE FOR ORDER BY` — the covering index serves the ORDER.
    /// (b) Streamed rows must arrive hash-contiguous and in ascending source
    ///     `hash_id` order (hash_id 1 → SHA_A, then 3 → SHA_C, then 4 → SHA_FF).
    #[test]
    fn stream_all_ptr_mappings_indexed_fixture_orders_by_hash_id() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // (a) EXPLAIN QUERY PLAN — the ordered SQL must not materialise a temp
        //     B-tree for the ORDER BY when the covering index is present.
        let explain_sql = "EXPLAIN QUERY PLAN \
             SELECT lower(hex(h.hash)), n.namespace, s.subtag \
             FROM mappings.current_mappings_9 m \
             JOIN master.hashes h ON h.hash_id = m.hash_id \
             JOIN master.tags t ON t.tag_id = m.tag_id \
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id \
             JOIN master.subtags s ON s.subtag_id = t.subtag_id \
             ORDER BY m.hash_id";
        let mut stmt = db.conn().prepare(explain_sql).unwrap();
        // EXPLAIN QUERY PLAN columns: id | parent | notused | detail
        let plan_details: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect();
        for detail in &plan_details {
            assert!(
                !detail.contains("USE TEMP B-TREE FOR ORDER BY"),
                "hash-led index must serve ORDER BY without a temp B-tree sort: {detail}"
            );
        }

        // (b) Stream rows and verify hash-contiguous, ascending-hash_id ordering.
        // The fixture has:
        //   hash_id=1 → SHA_A (two current mappings: maid, character:samus)
        //   hash_id=3 → SHA_C (one current mapping: series:metroid)
        //   hash_id=4 → SHA_FF (one current mapping: series:endgame)
        // With ORDER BY m.hash_id the current-pass rows must arrive in that order.
        let mut current_rows: Vec<String> = Vec::new();
        db.stream_all_ptr_mappings(9, &mut |sha, _tag, is_deleted| {
            if !is_deleted {
                current_rows.push(sha.to_owned());
            }
            true
        })
        .unwrap();

        // Each sha's rows must be contiguous (no sha reappears after a break).
        let mut seen: Vec<String> = Vec::new();
        let mut last: Option<&str> = None;
        for sha in &current_rows {
            if last != Some(sha.as_str()) {
                assert!(
                    !seen.contains(sha),
                    "sha {sha} reappeared non-contiguously in stream: {current_rows:?}"
                );
                seen.push(sha.clone());
                last = Some(sha.as_str());
            }
        }

        // Shas must appear in ascending source hash_id order:
        // SHA_A (hash_id 1) before SHA_C (hash_id 3) before SHA_FF (hash_id 4).
        let pos = |sha: &str| seen.iter().position(|s| s == sha);
        let pos_a = pos(SHA_A).expect("SHA_A must appear in current stream");
        let pos_c = pos(SHA_C).expect("SHA_C must appear in current stream");
        let pos_ff = pos(SHA_FF).expect("SHA_FF must appear in current stream");
        assert!(
            pos_a < pos_c && pos_c < pos_ff,
            "rows must arrive in ascending hash_id order (A={pos_a}, C={pos_c}, FF={pos_ff})"
        );
    }

    #[test]
    fn stream_resume_filters_hash_id() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        // Resume after hash_id=1 (SHA_A). Current-pass rows must be only hash_id 3 and 4,
        // ascending, and never hash_id 1.
        let mut rows: Vec<(u64, String)> = Vec::new();
        db.stream_ptr_mappings_pass(9, false, Some(1), &mut |hid, _sha, tag, _del| {
            rows.push((hid, tag.to_string()));
            true
        })
        .unwrap();

        assert!(
            !rows.is_empty(),
            "resume after hash_id=1 must still yield hash_id 3 and 4"
        );
        assert!(
            rows.iter().all(|(hid, _)| *hid > 1),
            "every resumed row must have hash_id > 1: {rows:?}"
        );
        // Ascending hash_id order (the hash-led index serves ORDER BY).
        let ids: Vec<u64> = rows.iter().map(|(h, _)| *h).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(
            ids, sorted,
            "resumed rows must arrive in ascending hash_id order"
        );
        assert!(
            ids.contains(&3) && ids.contains(&4),
            "hash_id 3 and 4 must appear"
        );

        // None cursor yields the full pass INCLUDING hash_id=1.
        let mut full: Vec<u64> = Vec::new();
        db.stream_ptr_mappings_pass(9, false, None, &mut |hid, _sha, _tag, _del| {
            full.push(hid);
            true
        })
        .unwrap();
        assert!(full.contains(&1), "unfiltered pass must include hash_id=1");
    }

    #[test]
    fn master_hash_max_reads_pk_max() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture_indexed(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        // build_ptr_fixture inserts hash_id 1,2,3,4 → MAX = 4.
        assert_eq!(db.master_hash_max().unwrap(), Some(4));
    }

    /// Empty `master.hashes` must return `None`, not `Some(0)`.
    ///
    /// Guards the `snapshot_fingerprint` contract: it calls
    /// `master_hash_max()?.unwrap_or(0)` — the `unwrap_or` is only correct if
    /// this method returns `None` (not `Some(0)`) when the table is empty.
    #[test]
    fn master_hash_max_empty_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        // Build a minimal three-file layout: master.hashes table exists but
        // has no rows; client.db and client.mappings.db are valid empty dbs.
        let master = rusqlite::Connection::open(dir.path().join("client.master.db")).unwrap();
        master
            .execute_batch("CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);")
            .unwrap();
        drop(master);
        rusqlite::Connection::open(dir.path().join("client.db"))
            .unwrap()
            .execute_batch("")
            .unwrap();
        rusqlite::Connection::open(dir.path().join("client.mappings.db"))
            .unwrap()
            .execute_batch("")
            .unwrap();

        let db = HydrusDb::open(dir.path()).unwrap();
        assert_eq!(
            db.master_hash_max().unwrap(),
            None,
            "empty master.hashes must return None, not Some(0)"
        );
    }

    /// On a Format-B fixture (no hash-led index):
    /// * Streaming still returns all expected rows (fallback arm is correct).
    /// * As a negative control, the same SQL *with* ORDER BY appended shows
    ///   `USE TEMP B-TREE FOR ORDER BY` in EXPLAIN QUERY PLAN — confirming that
    ///   the probe correctly identifies this as unindexed and that our code is
    ///   right to omit ORDER BY in this case.
    #[test]
    fn stream_all_ptr_mappings_unindexed_fallback_returns_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path()); // no hash-led index
        let db = HydrusDb::open(dir.path()).unwrap();

        // Probe confirms this fixture is unindexed.
        assert!(
            !hash_led_index_exists(db.conn(), "mappings", "current_mappings_9").unwrap(),
            "unindexed fixture: probe must return false"
        );

        // Negative control: adding ORDER BY to the unindexed table would force a
        // temp B-tree sort (proving the probe is meaningful).
        let explain_with_order = "EXPLAIN QUERY PLAN \
             SELECT lower(hex(h.hash)), n.namespace, s.subtag \
             FROM mappings.current_mappings_9 m \
             JOIN master.hashes h ON h.hash_id = m.hash_id \
             JOIN master.tags t ON t.tag_id = m.tag_id \
             JOIN master.namespaces n ON n.namespace_id = t.namespace_id \
             JOIN master.subtags s ON s.subtag_id = t.subtag_id \
             ORDER BY m.hash_id";
        let mut stmt = db.conn().prepare(explain_with_order).unwrap();
        let details: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect();
        assert!(
            details
                .iter()
                .any(|d| d.contains("USE TEMP B-TREE FOR ORDER BY")),
            "unindexed table must show B-tree sort when ORDER BY is applied \
             (confirms the probe is meaningful): {details:?}"
        );

        // Streaming (without ORDER BY — the unordered fallback) still returns
        // all rows correctly.
        let mut rows: Vec<(String, String, bool)> = Vec::new();
        db.stream_all_ptr_mappings(9, &mut |sha, tag, del| {
            rows.push((sha.to_owned(), tag.to_owned(), del));
            true
        })
        .unwrap();
        // Fixture: 4 current + 1 deleted = 5 parseable rows.
        assert_eq!(
            rows.len(),
            5,
            "unindexed fallback must return all 5 rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|(_, t, d)| t == "character:samus" && !*d),
            "current mapping character:samus must be present"
        );
        assert!(
            rows.iter().any(|(_, t, d)| t == "meta:badtag" && *d),
            "deleted mapping meta:badtag must be present"
        );
    }

    // ── audit_band_digest ─────────────────────────────────────────────────────

    #[test]
    fn audit_band_digest_count_matches_fixture_mappings() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_fixture(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();
        // build_ptr_fixture inserts 4 current mappings for service 9:
        //   SHA_A: "maid" (tag_id=1) + "character:samus" (tag_id=2) = 2
        //   SHA_C: "series:metroid" (tag_id=4)                       = 1
        //   SHA_FF: "series:endgame" (tag_id=5)                      = 1
        // Total = 4.  Full-range scan (prefix_bits == 0) must return all four.
        let lo = "00".repeat(32);
        let (count, _digest) = db.audit_band_digest(&lo, 0, 9).unwrap();
        assert_eq!(
            count, 4,
            "fixture has exactly 4 current mappings; got {count}"
        );
    }

    /// Two distinct Hydrus tag_ids that normalize to the same naiad string must
    /// be counted as ONE mapping after dedup, matching the mirror store which
    /// holds only the single normalized form.
    ///
    /// Collision rule: `Tag::parse` lowercases via `normalize()`, so subtag "Maid"
    /// and subtag "maid" both produce the canonical string "maid".  Hydrus can
    /// legitimately store both as separate tag_ids (they are distinct rows in
    /// `master.tags`); the parity audit must treat them as one.
    ///
    /// Without `tags.dedup()` this test would return count=2 (pre-fix behaviour),
    /// causing a false FAIL against a mirror that correctly stores the tag once.
    #[test]
    fn audit_band_digest_dedup_collapses_normalised_collision() {
        // Build a minimal fixture: SHA_A with two tag_ids that both normalize to
        // "maid" — subtag "maid" (tag_id=1) and subtag "Maid" (tag_id=2).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();

        let master = Connection::open(p.join("client.master.db")).unwrap();
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
                // namespace_id=1 is the empty namespace (unnamespaced tag).
                // subtag_id=1 -> "maid", subtag_id=2 -> "Maid" — distinct raw rows,
                // same normalized output ("maid" after lowercasing).
                "INSERT INTO namespaces VALUES (1, '');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'Maid');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 1, 2);",
            )
            .unwrap();
        Connection::open(p.join("client.db")).unwrap();
        let mappings = Connection::open(p.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                // Both tag_ids (1 and 2) are mapped to hash_id=1.
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);",
            )
            .unwrap();

        let db = HydrusDb::open(p).unwrap();
        let lo = "00".repeat(32);
        let (count, _digest) = db.audit_band_digest(&lo, 0, 9).unwrap();
        // After dedup both raw tags collapse to the single normalized string "maid".
        // Pre-fix this would return count=2 (a false mismatch against the mirror).
        assert_eq!(
            count, 1,
            "two raw Hydrus tags normalising to 'maid' must count as 1 after dedup; \
             got {count} (pre-fix this would be 2)"
        );
    }

    /// A genuinely absent mapping (mirror holds "maid" but not "character:samus")
    /// must still be detected as a mismatch after dedup — dedup must not mask
    /// real drift between the two sides.
    ///
    /// This test exercises the Hydrus side in isolation: a fixture with two
    /// genuinely distinct tags must report count=2, not 1.
    #[test]
    fn audit_band_digest_dedup_does_not_collapse_distinct_tags() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();

        let master = Connection::open(p.join("client.master.db")).unwrap();
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
                // tag_id=1: unnamespaced "maid"; tag_id=2: "character:samus".
                // These are genuinely distinct after normalization.
                "INSERT INTO namespaces VALUES (1, ''), (2, 'character');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2);",
            )
            .unwrap();
        Connection::open(p.join("client.db")).unwrap();
        let mappings = Connection::open(p.join("client.mappings.db")).unwrap();
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1);",
            )
            .unwrap();

        let db = HydrusDb::open(p).unwrap();
        let lo = "00".repeat(32);
        let (count, _digest) = db.audit_band_digest(&lo, 0, 9).unwrap();
        // "maid" and "character:samus" are distinct even after normalization;
        // dedup must not collapse them.
        assert_eq!(
            count, 2,
            "two genuinely distinct tags must count as 2 even after dedup; got {count}"
        );
    }

    /// Inline PTR fixture matching `fixture::write_ptr_seed_fixture` data layout.
    /// Service 9: h1(0x11..)=hash_id 1, h2(0x33..)=hash_id 2, h3(0xaa..)=hash_id 3,
    /// h4(0xbb..)=hash_id 4. Mappings: h1→{tag_id 1,2}, h2→{tag_id 1}, h4→{tag_id 3}.
    /// repository_tag_id_map_9: 800→tag_id 1 ("maid"), 801→tag_id 2 ("character:samus"),
    /// 802→tag_id 3 (empty subtag — unparseable, F10).
    fn build_ptr_seed_fixture_for_idband(dir: &std::path::Path) {
        let h1 = hex::decode(format!("11{}", "00".repeat(31))).unwrap();
        let h2 = hex::decode(format!("33{}", "00".repeat(31))).unwrap();
        let h3 = hex::decode(format!("aa{}", "00".repeat(31))).unwrap();
        let h4 = hex::decode(format!("bb{}", "00".repeat(31))).unwrap();
        let master = Connection::open(dir.join("client.master.db")).unwrap();
        master
            .execute_batch(
                "CREATE TABLE hashes (hash_id INTEGER PRIMARY KEY, hash BLOB);
                 CREATE TABLE namespaces (namespace_id INTEGER PRIMARY KEY, namespace TEXT);
                 CREATE TABLE subtags (subtag_id INTEGER PRIMARY KEY, subtag TEXT);
                 CREATE TABLE tags (tag_id INTEGER PRIMARY KEY, namespace_id INTEGER, subtag_id INTEGER);
                 CREATE TABLE repository_tag_id_map_9 (service_tag_id INTEGER PRIMARY KEY, tag_id INTEGER);
                 INSERT INTO namespaces VALUES (1, ''), (2, 'character');
                 INSERT INTO subtags VALUES (1, 'maid'), (2, 'samus'), (3, '');
                 INSERT INTO tags VALUES (1, 1, 1), (2, 2, 2), (3, 1, 3);
                 INSERT INTO repository_tag_id_map_9 VALUES (800, 1), (801, 2), (802, 3);",
            )
            .unwrap();
        for (hid, blob) in [(1i64, &h1), (2, &h2), (3, &h3), (4, &h4)] {
            master
                .execute(
                    "INSERT INTO hashes (hash_id, hash) VALUES (?1, ?2)",
                    rusqlite::params![hid, blob],
                )
                .unwrap();
        }
        Connection::open(dir.join("client.db")).unwrap();
        let mappings = Connection::open(dir.join("client.mappings.db")).unwrap();
        // h1(hash_id=1)→{tag 1, tag 2}, h2(hash_id=2)→{tag 1}, h4(hash_id=4)→{tag 3}.
        mappings
            .execute_batch(
                "CREATE TABLE current_mappings_9 (tag_id INTEGER, hash_id INTEGER);
                 INSERT INTO current_mappings_9 VALUES (1, 1), (2, 1), (1, 2), (3, 4);",
            )
            .unwrap();
    }

    /// `stream_ptr_idband_mappings` yields rows in hash_id order, respects the
    /// `[lo, hi)` window, and emits raw tag_ids including unparseable ones.
    #[test]
    fn stream_ptr_idband_mappings_orders_and_selects() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_seed_fixture_for_idband(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let h1: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x11;
            b
        };
        let h2: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0x33;
            b
        };
        let h4: [u8; 32] = {
            let mut b = [0u8; 32];
            b[0] = 0xbb;
            b
        };

        // Full range → all 4 mapping rows in hash_id order.
        let mut rows: Vec<([u8; 32], u64)> = Vec::new();
        db.stream_ptr_idband_mappings(9, 0, u64::MAX, &mut |h, tid| {
            rows.push((h, tid));
            true
        })
        .unwrap();
        assert!(!rows.is_empty(), "fixture has current mappings");

        // First row is h1 (hash_id=1, lowest).
        assert_eq!(rows[0].0, h1, "first row must be h1 (lowest hash_id)");
        // Raw tag_id=3 (unparseable) must be emitted — filtering is the seeder's job.
        assert!(
            rows.iter().any(|(_, tid)| *tid == 3),
            "raw tag_id 3 (unparseable) must be emitted by idband streamer"
        );

        // Narrow window [1, 2) → only hash_id=1 rows (h1).
        let mut narrow: Vec<([u8; 32], u64)> = Vec::new();
        db.stream_ptr_idband_mappings(9, 1, 2, &mut |h, tid| {
            narrow.push((h, tid));
            true
        })
        .unwrap();
        assert!(!narrow.is_empty(), "window [1,2) must yield h1 rows");
        assert!(
            narrow.iter().all(|(h, _)| *h == h1),
            "narrow [1,2) must only yield h1"
        );

        // Window [2, 3) → only h2.
        let mut h2_rows: Vec<([u8; 32], u64)> = Vec::new();
        db.stream_ptr_idband_mappings(9, 2, 3, &mut |h, tid| {
            h2_rows.push((h, tid));
            true
        })
        .unwrap();
        assert!(!h2_rows.is_empty(), "window [2,3) must yield h2 rows");
        assert!(
            h2_rows.iter().all(|(h, _)| *h == h2),
            "window [2,3) must only yield h2"
        );

        // Window [4, 5) → only h4 (unparseable tag_id=3 still emitted).
        let mut h4_rows: Vec<([u8; 32], u64)> = Vec::new();
        db.stream_ptr_idband_mappings(9, 4, 5, &mut |h, tid| {
            h4_rows.push((h, tid));
            true
        })
        .unwrap();
        assert_eq!(h4_rows.len(), 1, "h4 has exactly one mapping");
        assert!(
            h4_rows.iter().all(|(h, _)| *h == h4),
            "window [4,5) must only yield h4"
        );
        assert_eq!(
            h4_rows[0].1, 3,
            "h4's mapping has tag_id=3 (unparseable, emitted raw)"
        );

        // Empty window → no rows.
        let mut empty: Vec<([u8; 32], u64)> = Vec::new();
        db.stream_ptr_idband_mappings(9, 10, 20, &mut |h, tid| {
            empty.push((h, tid));
            true
        })
        .unwrap();
        assert!(empty.is_empty(), "window [10,20) must yield no rows");
    }

    /// `stream_ptr_tag_translation` yields (tag_id, service_tag_id) for parseable
    /// tags only — unparseable tag (802, empty subtag) must be absent (F10 filter).
    #[test]
    fn stream_ptr_tag_translation_parseable_and_f10() {
        let dir = tempfile::tempdir().unwrap();
        build_ptr_seed_fixture_for_idband(dir.path());
        let db = HydrusDb::open(dir.path()).unwrap();

        let mut pairs: Vec<(u64, u64)> = Vec::new();
        db.stream_ptr_tag_translation(9, &mut |tag_id, sid| {
            pairs.push((tag_id, sid));
            true
        })
        .unwrap();

        // tag_id=1 → service_tag_id=800 ("maid"), tag_id=2 → 801 ("character:samus").
        assert!(
            pairs.contains(&(1, 800)),
            "tag_id=1 must map to service_tag_id=800 (maid); got {pairs:?}"
        );
        assert!(
            pairs.contains(&(2, 801)),
            "tag_id=2 must map to service_tag_id=801 (character:samus); got {pairs:?}"
        );
        // tag_id=3 / service_tag_id=802 (empty subtag) must be absent — F10 filter.
        assert!(
            !pairs.iter().any(|(_, sid)| *sid == 802),
            "unparseable service_tag_id 802 must be absent (F10 filter)"
        );
        assert!(
            !pairs.iter().any(|(tid, _)| *tid == 3),
            "tag_id=3 (unparseable) must be absent from translation"
        );
    }
}
