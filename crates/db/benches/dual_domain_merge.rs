//! Dual-domain pull merge benchmark (#151).
//!
//! Establishes the baseline for the defect in #151: a repo that advertises
//! more than one hash domain falls into `pull_repo`'s multi-domain arm, which
//! always calls [`Db::merge_pulled_mappings`] — a whole-service authoritative
//! replace — instead of the incremental [`Db::merge_mapping_delta`]. The client
//! therefore pays a full snapshot merge on *every* pull, forever, while holding
//! the global `Mutex<Db>`.
//!
//! The fixture is sized to a real ~95k-file library snapshot (measured 2026-07-28):
//!
//! | metric | value |
//! |---|---|
//! | files | 94,317 |
//! | tagged files | 62,926 |
//! | mappings | 4,271,750 |
//! | tags | 1,105,644 |
//! | avg tags per tagged file | 67.89 |
//!
//! `full-snapshot` is what every dual-domain pull cost before the fix; `delta`
//! is what a single-domain BLAKE3 pull costs and what this issue restores. The
//! ratio between them is the regression #151 describes. `full-snapshot-domain`
//! is the new SHA-256 leg — the same full merge, scoped to one provenance bit —
//! and exists to show the bitmask does not make a full pull materially slower.
//!
//! **Scale.** The fixture is the real library's *shape* at 1/16 of its size:
//! criterion insists on >= 10 samples, and a full merge of 4.27M rows takes
//! minutes, so a true-scale criterion run takes hours to report what one
//! measurement of each path already says. Merge cost is linear in row count, so
//! the ratios hold; the true-scale one-shot numbers are recorded in
//! `docs/perf/2026-07-28-issue-151-dual-domain-incremental.md`.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use naiad_core::{FileRecord, Tag, hash_bytes};
use naiad_db::{Db, MappingDeltaInput, MappingDeltaStatus};
use std::hint::black_box;

/// Owned snapshot entries for the domain-scoped merge path.
type DomainEntries = Vec<(naiad_core::Hash, Vec<(Tag, Option<String>)>)>;

/// Tagged files in the fixture: the real library's 62,926 at 1/16 scale.
const FILES: i64 = 3_933;
/// Tags per file, rounded from the real library's 67.89 average.
const TAGS_PER_FILE: i64 = 68;
/// Distinct tags drawn from. The real library has 1.1M tags over 4.27M
/// mappings, i.e. each tag recurs ~3.9 times; this pool preserves that ratio
/// at the same 1/16 scale.
const TAG_POOL: i64 = 68_750;
/// Rows in an incremental delta — the "~200 delta rows per pull" the issue
/// describes as the steady state this regression replaced.
const DELTA_ROWS: usize = 200;

/// Build a library of `FILES` files with a subscribed shared service, plus the
/// snapshot entries a full pull from that service would produce.
fn build_fixture() -> (Db, i64, Vec<(naiad_core::Hash, Vec<Tag>)>, DomainEntries) {
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

    let svc = db
        .add_shared_service("bench_repo", "http://bench-repo/", None)
        .unwrap();

    // Snapshot entries: every file carries TAGS_PER_FILE tags off the pool.
    let entries: Vec<(naiad_core::Hash, Vec<Tag>)> = hashes
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

    // Domain-scoped variant: same entries with (tag, None) pairs for the widened API.
    let domain_entries: DomainEntries = entries
        .iter()
        .map(|(h, tags)| (*h, tags.iter().map(|t| (t.clone(), None)).collect()))
        .collect();

    (db, svc, entries, domain_entries)
}

/// The two merge paths, head to head at real-library scale.
fn bench_merge_paths(c: &mut Criterion) {
    let (db, svc, entries, domain_entries) = build_fixture();

    let mut group = c.benchmark_group("dual-domain-merge");
    // A full merge of 4.27M rows is seconds, not microseconds; keep criterion's
    // sample count at its floor so the bench finishes in minutes rather than
    // hours. The signal here is the order of magnitude, not the third digit.
    group.sample_size(10);

    // The pre-#151 dual-domain pull: whole-service DELETE, then one INSERT per
    // (file, tag) — every row rewritten under the global DB lock, every pull.
    group.bench_function("full-snapshot", |b| {
        b.iter(|| black_box(db.merge_pulled_mappings(svc, &entries).unwrap()))
    });

    // The new SHA-256 leg: the same full merge, scoped to one provenance bit.
    // Trades one whole-service DELETE for a bit-clear plus a reap of rows that
    // reached mask 0; this arm is what says whether that trade is cheap.
    group.bench_function("full-snapshot-domain", |b| {
        b.iter(|| {
            black_box(
                db.merge_pulled_mappings_in_domain(svc, "sha256", &domain_entries)
                    .unwrap(),
            )
        })
    });

    // Prime the service so the delta merges against a populated table, which is
    // the realistic case — a delta arriving at an already-synced client.
    db.merge_pulled_mappings(svc, &entries).unwrap();

    // The incremental path this issue restores: DELTA_ROWS statements, no wipe.
    let marker = db.max_file_id().unwrap();
    group.bench_function("delta", |b| {
        b.iter_batched(
            || {
                // Fresh changes per iteration so no iteration is a no-op
                // re-application of the previous one's rows.
                (0..DELTA_ROWS)
                    .map(|i| MappingDeltaInput {
                        hash: hash_bytes(format!("dual-domain-file-{i}").as_bytes()),
                        tag: Tag::parse(&format!("delta:t{i}")).unwrap(),
                        status: MappingDeltaStatus::Current,
                        seq: i as u64 + 1,
                        origin: None,
                    })
                    .collect::<Vec<_>>()
            },
            |changes| {
                black_box(
                    db.merge_mapping_delta(svc, "blake3", &changes, &[], DELTA_ROWS as u64, marker)
                        .unwrap(),
                )
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_merge_paths);
criterion_main!(benches);
