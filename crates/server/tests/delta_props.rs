//! Property-based test: delta-sync convergence. Any interleaving of signed
//! add/remove submissions yields a store whose incremental deltas — replayed
//! from zero, or resumed from a cursor captured mid-sequence — reconstruct
//! exactly the state the full snapshot reports.
//!
//! v6 plain client/server model: `DeltaMapping` is `(hash, tag, status, seq)`.
//! No supporter metadata.

use std::collections::{BTreeMap, BTreeSet};

use naiad_core::{Tag, hash_bytes};
use naiad_netproto::{Account, DeltaMapping, MappingStatus, Op};
use naiad_server::RepoStore;
use proptest::prelude::*;

const HASHES: usize = 4;
const TAGS: usize = 4;
/// The whole keyspace: every 64-char lowercase hex hash sorts in `["0"*64, "g")`.
const ALL_LO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const ALL_HI: &str = "g";

/// Fold collapsed delta rows into a client's `(hash, tag) -> status` view.
fn replay(rows: &[DeltaMapping], onto: &mut BTreeMap<(String, String), MappingStatus>) {
    for r in rows {
        onto.insert((r.hash.clone(), r.tag.clone()), r.status);
    }
}

/// The surviving `Current` keys of a replayed view.
fn current_set(state: &BTreeMap<(String, String), MappingStatus>) -> BTreeSet<(String, String)> {
    state
        .iter()
        .filter(|(_, s)| **s == MappingStatus::Current)
        .map(|(k, _)| k.clone())
        .collect()
}

/// Ground truth: the keys the full snapshot reports.
fn snapshot_set(store: &RepoStore) -> BTreeSet<(String, String)> {
    store
        .snapshot()
        .unwrap()
        .into_iter()
        .flat_map(|(hash, tags)| tags.into_iter().map(move |t| (hash.clone(), t.tag)))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn deltas_converge_on_the_snapshot(
        ops in prop::collection::vec((0..HASHES, 0..TAGS, 0..2usize, any::<bool>()), 1..=24),
        split in any::<prop::sample::Index>(),
    ) {
        let store = RepoStore::open_in_memory().unwrap();
        let accts = [
            Account::from_secret_bytes(&[1u8; 32]),
            Account::from_secret_bytes(&[2u8; 32]),
        ];
        let hashes: Vec<_> = (0..HASHES).map(|i| hash_bytes(format!("dp-{i}").as_bytes())).collect();
        let tags: Vec<_> = (0..TAGS).map(|i| Tag::parse(&format!("dp:t{i}")).unwrap()).collect();

        // Apply the first `split_at` ops, then capture the head delta + cursor —
        // this is what a client that synced early would have seen.
        let split_at = split.index(ops.len() + 1);
        for &(h, t, a, is_add) in ops.iter().take(split_at) {
            let op = if is_add { Op::Add } else { Op::Remove };
            store.apply_submission(&accts[a].sign(op, &hashes[h], &tags[t])).unwrap();
        }
        let head = store.bucket_delta(ALL_LO, ALL_HI, 0, usize::MAX).unwrap().0;
        let cursor = store.mapping_cursor().unwrap();

        // Apply the rest.
        for &(h, t, a, is_add) in ops.iter().skip(split_at) {
            let op = if is_add { Op::Add } else { Op::Remove };
            store.apply_submission(&accts[a].sign(op, &hashes[h], &tags[t])).unwrap();
        }

        let truth = snapshot_set(&store);

        // (a) A fresh client replaying the whole delta converges on the snapshot.
        let mut fresh = BTreeMap::new();
        let full_delta = store.bucket_delta(ALL_LO, ALL_HI, 0, usize::MAX).unwrap().0;
        replay(&full_delta, &mut fresh);
        prop_assert_eq!(current_set(&fresh), truth.clone());

        // (b) A resuming client (head, then tail from its cursor) converges too.
        let tail = store.bucket_delta(ALL_LO, ALL_HI, cursor, usize::MAX).unwrap().0;
        let mut resumed = BTreeMap::new();
        replay(&head, &mut resumed);
        replay(&tail, &mut resumed);
        prop_assert_eq!(current_set(&resumed), truth);
    }
}

