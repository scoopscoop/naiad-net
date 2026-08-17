//! BLAKE3 throughput benchmark.
//!
//! Speed is the headline requirement, so hashing is measured directly: the
//! single-buffer sequential baseline, plus a many-files group comparing a
//! sequential loop against a rayon fan-out — the shape the daemon's scan uses
//! (#42). Run `cargo bench -p naiad-indexer`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use naiad_core::hash_bytes;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::hint::black_box;

fn bench_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("blake3");
    for size in [4 * 1024usize, 1024 * 1024, 16 * 1024 * 1024] {
        // Deterministic pseudo-random-ish buffer; content doesn't affect speed.
        let data = vec![0xa5u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("{size}B"), |b| {
            b.iter(|| hash_bytes(black_box(&data)));
        });
    }
    group.finish();
}

/// Many-files shape: 64 × 1 MiB buffers hashed one-by-one vs. across the rayon
/// pool. This is the per-file fan-out the daemon's scan performs (I/O excluded,
/// so it isolates the CPU-side speedup).
fn bench_hash_many(c: &mut Criterion) {
    const COUNT: usize = 64;
    const SIZE: usize = 1024 * 1024;
    let files: Vec<Vec<u8>> = (0..COUNT).map(|i| vec![(i % 251) as u8; SIZE]).collect();

    let mut group = c.benchmark_group("blake3-many");
    group.throughput(Throughput::Bytes((COUNT * SIZE) as u64));
    group.bench_function("sequential", |b| {
        b.iter(|| {
            files
                .iter()
                .map(|d| hash_bytes(black_box(d)))
                .collect::<Vec<_>>()
        });
    });
    group.bench_function("rayon", |b| {
        b.iter(|| {
            files
                .par_iter()
                .map(|d| hash_bytes(black_box(d)))
                .collect::<Vec<_>>()
        });
    });
    group.finish();
}

criterion_group!(benches, bench_hash, bench_hash_many);
criterion_main!(benches);
