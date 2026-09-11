//! The `DynValue` invariants from `docs/grasp-dbsp/mapping.md`.
//!
//! Each of these fails *silently* if violated — producing wrong query results
//! or rows that never consolidate, rather than a panic — which is why they are
//! tested rather than merely documented.

use dbsp::default_hash;
use grasp_dbsp_runner::json::{decode_value, encode_value};
use grasp_dbsp_runner::value::{DynValue, TypeDesc};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use rkyv::Deserialize;
use size_of::SizeOf;
use std::cmp::Ordering;

/// Serialize, then compare in archived form.
fn archived_cmp(a: &DynValue, b: &DynValue) -> Ordering {
    let ba = rkyv::to_bytes::<_, 4096>(a).expect("serialize a");
    let bb = rkyv::to_bytes::<_, 4096>(b).expect("serialize b");
    let aa = unsafe { rkyv::archived_root::<DynValue>(&ba) };
    let ab = unsafe { rkyv::archived_root::<DynValue>(&bb) };
    aa.cmp(ab)
}

/// The hash that decides which worker a key lands on.
///
/// `dbsp`'s own, not `std`'s: sharding is `key.default_hash() % workers`
/// (`dbsp/src/operator/dynamic/communication/shard.rs:454`), and
/// `default_hash` is xxh3 (`dbsp/src/hash.rs:7-11`). Both hashers consume the
/// same `Hash` impl, so today they agree about almost everything — but a
/// `Hash` that fed a streaming hasher ambiguously, writing a container with no
/// length separator so that `["ab"]` and `["a", "b"]` produce one byte stream,
/// is a question about *this* function and not about `std`'s.
fn hash_of(v: &DynValue) -> u64 {
    default_hash(v)
}

/// Leaf values, plus nesting. Recursion is what makes the archived
/// representation interesting, so the three containers whose `Ord`, `Hash` and
/// archived ordering we own — `Record`, `Array` and `Dict` — must all be in the
/// mix. `Json` is here for the opposite reason: `FlatVariant` is supposed to
/// satisfy these by construction, so a failure would mean it does not.
///
/// **Every variant, and that is the point.** These properties are what the
/// append-only discipline in `mapping.md` is *for*: the variant order is the
/// archived discriminant, so a variant this strategy never draws is a variant
/// whose archived order has never been compared with its in-memory one. The
/// six appended after the containers each carry a foreign payload —
/// `ByteArray`, `ShortInterval`, `FlatVariant` and the three temporal
/// newtypes — so agreement there is borrowed rather than derived, which is
/// exactly the case worth drawing. [`every_variant_is_generated`] holds this
/// to it, and its match stops compiling when a variant is appended.
///
/// `Dict` keys here are arbitrary values, not the scalars the *type system*
/// admits: `DynValue` can hold a composite key structurally, and ordering
/// safety should be exercised over everything the representation can hold
/// rather than only what the checker will build.
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
        any_date(),
        any_time(),
        any_timestamp(),
        any_interval(),
        any_bytes(),
        any_dynamic(),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(DynValue::Record),
            prop::collection::vec(inner.clone(), 0..4).prop_map(DynValue::Array),
            prop::collection::vec((inner.clone(), inner), 0..4)
                .prop_map(|kvs| DynValue::Dict(kvs.into_iter().collect())),
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

