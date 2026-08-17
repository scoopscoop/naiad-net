//! Benchmark: tag-relation expansion cost in `Db::search`.
//! Builds a large synthetic graph once, then compares an expanded query
//! (siblings + parents applied) against a raw query (literal match) so the
//! delta is the expansion overhead.

use criterion::{Criterion, criterion_group, criterion_main};
use naiad_core::{FileRecord, Tag, hash_bytes, parse_query};
use naiad_db::{Db, Expansion, ReadScope};
use std::hint::black_box;

const TAGS: i64 = 10_000;
const FILES: i64 = 50_000;

/// Build a library: TAGS tags, a sibling+parent chain over the first slice of
/// them, and FILES files each mapped to a couple of tags. Returns the db.
fn build_db() -> Db {
    let db = Db::open_in_memory().unwrap();
    let svc = db.local_service_id().unwrap();

    // Intern TAGS tags: `bench:t{i}`.
    let tag_ids: Vec<i64> = (0..TAGS)
        .map(|i| {
            db.intern_tag(&Tag::parse(&format!("bench:t{i}")).unwrap())
                .unwrap()
        })
        .collect();

    // Relation graph: pair up the first 2000 tags as siblings (t1->t0, t3->t2, …)
    // and add a parent edge from each even tag to the next band, so expansion has
    // real work to do.
    for i in (0..2_000usize).step_by(2) {
        db.add_sibling(tag_ids[i + 1], tag_ids[i], svc).unwrap();
        db.add_parent(tag_ids[i], tag_ids[2_000 + (i / 2)], svc)
            .unwrap();
    }

    // FILES files, each mapped to two tags spread across the vocabulary.
    for f in 0..FILES {
        let h = hash_bytes(format!("bench-file-{f}").as_bytes());
        db.insert_file(
            &FileRecord::new(h, format!("/bench/{f}.bin").into(), 1, None),
            1,
        )
        .unwrap();
        let fid = db
            .file_id_by_path(std::path::Path::new(&format!("/bench/{f}.bin")))
            .unwrap()
            .unwrap();
        db.add_mapping(fid, tag_ids[(f % TAGS) as usize], svc)
            .unwrap();
        db.add_mapping(fid, tag_ids[((f * 7) % TAGS) as usize], svc)
            .unwrap();
    }
    db
}

fn bench_search(c: &mut Criterion) {
    let db = build_db();
    // A query whose tag participates in the relation graph (so Expanded does work).
    let query = parse_query(&["bench:t0".to_string()]).unwrap();

    let mut group = c.benchmark_group("search");
    group.bench_function("expanded", |b| {
        b.iter(|| {
            black_box(
                db.search(&query, ReadScope::Merged, Expansion::Expanded)
                    .unwrap(),
            )
        })
    });
    group.bench_function("raw", |b| {
        b.iter(|| {
            black_box(
                db.search(&query, ReadScope::Merged, Expansion::Raw)
                    .unwrap(),
            )
        })
    });
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
