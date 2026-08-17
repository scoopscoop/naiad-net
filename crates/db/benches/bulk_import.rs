//! Write-path benchmark for the review finding on migration
//! `0022_relation_graph_version_local_mappings.sql`: making the `mappings`
//! triggers behind `relation_graph_version` unconditional (needed for
//! correctness — see the migration's header comment) means every local
//! (`author IS NULL`) mapping insert now bumps *two* version counters
//! (`trust_score_version` from 0021, `relation_graph_version` from 0016+0022)
//! instead of one. The Hydrus importer inserts exactly this shape: many local
//! rows, in bulk, inside one transaction (`Db::apply_hydrus_mappings`).
//!
//! Per maxim #12 ("optimize when a benchmark says so"), this measures the
//! real cost before deciding whether to batch/defer the version bump.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use naiad_core::{FileRecord, Tag, hash_bytes};
use naiad_db::Db;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mappings applied per timed iteration, matching a modest single-batch
/// Hydrus import chunk.
const BATCH: usize = 2_000;

fn bench_bulk_import(c: &mut Criterion) {
    let db = Db::open_in_memory().unwrap();
    let svc = db.local_service_id().unwrap();

    // One fixed file; each batch uses fresh tag text so every row is a genuine
    // new insert (and so every trigger actually fires — `ON CONFLICT DO
    // NOTHING` skips triggers for rows that collide with an existing key).
    let h = hash_bytes(b"bulk-import-fixture");
    db.insert_file(&FileRecord::new(h, "/bulk/fixture.bin".into(), 1, None), 1)
        .unwrap();
    let fid = db
        .file_id_by_path(std::path::Path::new("/bulk/fixture.bin"))
        .unwrap()
        .expect("file just inserted");

    let counter = AtomicUsize::new(0);
    let mut group = c.benchmark_group("bulk-import");
    group.sample_size(20);
    group.bench_function("hydrus-import-2000-local-mappings", |b| {
        b.iter_batched(
            || {
                let base = counter.fetch_add(BATCH, Ordering::Relaxed);
                (0..BATCH)
                    .map(|i| (fid, Tag::parse(&format!("bulk:t{}", base + i)).unwrap()))
                    .collect::<Vec<_>>()
            },
            |items| black_box(db.apply_hydrus_mappings(svc, &items).unwrap()),
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_bulk_import);
criterion_main!(benches);