// ── Deterministic unit cases ─────────────────────────────────────────────────

/// v6 UPSERT semantics: any Remove sets the mapping to Deleted regardless of
/// which account removes it; a later Add resurrects it (non-sticky).
#[test]
fn remove_sets_mapping_deleted_regardless_of_author() {
    let store = RepoStore::open_in_memory().unwrap();
    let a = Account::from_secret_bytes(&[1u8; 32]);
    let b = Account::from_secret_bytes(&[2u8; 32]);
    let hash = hash_bytes(b"upsert-test");
    let tag = Tag::parse("series:metroid").unwrap();

    // A adds (hash, tag).
    store
        .apply_submission(&a.sign(Op::Add, &hash, &tag))
        .unwrap();
    let c0 = store.mapping_cursor().unwrap();

    // B removes (hash, tag): in v6 UPSERT semantics, the single repo_mappings
    // row is overwritten → mapping becomes Deleted.
    store
        .apply_submission(&b.sign(Op::Remove, &hash, &tag))
        .unwrap();

    let delta = store
        .bucket_delta(ALL_LO, ALL_HI, c0, usize::MAX)
        .unwrap()
        .0;
    assert_eq!(delta.len(), 1, "exactly one touched key");
    let d = &delta[0];
    assert_eq!(
        d.status,
        MappingStatus::Deleted,
        "v6: any Remove sets the mapping to Deleted"
    );
    assert!(d.seq > c0, "seq bumped above prior cursor");
    assert!(
        store.snapshot().unwrap().is_empty(),
        "mapping absent from snapshot"
    );
}

/// A Remove followed by a later Add resurects the mapping (non-sticky delete).
#[test]
fn add_after_remove_resurrects_mapping() {
    let store = RepoStore::open_in_memory().unwrap();
    let a = Account::from_secret_bytes(&[1u8; 32]);
    let hash = hash_bytes(b"resurrect-test");
    let tag = Tag::parse("series:zelda").unwrap();

    store
        .apply_submission(&a.sign(Op::Add, &hash, &tag))
        .unwrap();
    store
        .apply_submission(&a.sign(Op::Remove, &hash, &tag))
        .unwrap();

    let c1 = store.mapping_cursor().unwrap();
    assert!(
        store.snapshot().unwrap().is_empty(),
        "mapping deleted before re-add"
    );

    // Re-add: the UPSERT sets status back to Current (non-sticky).
    store
        .apply_submission(&a.sign(Op::Add, &hash, &tag))
        .unwrap();

    let delta = store
        .bucket_delta(ALL_LO, ALL_HI, c1, usize::MAX)
        .unwrap()
        .0;
    assert_eq!(delta.len(), 1, "exactly one change after re-add");
    assert_eq!(
        delta[0].status,
        MappingStatus::Current,
        "mapping resurrected"
    );
    assert!(delta[0].seq > c1, "seq bumped");
    assert!(
        !store.snapshot().unwrap().is_empty(),
        "mapping visible in snapshot after re-add"
    );
}

/// Seq is strictly monotone across submits: each new entry gets a higher seq
/// than the previous. Clients use this as their sync cursor.
#[test]
fn seq_is_strictly_monotone() {
    let store = RepoStore::open_in_memory().unwrap();
    let a = Account::from_secret_bytes(&[3u8; 32]);
    let hashes: Vec<_> = (0..4)
        .map(|i| hash_bytes(format!("mono-{i}").as_bytes()))
        .collect();
    let tag = Tag::parse("a:x").unwrap();

    let mut prev_cursor = 0u64;
    for h in &hashes {
        store.apply_submission(&a.sign(Op::Add, h, &tag)).unwrap();
        let cursor = store.mapping_cursor().unwrap();
        assert!(
            cursor > prev_cursor,
            "cursor must advance on each distinct submit"
        );
        prev_cursor = cursor;
    }
}