/// A value reports what it weighs, and `dbsp` spills on the answer.
///
/// The fifth load-bearing property, and the newest: `approximate_byte_size`
/// samples this number to decide whether a batch stays in memory or goes to a
/// file (`dbsp/src/trace.rs:568-579`). It failed silently in the same way the
/// others do — a relation of records reporting a constant would simply never
/// spill, however large it grew — which is why it is a test and not a comment.
#[test]
fn a_value_reports_what_it_holds() {
    let kib = "x".repeat(1024);
    let held = 10 * 1024;
    let record = DynValue::record((0..10).map(|_| DynValue::str(&kib)));
    let bare = std::mem::size_of::<DynValue>();

    let size = record.size_of().total_bytes();
    assert!(
        size >= held,
        "a record of ten kilobytes reports {size}, less than the {held} it holds"
    );
    assert!(
        size < held * 2,
        "a record of ten kilobytes reports {size}, far more than the {held} it holds"
    );

    // The same payload under an array and under a dict weighs the same, give or
    // take each container's own allocation.
    let array = DynValue::Array(vec![record.clone()]);
    let dict = DynValue::Dict([(DynValue::str("k"), record.clone())].into_iter().collect());
    for (what, v) in [("an array", &array), ("a dict", &dict)] {
        let nested = v.size_of().total_bytes();
        assert!(
            nested >= size && nested < size + 1024,
            "{what} holding that record reports {nested}, against the record's own {size}"
        );
    }

    // An empty container owns nothing, and a scalar owns nothing either.
    for empty in [
        DynValue::record([]),
        DynValue::Array(Vec::new()),
        DynValue::Dict(std::collections::BTreeMap::new()),
        DynValue::I64(1),
    ] {
        let size = empty.size_of().total_bytes();
        assert!(
            size <= bare,
            "{empty:?} reports {size}, more than the {bare} the enum itself is"
        );
    }

    // A document's buffer is an `Arc`, and two values sharing one count it
    // once — which is the property `FlatVariant` was chosen for.
    let doc = json_value(serde_json::json!({"k": kib}));
    let one = DynValue::Array(vec![doc.clone()]).size_of().total_bytes();
    let two = DynValue::Array(vec![doc.clone(), doc])
        .size_of()
        .total_bytes();
    assert!(
        two < one + 1024,
        "two values sharing one document buffer report {two} against one value's {one}, \
         so the buffer was counted twice"
    );
}

/// Every variant either owns nothing on the heap, or grows when its payload
/// does.
///
/// The previous test measures one shape; this one measures all of them, and is
/// what an appended variant runs into: `owns_an_allocation` is exhaustive, so
/// the file stops compiling until the new variant says which it is, and the
/// count at the end stops matching until it joins one of the two lists.
#[test]
fn every_variant_that_owns_something_reports_it() {
    fn owns_an_allocation(v: &DynValue) -> bool {
        match v {
            DynValue::None
            | DynValue::Bool(_)
            | DynValue::I64(_)
            | DynValue::F64(_)
            | DynValue::Date(_)
            | DynValue::Time(_)
            | DynValue::Timestamp(_)
            | DynValue::Interval(_) => false,
            DynValue::String(_)
            | DynValue::Bytes(_)
            | DynValue::Json(_)
            | DynValue::Dynamic(_)
            | DynValue::Record(_)
            | DynValue::Array(_)
            | DynValue::Dict(_) => true,
        }
    }

    const PAD: usize = 4096;
    let pad = "x".repeat(PAD);
    let dynamic = |s: &str| {
        DynValue::Dynamic(feldera_sqllib::FlatVariant::from(
            feldera_sqllib::Variant::String(feldera_sqllib::SqlString::from_ref(s)),
        ))
    };

    // One pair per variant that owns something: the same shape, `PAD` bytes
    // apart.
    let pairs = [
        (DynValue::str(""), DynValue::str(&pad)),
        (
            DynValue::Bytes(feldera_sqllib::ByteArray::from_vec(Vec::new())),
            DynValue::Bytes(feldera_sqllib::ByteArray::from_vec(vec![0; PAD])),
        ),
        (
            json_value(serde_json::json!("")),
            json_value(serde_json::json!(pad)),
        ),
        (dynamic(""), dynamic(&pad)),
        (
            DynValue::record([]),
            DynValue::record([DynValue::str(&pad)]),
        ),
        (
            DynValue::Array(Vec::new()),
            DynValue::Array(vec![DynValue::str(&pad)]),
        ),
        (
            DynValue::Dict(std::collections::BTreeMap::new()),
            DynValue::Dict(
                [(DynValue::str("k"), DynValue::str(&pad))]
                    .into_iter()
                    .collect(),
            ),
        ),
    ];
    for (small, large) in &pairs {
        assert!(owns_an_allocation(small), "{small:?} is in the wrong list");
        let grew = large.size_of().total_bytes() - small.size_of().total_bytes();
        assert!(
            grew >= PAD,
            "{large:?} carries {PAD} bytes more than {small:?} and reports only {grew} more; \
             `dbsp` spills on this number, so what it cannot see it never writes out"
        );
    }

    // And one per variant that owns nothing, which cannot grow at all.
    let scalars = [
        DynValue::None,
        DynValue::Bool(true),
        DynValue::I64(1),
        DynValue::F64(dbsp::algebra::F64::new(1.0)),
        DynValue::Date(feldera_sqllib::make_date___(2024, 1, 15).expect("valid")),
        grasp_dbsp_runner::value::parse_time("14:30:00").expect("a valid time"),
        DynValue::Timestamp(feldera_sqllib::Timestamp::from_microseconds(1)),
        DynValue::Interval(feldera_sqllib::ShortInterval::from_microseconds(1)),
    ];
    let bare = std::mem::size_of::<DynValue>();
    for v in &scalars {
        assert!(!owns_an_allocation(v), "{v:?} is in the wrong list");
        assert_eq!(
            v.size_of().total_bytes(),
            bare,
            "{v:?} owns nothing, so it weighs what the enum weighs"
        );
    }

    assert_eq!(
        pairs.len() + scalars.len(),
        15,
        "`DynValue` has a variant in neither list"
    );
}

