//! Indexer throughput benchmark (README §10): files/sec draining `scan()`
//! (walk + fingerprint + dual hash) over a temp tree of 500 × 8 KiB files.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

const FILES: usize = 500;

fn bench_scan(c: &mut Criterion) {
    let specs: Vec<(String, Vec<u8>)> = (0..FILES)
        .map(|i| {
            (
                format!("d{}/f{i}.png", i % 10),
                vec![(i % 251) as u8; 8 * 1024],
            )
        })
        .collect();
    let refs: Vec<(&str, &[u8])> = specs
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let dir = naiad_test_support::fixture_dir(&refs);

    let mut group = c.benchmark_group("indexer-scan");
    group.throughput(Throughput::Elements(FILES as u64));
    group.sample_size(10);
    group.bench_function("scan-500x8k", |b| {
        b.iter(|| {
            let n = naiad_indexer::scan(black_box(dir.path()))
                .filter(|r| r.is_ok())
                .count();
            assert_eq!(n, FILES);
        })
    });
    group.finish();
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
