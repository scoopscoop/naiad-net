//! One-shot timing of the three mapping-merge paths at the real test library's
//! scale (#151). Results and analysis:
//! `docs/perf/2026-07-28-issue-151-dual-domain-incremental.md`.
//!
//! Criterion is the wrong tool at this size: it insists on >= 10 samples, and a
//! single full merge of 4.27M rows takes minutes, so a true-scale criterion run
//! takes hours to report what one measurement of each path already says. The
//! repeatable 1/16-scale version lives in `benches/dual_domain_merge.rs`.
//!
//! Kept rather than deleted because #142 (SHA-256 deltas) needs exactly this
//! comparison again, against the same fixture, to show its own win.

use naiad_core::{FileRecord, Hash, Tag, hash_bytes};
use naiad_db::{Db, MappingDeltaInput, MappingDeltaStatus};
use std::time::Instant;

/// Tagged files in the real library (measured 2026-07-28).
const FILES: i64 = 62_926;
/// Rounded from the real library's 67.89 mappings per tagged file.
const TAGS_PER_FILE: i64 = 68;
/// Real library: 1,105,644 distinct tags over 4,271,750 mappings.
const TAG_POOL: i64 = 1_100_000;
/// The steady-state delta size the issue describes.
const DELTA_ROWS: usize = 200;

fn main() {
    let t0 = Instant::now();
    let db = Db::open_in_memory().unwrap();

    let mut hashes = Vec::with_capacity(FILES as usize);
    db.with_tx(|db| {
        for f in 0..FILES {
            let h = hash_bytes(format!("dual-domain-file-{f}").as_bytes());
            let path = format!("/dual/{f}.bin");
            db.insert_file(&FileRecord::new(h, path.into(), 1, None), 1)?;
            hashes.push(h);
        }
        Ok(())
    })
    .unwrap();
    println!("files inserted: {FILES} in {:?}", t0.elapsed());

    let svc = db
        .add_shared_service("bench_repo", "http://bench-repo/", None)
        .unwrap();

    let t = Instant::now();
    let entries: Vec<(Hash, Vec<Tag>)> = hashes
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let tags = (0..TAGS_PER_FILE)
                .map(|k| {
                    let n = (i as i64 * TAGS_PER_FILE + k) % TAG_POOL;
                    Tag::parse(&format!("bench:t{n}")).unwrap()
                })
                .collect();
            (*h, tags)
        })
        .collect();
    let rows: usize = entries.iter().map(|(_, t)| t.len()).sum();
    println!("entries built: {rows} mappings in {:?}", t.elapsed());

    // Domain-scoped variant: (tag, None) pairs for the widened merge_pulled_mappings_in_domain API.
    let domain_entries: Vec<_> = entries
        .iter()
        .map(|(h, tags)| (*h, tags.iter().map(|t| (t.clone(), None)).collect()))
        .collect();

    // 1. The path EVERY dual-domain pull took before #151: whole-service
    //    authoritative replace (DELETE ... WHERE service_id, then re-insert).
    let t = Instant::now();
    let s = db.merge_pulled_mappings(svc, &entries).unwrap();
    let full_all = t.elapsed();
    println!(
        "[1] merge_pulled_mappings (pre-#151 dual-domain path): {full_all:?}  \
         matched={} mappings={}",
        s.matched_files, s.mappings
    );

    // 2. The new SHA-256 leg: same work, scoped to one domain's provenance bit.
    //    Measures what the bitmask costs over a plain DELETE.
    let t = Instant::now();
    let s = db
        .merge_pulled_mappings_in_domain(svc, "sha256", &domain_entries)
        .unwrap();
    let full_domain = t.elapsed();
    println!(
        "[2] merge_pulled_mappings_in_domain (new sha256 leg): {full_domain:?}  \
         matched={} mappings={}",
        s.matched_files, s.mappings
    );

    // 3. The new BLAKE3 leg: the incremental path #151 restores.
    let marker = db.max_file_id().unwrap();
    let changes: Vec<MappingDeltaInput> = (0..DELTA_ROWS)
        .map(|i| MappingDeltaInput {
            hash: hashes[i],
            tag: Tag::parse(&format!("delta:t{i}")).unwrap(),
            status: MappingDeltaStatus::Current,
            seq: i as u64 + 1,
            origin: None,
        })
        .collect();
    let t = Instant::now();
    let s = db
        .merge_mapping_delta(svc, "blake3", &changes, &[], DELTA_ROWS as u64, marker)
        .unwrap();
    let delta = t.elapsed();
    println!(
        "[3] merge_mapping_delta ({DELTA_ROWS} rows, new blake3 leg): {delta:?}  \
         matched={} mappings={}",
        s.matched_files, s.mappings
    );

    println!("\n--- summary ---");
    println!("full (all-domain)     : {full_all:?}");
    println!("full (domain-scoped)  : {full_domain:?}");
    println!("delta ({DELTA_ROWS} rows)       : {delta:?}");
    println!(
        "regression factor #151: {:.0}x  (dual-domain full pull vs the delta it should have used)",
        full_all.as_secs_f64() / delta.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!(
        "bitmask overhead      : {:.2}x  (domain-scoped full vs plain full)",
        full_domain.as_secs_f64() / full_all.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!("total wall clock      : {:?}", t0.elapsed());
}