/// Invariant 3: the hash is stable across builds and processes.
///
/// It is what places a key on a worker, so a change moves every key at once —
/// and once anything is checkpointed it is also what a restore is matched
/// against, which is why `dbsp` pins its own the same way
/// (`dbsp/src/dynamic/data.rs:108-109`). Nothing else here would notice: a
/// reordered variant, or a `Hash` that grew a field, keeps every other
/// property in this file true.
///
/// The numbers were read out of this test's first failure, not derived. One
/// value per variant, so a reordering shows up as a whole column moving.
#[test]
fn the_shard_hash_is_pinned() {
    let cases: Vec<(DynValue, u64)> = vec![
        (DynValue::None, 14374147212387527897),
        (DynValue::Bool(true), 10971357638593500668),
        (DynValue::I64(1), 18110323628208699528),
        (
            DynValue::F64(dbsp::algebra::F64::new(0.0)),
            12217256802940554018,
        ),
        (DynValue::str("a"), 13175042669753105890),
        (
            DynValue::record([DynValue::I64(1), DynValue::None]),
            322651925485280935,
        ),
        (json_value(serde_json::json!({"a": 1})), 9149952449185460157),
        (
            DynValue::Array(vec![DynValue::I64(1)]),
            17433800960406894783,
        ),
        (
            DynValue::Dict(
                [(DynValue::str("a"), DynValue::I64(1))]
                    .into_iter()
                    .collect(),
            ),
            10267951854747464872,
        ),
        (
            DynValue::Date(feldera_sqllib::make_date___(2024, 1, 15).expect("a valid date")),
            15361640281678058060,
        ),
        (
            grasp_dbsp_runner::value::parse_time("14:30:00").expect("a valid time"),
            8576659017415314607,
        ),
        (
            DynValue::Timestamp(feldera_sqllib::Timestamp::from_microseconds(1)),
            16617346159300649605,
        ),
        (
            DynValue::Interval(feldera_sqllib::ShortInterval::from_microseconds(1)),
            8391286410845269142,
        ),
        (
            DynValue::Bytes(feldera_sqllib::ByteArray::from_vec(vec![1, 2])),
            3586656741048657233,
        ),
        (
            DynValue::Dynamic(feldera_sqllib::FlatVariant::from(
                feldera_sqllib::Variant::BigInt(1),
            )),
            16185041875707032582,
        ),
    ];
    let got: Vec<u64> = cases.iter().map(|(v, _)| default_hash(v)).collect();
    let want: Vec<u64> = cases.iter().map(|(_, w)| *w).collect();
    assert_eq!(
        got, want,
        "the hash that places a key on a worker changed; every key moves, and \
         every checkpoint written against the old one is unreadable"
    );
}

/// The float trap named in invariant 2, pinned explicitly: `0.0 == -0.0` is
/// true for `f64`, and the two have different bit patterns, so a hash derived
/// from the bits would violate the `Eq`/`Hash` contract.
#[test]
fn signed_zero_hashes_consistently() {
    let pos = DynValue::F64(dbsp::algebra::F64::new(0.0));
    let neg = DynValue::F64(dbsp::algebra::F64::new(-0.0));
    if pos == neg {
        assert_eq!(
            hash_of(&pos),
            hash_of(&neg),
            "0.0 and -0.0 compare equal but hash differently"
        );
    }
}

