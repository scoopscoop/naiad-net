//! Scale benchmark (README §10): tag search and completion over 1,000,000
//! mappings (200k files × 5 tags). README once called this "FTS5 search" —
//! the db uses plain SQL (no FTS5); this measures the real path.
//! Building the fixture takes ~a minute; criterion runs after that are cheap.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use naiad_core::{FileRecord, Tag, hash_bytes, parse_query};
use naiad_db::{CompletionMode, Db, Expansion, ReadScope};
use std::hint::black_box;
use std::sync::atomic::{AtomicI64, Ordering};

const TAGS: i64 = 5_000;
const FILES: i64 = 200_000; // × 5 mappings each = 1,000,000 mappings

/// Sibling edges added to the relation-rebuild fixture (round-3 review finding
/// `rebuild-bench-fixture-has-no-relation-edges`): the fixture used to have
/// zero `tag_siblings`/`tag_parents` rows, so every "rebuild" the bench timed
/// was actually a rebuild of an *empty* graph — a hollow number. Each of these
/// uses a unique bad/ideal tag pair (`tag_siblings` is `UNIQUE(bad_tag_id,
/// service_id)`, so distinct pairs are needed to reach real edge counts, not
/// a single long alias chain).
const SIBLING_EDGES: i64 = 10_000;
/// Parent edges added alongside the siblings above, for the same reason.
const PARENT_EDGES: i64 = 10_000;

fn build_db() -> Db {
    let db = Db::open_in_memory().unwrap();
    let svc = db.local_service_id().unwrap();
    let tag_ids: Vec<i64> = db
        .with_tx(|db| {
            (0..TAGS)
                .map(|i| db.intern_tag(&Tag::parse(&format!("bench:t{i}")).unwrap()))
                .collect::<Result<Vec<i64>, _>>()
        })
        .unwrap();
    db.with_tx(|db| {
        for f in 0..FILES {
            let h = hash_bytes(format!("scale-file-{f}").as_bytes());
            let path = format!("/scale/{f}.bin");
            db.insert_file(&FileRecord::new(h, path.clone().into(), 1, None), 1)?;
            let fid = db
                .file_id_by_path(std::path::Path::new(&path))?
                .expect("file just inserted");
            for k in 0..5 {
                db.add_mapping(fid, tag_ids[((f * 5 + k) % TAGS) as usize], svc)?;
            }
        }
        Ok(())
    })
    .unwrap();
    db
}

/// [`build_db`] plus a realistic relation graph on an **authored** (shared)
/// service: `SIBLING_EDGES` sibling edges and `PARENT_EDGES` parent edges, on
/// a disjoint `rel:` tag namespace so they add graph weight without changing
/// `bench:t0`'s own mapping/search counts. Used by [`bench_rebuild_components`]
/// so its numbers describe a real rebuild, not an empty one.
fn build_db_with_relations() -> Db {
    let db = build_db();
    let repo = db
        .add_shared_service("bench_repo", "http://bench-repo/", None)
        .unwrap();
    let mut cache = naiad_db::TagCache::new();

    let siblings: Vec<(Tag, Tag)> = (0..SIBLING_EDGES)
        .map(|i| {
            (
                Tag::parse(&format!("rel:sib_bad_{i}")).unwrap(),
                Tag::parse(&format!("rel:sib_ideal_{i}")).unwrap(),
            )
        })
        .collect();
    db.add_siblings_batch(repo, &siblings, &mut cache).unwrap();

    let parents: Vec<(Tag, Tag)> = (0..PARENT_EDGES)
        .map(|i| {
            (
                Tag::parse(&format!("rel:par_child_{i}")).unwrap(),
                Tag::parse(&format!("rel:par_parent_{i}")).unwrap(),
            )
        })
        .collect();
    db.add_parents_batch(repo, &parents, &mut cache).unwrap();

    db
}

