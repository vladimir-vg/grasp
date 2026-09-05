//! The `DynValue` invariants from `docs/design/mapping.md`.
//!
//! Each of these fails *silently* if violated — producing wrong query results
//! or rows that never consolidate, rather than a panic — which is why they are
//! tested rather than merely documented.

use dbsp_runner::value::DynValue;
use proptest::prelude::*;
use rkyv::Deserialize;
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Serialize, then compare in archived form.
fn archived_cmp(a: &DynValue, b: &DynValue) -> Ordering {
    let ba = rkyv::to_bytes::<_, 4096>(a).expect("serialize a");
    let bb = rkyv::to_bytes::<_, 4096>(b).expect("serialize b");
    let aa = unsafe { rkyv::archived_root::<DynValue>(&ba) };
    let ab = unsafe { rkyv::archived_root::<DynValue>(&bb) };
    aa.cmp(ab)
}

fn hash_of(v: &DynValue) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Leaf values, plus one level of nesting. Recursion is what makes the archived
/// representation interesting, so records must be in the mix.
fn any_value() -> impl Strategy<Value = DynValue> {
    let leaf = prop_oneof![
        Just(DynValue::Absent),
        any::<bool>().prop_map(DynValue::Bool),
        any::<i64>().prop_map(DynValue::I64),
        any::<f64>()
            .prop_filter("NaN has no total order", |f| !f.is_nan())
            .prop_map(|f| DynValue::F64(dbsp::algebra::F64::new(f))),
        ".{0,8}".prop_map(DynValue::String),
        ".{0,8}".prop_map(|s| DynValue::str(&s)),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop::collection::vec(inner, 0..4).prop_map(DynValue::Record)
    })
}

proptest! {
    /// Invariant 1: archived ordering must equal in-memory ordering.
    ///
    /// `dbsp` sorts batches with in-memory `Ord` but compares nested archived
    /// containers element-wise in archived form. If the two disagree, container
    /// comparison stops being transitive and rkyv's `ArchivedBTreeMap` binary
    /// search breaks — with no error at the point of failure.
    #[test]
    fn archived_order_matches_in_memory_order(a in any_value(), b in any_value()) {
        prop_assert_eq!(a.cmp(&b), archived_cmp(&a, &b), "a = {:?}, b = {:?}", a, b);
    }

    /// Invariant 2: values that compare equal must hash equally.
    ///
    /// Multi-worker runs shard by `key.default_hash() % workers`. Equal-but
    /// differently-hashed keys land on different workers and never consolidate,
    /// so retractions stop cancelling insertions and the Z-set silently
    /// accumulates.
    #[test]
    fn eq_implies_equal_hash(a in any_value(), b in any_value()) {
        if a == b {
            prop_assert_eq!(hash_of(&a), hash_of(&b));
        }
    }

    /// `Ord` must be a total order — antisymmetric, and consistent with `Eq`.
    #[test]
    fn ord_is_consistent(a in any_value(), b in any_value()) {
        prop_assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
        prop_assert_eq!(a.cmp(&b) == Ordering::Equal, a == b);
    }
}

/// The float trap named in invariant 2, pinned explicitly: `0.0 == -0.0` is
/// true for `f64`, and the two have different bit patterns, so a hash derived
/// from the bits would violate the `Eq`/`Hash` contract.
#[test]
fn signed_zero_hashes_consistently() {
    let pos = DynValue::F64(dbsp::algebra::F64::new(0.0));
    let neg = DynValue::F64(dbsp::algebra::F64::new(-0.0));
    if pos == neg {
        assert_eq!(hash_of(&pos), hash_of(&neg), "0.0 and -0.0 compare equal but hash differently");
    }
}

/// A round trip through the archived form must preserve the value, or a batch
/// spilled to storage comes back as something else.
#[test]
fn archived_round_trip_preserves_values() {
    let cases = vec![
        DynValue::Absent,
        DynValue::Bool(true),
        DynValue::I64(-7),
        DynValue::F64(dbsp::algebra::F64::new(1.5)),
        DynValue::String("hello".into()),
        DynValue::str("world"),
        DynValue::record([DynValue::I64(1), DynValue::str("x"), DynValue::Absent]),
        // Nested records exercise the `#[omit_bounds]` recursion.
        DynValue::record([DynValue::record([DynValue::I64(2)]), DynValue::Bool(false)]),
    ];
    for v in cases {
        let bytes = rkyv::to_bytes::<_, 4096>(&v).expect("serialize");
        // `from_bytes` would require the archived type to derive `CheckBytes`,
        // which it does not; `dbsp` reads batches through the unchecked path too.
        let archived = unsafe { rkyv::archived_root::<DynValue>(&bytes) };
        let back: DynValue =
            archived.deserialize(&mut rkyv::Infallible).expect("deserialize");
        assert_eq!(v, back);
    }
}
