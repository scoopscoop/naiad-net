//! Bucket/batch pull response-time benchmark (README §10): snapshot, one
//! 8-bit-prefix bucket read, and a from-zero delta over a 100k-mapping store.
//! Fixture build signs+verifies 100k submissions (~10s).

use criterion::{Criterion, criterion_group, criterion_main};
use naiad_core::{Tag, bucket_key, bucket_upper, hash_bytes};
use naiad_netproto::{Account, Op};
use naiad_server::RepoStore;
use std::hint::black_box;

const MAPPINGS: usize = 100_000;

fn build_store() -> RepoStore {
    let store = RepoStore::open_in_memory().unwrap();
    let acct = Account::generate();
    for i in 0..MAPPINGS {
        let h = hash_bytes(format!("pull-{i}").as_bytes());
        let t = Tag::parse(&format!("bench:t{}", i % 500)).unwrap();
        store.apply_submission(&acct.sign(Op::Add, &h, &t)).unwrap();
    }
    store
}

fn bench_pull(c: &mut Criterion) {
    let store = build_store();
    let probe = hash_bytes(b"pull-0");
    let lo = bucket_key(&probe, 8);
    let hi = bucket_upper(&probe, 8);

    let mut group = c.benchmark_group("repo-pull-100k");
    group.sample_size(20);
    group.bench_function("snapshot", |b| {
        b.iter(|| black_box(store.snapshot().unwrap()))
    });
    group.bench_function("bucket-8bit", |b| {
        b.iter(|| black_box(store.bucket(&lo, &hi, usize::MAX).unwrap().0))
    });
    group.bench_function("bucket-delta-from-zero", |b| {
        b.iter(|| black_box(store.bucket_delta(&lo, &hi, 0, usize::MAX).unwrap().0))
    });
    group.finish();
}

criterion_group!(benches, bench_pull);
criterion_main!(benches);