fn bench_search_scale(c: &mut Criterion) {
    let db = build_db();
    let query = parse_query(&["bench:t0".to_string()]).unwrap();

    let mut group = c.benchmark_group("search-1m");
    group.sample_size(20);
    group.bench_function("raw", |b| {
        b.iter(|| {
            black_box(
                db.search(&query, ReadScope::Merged, Expansion::Raw)
                    .unwrap(),
            )
        })
    });
    group.bench_function("expanded", |b| {
        b.iter(|| {
            black_box(
                db.search(&query, ReadScope::Merged, Expansion::Expanded)
                    .unwrap(),
            )
        })
    });
    group.bench_function("complete-prefix", |b| {
        b.iter(|| black_box(db.complete_tags("t1", 20, CompletionMode::Prefix).unwrap()))
    });
    group.bench_function("complete-substring", |b| {
        b.iter(|| {
            black_box(
                db.complete_tags("42", 20, CompletionMode::Substring)
                    .unwrap(),
            )
        })
    });
    group.finish();
}

/// Diagnostic bench to isolate whether the residual after-local-write overhead
/// lives in the caches or in the surrounding search work. Times relation_graph()
/// rebuild alone vs the full search, on the same 1M-mapping + 20,000-relation-edge
/// fixture (`build_db_with_relations`, round-3 review — the previous fixture had
/// no relation edges at all).
fn bench_rebuild_components(c: &mut Criterion) {
    let db = build_db_with_relations();
    let local = db.local_service_id().unwrap();
    let query = parse_query(&["bench:t0".to_string()]).unwrap();
    let services = db.included_services(ReadScope::Merged).unwrap();

    // Warm caches once.
    db.search(&query, ReadScope::Merged, Expansion::Expanded)
        .unwrap();

    let counter = AtomicI64::new(900_000);
    let mut group = c.benchmark_group("rebuild-components");
    group.sample_size(20);

    // How much does relation-graph rebuild alone cost (without the full search)?
    // Timed: just relation_graph() after an authored write bumped the version.
    let shared = db
        .add_shared_service("diag_repo", "http://diag-repo/", None)
        .unwrap();
    let diag_hash = hash_bytes(b"scale-file-0");
    group.bench_function("relation-graph-rebuild-only", |b| {
        b.iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let tag = Tag::parse(&format!("diag:t{n}")).unwrap();
                db.merge_pulled_mappings(shared, &[(diag_hash, vec![tag])])
                    .unwrap();
            },
            |()| black_box(db.relation_graph(&services).unwrap()),
            BatchSize::PerIteration,
        )
    });

    // Warm relation_graph, then time relation_graph again (no write) — should
    // be near-zero since it just returns the cached Arc.
    group.bench_function("relation-graph-warm-hit", |b| {
        b.iter(|| black_box(db.relation_graph(&services).unwrap()))
    });

    // How long does a single version check take after a write?
    // This isolates SQLite WAL/journal-read overhead from the actual rebuild.
    group.bench_function("version-check-after-write", |b| {
        b.iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let fid = db
                    .file_id_by_path(std::path::Path::new("/scale/0.bin"))
                    .unwrap()
                    .unwrap();
                let tid = db
                    .intern_tag(&Tag::parse(&format!("diag3:t{n}")).unwrap())
                    .unwrap();
                db.add_mapping(fid, tid, local).unwrap();
            },
            |()| black_box(db.relation_graph_version().unwrap()),
            BatchSize::PerIteration,
        )
    });

    // How much does Expansion::Raw search cost after a write? Raw skips
    // relation_graph() entirely, so this measures only block_matcher +
    // files_matching — the non-graph overhead.
    group.bench_function("raw-search-after-write", |b| {
        b.iter_batched(
            || {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let fid = db
                    .file_id_by_path(std::path::Path::new("/scale/0.bin"))
                    .unwrap()
                    .unwrap();
                let tid = db
                    .intern_tag(&Tag::parse(&format!("diag2:t{n}")).unwrap())
                    .unwrap();
                db.add_mapping(fid, tid, local).unwrap();
            },
            |()| {
                black_box(
                    db.search(&query, ReadScope::Merged, Expansion::Raw)
                        .unwrap(),
                )
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_search_scale, bench_rebuild_components);
criterion_main!(benches);
