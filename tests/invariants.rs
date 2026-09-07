//! The `DynValue` invariants from `docs/design/mapping.md`.
//!
//! Each of these fails *silently* if violated — producing wrong query results
//! or rows that never consolidate, rather than a panic — which is why they are
//! tested rather than merely documented.

use dbsp_runner::json::{decode_value, encode_value};
use dbsp_runner::value::{DynValue, TypeDesc};
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

/// Leaf values, plus nesting. Recursion is what makes the archived
/// representation interesting, so the two containers whose `Ord`, `Hash` and
/// archived ordering we own — `Record` and `Array` — must both be in the mix.
/// `Json` is here for the opposite reason: `FlatVariant` is supposed to satisfy
/// these by construction, so a failure would mean it does not.
fn any_value() -> impl Strategy<Value = DynValue> {
    let leaf = prop_oneof![
        Just(DynValue::None),
        any::<bool>().prop_map(DynValue::Bool),
        any::<i64>().prop_map(DynValue::I64),
        any::<f64>()
            .prop_filter("NaN has no total order", |f| !f.is_nan())
            .prop_map(|f| DynValue::F64(dbsp::algebra::F64::new(f))),
        ".{0,8}".prop_map(DynValue::String),
        any_json().prop_map(json_value),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(DynValue::Record),
            prop::collection::vec(inner, 0..4).prop_map(DynValue::Array),
        ]
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
        DynValue::None,
        DynValue::Bool(true),
        DynValue::I64(-7),
        DynValue::F64(dbsp::algebra::F64::new(1.5)),
        DynValue::String("hello".into()),
        DynValue::str("world"),
        DynValue::record([DynValue::I64(1), DynValue::str("x"), DynValue::None]),
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

// ---------------------------------------------------------------------------
// JSON round trip
// ---------------------------------------------------------------------------
//
// A declared type is a promise the runtime cannot break, and the JSON codec is
// where a broken promise surfaces: `encode_value` refuses to write a `null` into
// a column that is not optional, and `decode_value` refuses to read one. If the
// two ever disagree, output stops being re-ingestible — which is what happens
// when an operator produces a value its own `TypeDesc` forbids.

/// An arbitrary JSON document, as `serde_json` sees it.
fn any_json() -> impl Strategy<Value = serde_json::Value> {
    use serde_json::Value as J;
    let leaf = prop_oneof![
        Just(J::Null),
        any::<bool>().prop_map(J::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        any::<f64>()
            .prop_filter("JSON has no NaN or infinity", |f| f.is_finite())
            .prop_map(|f| serde_json::json!(f)),
        ".{0,6}".prop_map(J::String),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(J::Array),
            prop::collection::vec(("[a-z]{1,3}", inner), 0..3)
                .prop_map(|kv| J::Object(kv.into_iter().collect())),
        ]
    })
}

/// The canonical `FlatVariant` for a document — map keys sorted and
/// deduplicated, which is what makes two spellings of one object a single
/// Z-set key.
fn json_value(v: serde_json::Value) -> DynValue {
    DynValue::Json(serde_json::from_value(v).expect("a document is always encodable"))
}

/// A type, respecting `TypeDesc::Optional`'s invariant that `T` is never itself
/// optional. Field names are positional so a generated record cannot collide
/// with itself.
fn any_type() -> impl Strategy<Value = TypeDesc> {
    let leaf = prop_oneof![
        Just(TypeDesc::Bool),
        Just(TypeDesc::I64),
        Just(TypeDesc::F64),
        Just(TypeDesc::String),
    ];
    let core = leaf.prop_recursive(3, 12, 3, |inner| {
        let inner2 = inner.clone();
        let field = prop_oneof![
            inner.clone(),
            inner.prop_map(|t| TypeDesc::Optional(Box::new(t))),
            // `optional(json)` is disallowed, so `json` only ever appears
            // unwrapped — a document carries its own null.
            Just(TypeDesc::Json),
        ];
        prop_oneof![
            prop::collection::vec(field, 1..4).prop_map(|ts| {
                TypeDesc::record(ts.into_iter().enumerate().map(|(i, t)| (format!("f{i}"), t)))
            }),
            inner2.prop_map(|t| TypeDesc::Array(Box::new(t))),
        ]
    });
    prop_oneof![
        core.prop_flat_map(|t| {
            prop_oneof![Just(t.clone()), Just(TypeDesc::Optional(Box::new(t)))]
        }),
        Just(TypeDesc::Json),
    ]
}