proptest! {
    /// A round trip through the archived form must preserve the value, or a
    /// batch spilled to storage comes back as something else.
    ///
    /// Over the generator rather than a hand-written list, which is how this
    /// reaches the variants a list forgets — it held eight values, none of them
    /// a `Dict` and none of the six appended after it. Nesting comes with it,
    /// and nesting is what exercises the `#[omit_bounds]` recursion.
    ///
    /// **Read back with `dbsp`'s own deserializer**, not `rkyv::Infallible`.
    /// That is not a detail: `ShortInterval`'s hand-written `Deserialize`
    /// downcasts to `dbsp::storage::file::Deserializer` to read the storage
    /// format version — the representation changed from milliseconds to
    /// microseconds at version 4 — and *panics* given anything else. So an
    /// `interval` has exactly one path back out of the archived form, and it is
    /// the one `dbsp` takes for a spilled batch. The old list never held one,
    /// so the test read every value through a deserializer no batch uses.
    #[test]
    fn archived_round_trip_preserves_values(v in any_value()) {
        let bytes = rkyv::to_bytes::<_, 4096>(&v).expect("serialize");
        // `from_bytes` would require the archived type to derive `CheckBytes`,
        // which it does not; `dbsp` reads batches through the unchecked path too.
        let archived = unsafe { rkyv::archived_root::<DynValue>(&bytes) };
        let back: DynValue = archived
            .deserialize(&mut dbsp::storage::file::Deserializer::default())
            .expect("deserialize");
        prop_assert_eq!(v, back);
    }
}

/// Every `DynValue` variant is one [`any_value`] actually draws.
///
/// The properties above are only as wide as the strategy feeding them, and a
/// variant it never draws is a variant whose archived order has never been
/// compared with its in-memory one — which is the whole discipline
/// `mapping.md` asks for. This is what stops that from being a matter of
/// remembering.
///
/// Two things have to change before it passes again when a variant is appended:
/// the match below stops compiling until the variant has an arm, and the set
/// comparison fails until the strategy draws it. Neither can be satisfied by
/// editing a list.
#[test]
fn every_variant_is_generated() {
    fn walk(v: &DynValue, seen: &mut std::collections::BTreeSet<&'static str>) {
        let name = match v {
            DynValue::None => "None",
            DynValue::Bool(_) => "Bool",
            DynValue::I64(_) => "I64",
            DynValue::F64(_) => "F64",
            DynValue::String(_) => "String",
            DynValue::Json(_) => "Json",
            DynValue::Date(_) => "Date",
            DynValue::Time(_) => "Time",
            DynValue::Timestamp(_) => "Timestamp",
            DynValue::Interval(_) => "Interval",
            DynValue::Bytes(_) => "Bytes",
            DynValue::Dynamic(_) => "Dynamic",
            DynValue::Record(vs) => {
                vs.iter().for_each(|x| walk(x, seen));
                "Record"
            }
            DynValue::Array(vs) => {
                vs.iter().for_each(|x| walk(x, seen));
                "Array"
            }
            DynValue::Dict(kvs) => {
                kvs.iter().for_each(|(k, v)| {
                    walk(k, seen);
                    walk(v, seen);
                });
                "Dict"
            }
        };
        seen.insert(name);
    }

    let every: std::collections::BTreeSet<&'static str> = [
        "None",
        "Bool",
        "I64",
        "F64",
        "String",
        "Record",
        "Json",
        "Array",
        "Dict",
        "Date",
        "Time",
        "Timestamp",
        "Interval",
        "Bytes",
        "Dynamic",
    ]
    .into_iter()
    .collect();

    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let strategy = any_value();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..2000 {
        let tree = strategy.new_tree(&mut runner).expect("a value");
        walk(&tree.current(), &mut seen);
    }
    assert_eq!(
        seen, every,
        "`any_value` draws a different set of variants than `DynValue` has; \
         a variant it never draws is never held to the invariants above"
    );
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
            Just(TypeDesc::Json),
            Just(TypeDesc::Optional(Box::new(TypeDesc::Json))),
        ];
        prop_oneof![
            // From zero: `record()` is a legal type, and the empty case is the
            // one a hand-written fixture is least likely to reach.
            prop::collection::vec(field, 0..4).prop_map(|ts| {
                TypeDesc::record(
                    ts.into_iter()
                        .enumerate()
                        .map(|(i, t)| (format!("f{i}"), t)),
                )
            }),
            inner2.clone().prop_map(|t| TypeDesc::Array(Box::new(t))),
            // Keys are scalars, which is what `TypeDesc::is_dict_key` admits and
            // what has a JSON object-key spelling to round-trip through.
            (
                prop_oneof![
                    Just(TypeDesc::Bool),
                    Just(TypeDesc::I64),
                    Just(TypeDesc::F64),
                    Just(TypeDesc::String)
                ],
                inner2,
            )
                .prop_map(|(k, v)| TypeDesc::Dict(Box::new(k), Box::new(v))),
        ]
    });
    prop_oneof![
        core.prop_flat_map(|t| {
            prop_oneof![Just(t.clone()), Just(TypeDesc::Optional(Box::new(t)))]
        }),
        Just(TypeDesc::Json),
        Just(TypeDesc::Optional(Box::new(TypeDesc::Json))),
        Just(TypeDesc::Date),
        Just(TypeDesc::Time),
        Just(TypeDesc::Timestamp),
        Just(TypeDesc::Interval),
        Just(TypeDesc::Bytes),
        Just(TypeDesc::Dynamic),
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
        // A *bare* null document is excluded under `optional(json)`: it writes
        // `null`, which reads back as absence, so it does not round-trip there.
        // `optional_json_null_degrades_to_absence` pins that rather than leaving
        // it implied by this filter. Nested nulls are unaffected.
        TypeDesc::Optional(inner) if *inner == TypeDesc::Json => prop_oneof![
            Just(DynValue::None),
            any_json()
                .prop_filter("a bare null document degrades to absence", |v| !v.is_null())
                .prop_map(json_value),
        ]
        .boxed(),
        TypeDesc::Dict(k, v) => prop::collection::vec((value_of(*k), value_of(*v)), 0..4)
            .prop_map(|kvs| DynValue::Dict(kvs.into_iter().collect()))
            .boxed(),
        TypeDesc::Optional(inner) => prop_oneof![Just(DynValue::None), value_of(*inner)].boxed(),
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
        TypeDesc::Date => any_date().boxed(),
        TypeDesc::Time => any_time().boxed(),
        TypeDesc::Dynamic => any_dynamic().boxed(),
        TypeDesc::Bytes => any_bytes().boxed(),
        TypeDesc::Interval => any_interval().boxed(),
        TypeDesc::Timestamp => any_timestamp().boxed(),
    }
}

