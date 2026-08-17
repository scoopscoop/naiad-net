//! Property-based tests (README §10) for the signing contract: a fresh
//! signature always verifies and any tampered field is rejected.

use naiad_core::{Tag, hash_bytes};
use naiad_netproto::{Account, Op, RelKind, verify, verify_relation};
use proptest::prelude::*;

/// Strategy: a valid, already-normalized tag built from safe alphabet parts.
fn arb_tag() -> impl Strategy<Value = Tag> {
    ("[a-z0-9]{1,10}", "[a-z0-9]{1,16}")
        .prop_map(|(ns, sub)| Tag::parse(&format!("{ns}:{sub}")).unwrap())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fresh_signatures_verify(seed in any::<[u8; 32]>(), t in arb_tag(), payload in any::<Vec<u8>>()) {
        let acct = Account::from_secret_bytes(&seed);
        let h = hash_bytes(&payload);
        let sub = acct.sign(Op::Add, &h, &t);
        prop_assert!(verify(&sub).is_ok());
    }

    #[test]
    fn tampered_fields_are_rejected(seed in any::<[u8; 32]>(), t in arb_tag(), payload in any::<Vec<u8>>()) {
        let acct = Account::from_secret_bytes(&seed);
        let h = hash_bytes(&payload);
        let good = acct.sign(Op::Add, &h, &t);

        // Flip the op.
        let mut bad = good.clone();
        bad.op = Op::Remove;
        prop_assert!(verify(&bad).is_err(), "flipped op must not verify");

        // Change the tag (guaranteed different: prepend "zz" to mangle the namespace name).
        let mut bad = good.clone();
        bad.tag = format!("zz{}", bad.tag);
        prop_assert!(verify(&bad).is_err(), "changed tag must not verify");

        // Change the hash.
        let mut bad = good.clone();
        bad.hash = hash_bytes(b"something else entirely").to_hex();
        prop_assert!(verify(&bad).is_err(), "changed hash must not verify");

        // Reattribute to a different account's key.
        let mut bad = good.clone();
        bad.author = Account::from_secret_bytes(&{
            let mut other = seed;
            other[0] = other[0].wrapping_add(1);
            other
        }).public_hex();
        prop_assert!(verify(&bad).is_err(), "reattributed author must not verify");
    }

    #[test]
    fn relation_signatures_verify(
        seed in any::<[u8; 32]>(),
        a in arb_tag(),
        b in arb_tag(),
    ) {
        prop_assume!(a != b);
        let acct = Account::from_secret_bytes(&seed);
        let sub = acct.sign_relation(Op::Add, RelKind::Sibling, &a, &b);
        prop_assert!(verify_relation(&sub).is_ok());
    }
}