/// A value inhabiting `ty`. Non-finite floats are excluded deliberately: JSON
/// has no NaN or infinity, so those are the one case the codec rejects rather
/// than round-trips, and `non_finite_floats_are_rejected` pins that separately.
fn value_of(ty: TypeDesc) -> BoxedStrategy<DynValue> {
    match ty {
        TypeDesc::Bool => any::<bool>().prop_map(DynValue::Bool).boxed(),
        TypeDesc::I64 => any::<i64>().prop_map(DynValue::I64).boxed(),
        TypeDesc::F64 => any::<f64>()
            .prop_filter("JSON has no NaN or infinity", |f| f.is_finite())
            .prop_map(|f| DynValue::F64(dbsp::algebra::F64::new(f)))
            .boxed(),
        TypeDesc::String => ".{0,8}".prop_map(DynValue::String).boxed(),
        TypeDesc::Optional(inner) => {
            prop_oneof![Just(DynValue::None), value_of(*inner)].boxed()
        }
        TypeDesc::Record(fields) => fields
            .into_iter()
            .map(|(_, t)| value_of(t))
            .collect::<Vec<_>>()
            .prop_map(DynValue::Record)
            .boxed(),
        TypeDesc::Array(elem) => prop::collection::vec(value_of(*elem), 0..4)
            .prop_map(DynValue::Array)
            .boxed(),
        TypeDesc::Json => any_json().prop_map(json_value).boxed(),
    }
}

fn any_typed_value() -> impl Strategy<Value = (TypeDesc, DynValue)> {
    any_type().prop_flat_map(|ty| (Just(ty.clone()), value_of(ty)))
}

proptest! {
    /// Encoding and decoding are inverses, through the text form the runner
    /// actually emits. A value an operator can produce must therefore be one
    /// the codec can write *and* read back at the same type.
    #[test]
    fn json_round_trip_preserves_values((ty, v) in any_typed_value()) {
        let json = encode_value(&v, &ty)
            .unwrap_or_else(|e| panic!("encoding {v:?} as `{ty}` failed: {e}"));
        let text = serde_json::to_string(&json).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse");
        let back = decode_value(&parsed, &ty)
            .unwrap_or_else(|e| panic!("decoding `{text}` as `{ty}` failed: {e}"));
        prop_assert_eq!(v, back, "type = {}", ty);
    }
}

/// The two ways a value can fail its own type, pinned explicitly. Both used to
/// encode silently as `null`, producing output this codec would then refuse to
/// read back.
#[test]
fn a_value_that_contradicts_its_type_is_refused() {
    assert!(encode_value(&DynValue::None, &TypeDesc::I64).is_err(), "NONE in a definite column");
    assert!(
        encode_value(&DynValue::None, &TypeDesc::Optional(Box::new(TypeDesc::I64))).is_ok(),
        "NONE is fine where the type allows it"
    );
}

#[test]
fn non_finite_floats_are_rejected() {
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let v = DynValue::F64(dbsp::algebra::F64::new(f));
        assert!(encode_value(&v, &TypeDesc::F64).is_err(), "{f} has no JSON form");
    }
}

// ---------------------------------------------------------------------------
// Absence, JSON null, and a missing key
// ---------------------------------------------------------------------------
//
// Three different things, and the codec has to keep them apart. These follow
// Feldera: a column's nullability decides what an omitted column and a bare
// `null` mean, and a `VARIANT NOT NULL` — which is what every `json` column is,
// since `optional(json)` is disallowed — takes `null` as a *value*.

fn record_type(fields: &[(&str, TypeDesc)]) -> TypeDesc {
    TypeDesc::record(fields.iter().map(|(n, t)| (n.to_string(), t.clone())))
}

#[test]
fn an_omitted_column_is_absence_not_a_json_value() {
    let ty = record_type(&[("payload", TypeDesc::Json)]);
    let err = decode_value(&serde_json::json!({}), &ty).expect_err("a json column is not optional");
    assert!(
        err.0.contains("is missing"),
        "an omitted `json` column is absence, which `json` does not admit: {}",
        err.0
    );
}

#[test]
fn an_explicit_null_in_a_json_column_is_a_value() {
    let ty = record_type(&[("payload", TypeDesc::Json)]);
    let decoded = decode_value(&serde_json::json!({ "payload": null }), &ty).expect("json null");
    let json = serde_json::to_string(&encode_value(&decoded, &ty).expect("encodes")).unwrap();
    assert_eq!(json, r#"{"payload":null}"#, "JSON null survives a round trip");
}

#[test]
fn an_omitted_optional_column_is_still_none() {
    // The guard on the change that made the two cases above distinguishable:
    // reading a missing key as `null` used to collapse them, and every other
    // type must keep behaving exactly as it did.
    let ty = record_type(&[("v", TypeDesc::Optional(Box::new(TypeDesc::I64)))]);
    for j in [serde_json::json!({}), serde_json::json!({ "v": null })] {
        assert_eq!(
            decode_value(&j, &ty).expect("optional accepts both"),
            DynValue::Record(vec![DynValue::None]),
            "omitted and explicit null both read as absence for an optional column"
        );
    }
    let definite = record_type(&[("v", TypeDesc::I64)]);
    assert!(decode_value(&serde_json::json!({}), &definite).is_err(), "and neither for a definite one");
}