// The six variants appended after the containers, each as one strategy both
// `any_value` and `value_of` draw from. Two lists would drift, and the way they
// would drift is silently: a variant missing from one of them is a variant that
// simply never appears in that half of the suite.

/// Inside the range the written form can spell: `make_date___` refuses a year
/// outside `1..=9999`, and the round trip is over what a program can write.
fn any_date() -> impl Strategy<Value = DynValue> {
    (1i32..=9999, 1i32..=12, 1i32..=28)
        .prop_map(|(y, m, d)| DynValue::Date(feldera_sqllib::make_date___(y, m, d).expect("valid")))
}

/// A point in one day, to microsecond resolution.
fn any_time() -> impl Strategy<Value = DynValue> {
    (0i64..86_400_000_000).prop_map(|micros| {
        let (h, m) = (micros / 3_600_000_000, micros / 60_000_000 % 60);
        let (s, us) = (micros / 1_000_000 % 60, micros % 1_000_000);
        grasp_dbsp_runner::value::parse_time(&format!("{h:02}:{m:02}:{s:02}.{us:06}"))
            .expect("a valid time")
    })
}

/// Bounded by what `Display` can write: `Timestamp` prints a calendar date, and
/// the parser reads years `1..=9999` back.
fn any_timestamp() -> impl Strategy<Value = DynValue> {
    (-62_135_596_800_000_000i64..=253_402_300_799_000_000).prop_map(|micros| {
        DynValue::Timestamp(feldera_sqllib::Timestamp::from_microseconds(micros))
    })
}

/// Bounded well inside `i64`, so that the sum in an addition test cannot
/// overflow — the round trip itself holds for any value.
fn any_interval() -> impl Strategy<Value = DynValue> {
    (-1_000_000_000_000_000i64..=1_000_000_000_000_000).prop_map(|micros| {
        DynValue::Interval(feldera_sqllib::ShortInterval::from_microseconds(micros))
    })
}

/// A payload of any length, empty included — `ByteArray` orders
/// lexicographically, so the prefix pairs are the interesting ones.
fn any_bytes() -> impl Strategy<Value = DynValue> {
    prop::collection::vec(any::<u8>(), 0..8)
        .prop_map(|b| DynValue::Bytes(feldera_sqllib::ByteArray::from_vec(b)))
}

/// Tagged the way `to_dynamic` tags them, so this is what a program could
/// actually put in one — and the tags are what the ordering sorts by first.
fn any_dynamic() -> impl Strategy<Value = DynValue> {
    prop_oneof![
        any::<bool>().prop_map(feldera_sqllib::Variant::Boolean),
        any::<i64>().prop_map(feldera_sqllib::Variant::BigInt),
        ".{0,8}"
            .prop_map(|s| feldera_sqllib::Variant::String(feldera_sqllib::SqlString::from_ref(&s))),
        (1i32..=9999, 1i32..=12, 1i32..=28).prop_map(|(y, m, d)| {
            feldera_sqllib::Variant::Date(feldera_sqllib::make_date___(y, m, d).expect("valid"))
        }),
        prop::collection::vec(any::<u8>(), 0..8).prop_map(|b| {
            feldera_sqllib::Variant::Binary(feldera_sqllib::ByteArray::from_vec(b))
        }),
    ]
    .prop_map(|v| DynValue::Dynamic(feldera_sqllib::FlatVariant::from(v)))
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
    assert!(
        encode_value(&DynValue::None, &TypeDesc::I64).is_err(),
        "NONE in a definite column"
    );
    assert!(
        encode_value(
            &DynValue::None,
            &TypeDesc::Optional(Box::new(TypeDesc::I64))
        )
        .is_ok(),
        "NONE is fine where the type allows it"
    );
}

/// NaN and the infinities write as `null`, as `serde_json` and therefore Feldera
/// do — and do not survive a round trip, which is why the property test above
/// filters them out.
///
/// This is not the refusal beside it. `NONE` is not an `f64`, so writing it into
/// a definite column would contradict the type; NaN *is* one that JSON cannot
/// spell.
#[test]
fn non_finite_floats_encode_as_null() {
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let v = DynValue::F64(dbsp::algebra::F64::new(f));
        let j = encode_value(&v, &TypeDesc::F64).expect("encodes");
        assert_eq!(j, serde_json::Value::Null, "{f} writes as null");
        assert!(
            decode_value(&j, &TypeDesc::F64).is_err(),
            "and does not read back into a definite f64 column"
        );
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
    assert_eq!(
        json, r#"{"payload":null}"#,
        "JSON null survives a round trip"
    );
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
    assert!(
        decode_value(&serde_json::json!({}), &definite).is_err(),
        "and neither for a definite one"
    );
}

/// A JSON null document survives in a `json` column and not in an
/// `optional(json)` one.
///
/// Both spellings of nothing write `null`, and for an optional column `null`
/// reads back as absence — Feldera's rule for a nullable `VARIANT`, and the one
/// every other optional type follows. Declaring the column definite is what
/// keeps the document, which is Feldera's `VARIANT NOT NULL`.
#[test]
fn optional_json_null_degrades_to_absence() {
    let null_doc: DynValue = json_value(serde_json::Value::Null);

    let round_trip = |v: &DynValue, ty: &TypeDesc| -> DynValue {
        let j = encode_value(v, ty).expect("encodes");
        assert_eq!(j, serde_json::Value::Null, "both write as `null`");
        decode_value(&j, ty).expect("decodes")
    };

    assert_eq!(
        round_trip(&null_doc, &TypeDesc::Json),
        null_doc,
        "a definite `json` column keeps the null document"
    );

    let opt = TypeDesc::Optional(Box::new(TypeDesc::Json));
    assert_eq!(
        round_trip(&null_doc, &opt),
        DynValue::None,
        "an `optional(json)` column reads it back as absence"
    );
    assert_eq!(
        round_trip(&DynValue::None, &opt),
        DynValue::None,
        "and absence stays absence"
    );
}
