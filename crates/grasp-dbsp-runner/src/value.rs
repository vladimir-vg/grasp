//! The runtime value model.
//!
//! Every stream in a lowered circuit carries the same Rust type, [`DynValue`].
//! The language's type system lives at the level of [`TypeDesc`], which the type
//! checker computes per node and the JSON codec reads.
//!
//! See `docs/grasp-dbsp/mapping.md` for the invariants this type must uphold.

use std::collections::BTreeMap;

use dbsp::ZWeight;
use dbsp::algebra::{AddAssignByRef, AddByRef, F64, HasZero, MulByRef};
use feldera_macros::IsNone;
use feldera_sqllib::FlatVariant;
use size_of::SizeOf;

/// The universal runtime value.
///
/// The derive set is copied from `feldera_sqllib::Variant`, which is the
/// in-tree precedent for a recursive enum that satisfies `dbsp`'s `DBData`.
/// `#[omit_bounds]` on the recursive field is what stops the generated rkyv
/// bounds from recursing infinitely.
///
/// Variant order is significant twice over: it determines `Ord` (and therefore
/// batch layout and `min`/`max` results), and it determines the archived
/// discriminant, which is a persisted storage format. **Append new variants at
/// the end.**
///
/// Absence is a variant rather than a wrapping `Option`, matching `Variant`'s
/// `SqlNull` — so there is exactly one `None`, and no `Some(None)` to
/// distinguish from it.
#[derive(
    Debug,
    Default,
    Eq,
    Ord,
    Clone,
    Hash,
    PartialEq,
    PartialOrd,
    SizeOf,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    IsNone,
)]
// `#[omit_bounds]` on the recursive field suppresses the bounds rkyv would have
// generated, so they have to be supplied here. `Vec` needs `ScratchSpace` in
// addition to `Serializer`. `Variant` carries the same pair plus the shared
// registry, which its `Arc` payloads require and ours do not.
#[archive(bound(
    serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer",
    deserialize = "__D: rkyv::Fallible"
))]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
pub enum DynValue {
    /// No value. Written `NONE` in source; encoded as JSON `null`.
    ///
    /// Being variant 0 makes it sort before every value, which is what both
    /// `min`/`max` and the language's comparison operators rely on.
    #[default]
    None,
    Bool(bool),
    I64(i64),
    F64(F64),
    String(String),
    /// `#[omit_bounds]` stops rkyv's derived bounds recursing through the
    /// element type.
    ///
    /// `#[size_of(skip, skip_bounds)]` is needed because the `SizeOf` derive
    /// would otherwise require `Vec<DynValue>: SizeOf` while proving
    /// `DynValue: SizeOf`, which the trait solver reports as an overflow.
    /// `skip_bounds` alone is not enough — the derive still emits the call — so
    /// the field is skipped entirely, exactly as `sqllib::Variant` does for its
    /// `Array` and `Map` payloads.
    ///
    /// The consequence is real: `dbsp`'s memory accounting does not see record
    /// payloads, so reported sizes under-count. A hand-written `SizeOf` impl
    /// would fix it and is the obvious follow-up.
    Record(
        #[omit_bounds]
        #[size_of(skip, skip_bounds)]
        Vec<DynValue>,
    ),
    /// `json` — a whole document, byte-encoded.
    ///
    /// **Appended, and new variants must be too**: the variant order is the
    /// archived discriminant, which is a storage format.
    ///
    /// `FlatVariant` is chosen for its invariants rather than its convenience:
    /// its archived form *is* the byte encoding, so archived and in-memory
    /// ordering cannot disagree, and `Eq`/`Hash` route through functions over
    /// the same bytes. Map entries are stored sorted and deduplicated, so two
    /// documents written with their keys in different orders are one value,
    /// one hash and one Z-set key.
    Json(
        #[omit_bounds]
        #[size_of(skip, skip_bounds)]
        FlatVariant,
    ),
    /// `array(T)` — a sequence of one element type.
    ///
    /// **Appended, and new variants must be too**: the variant order is the
    /// archived discriminant, which is a storage format.
    ///
    /// Like `Record`, this is a container whose `Ord`, `Hash` and archived
    /// ordering are ours rather than borrowed, so it is covered by the
    /// proptests in `tests/invariants.rs`. Same derive caveats as `Record`.
    Array(
        #[omit_bounds]
        #[size_of(skip, skip_bounds)]
        Vec<DynValue>,
    ),
    /// `dict(K,V)` — a key-value map.
    ///
    /// **Appended, and new variants must be too**: the variant order is the
    /// archived discriminant, which is a storage format.
    ///
    /// A `BTreeMap` rather than a sorted `Vec` of pairs so that the canonical
    /// form is structural: there is no way to build an unsorted or duplicated
    /// dict, so two dicts written with their entries in different orders are
    /// one value, one hash and one Z-set key without anything having to
    /// remember to canonicalise. `Record` gets the same property by sorting its
    /// *type*'s fields; a dict's keys are values, so it has to come from the
    /// container.
    ///
    /// This upholds invariant 1 because `ArchivedBTreeMap`'s `cmp` is
    /// `self.iter().cmp(other.iter())` — iteration order, the same lexicographic
    /// comparison `BTreeMap` itself uses. `sqllib::Variant` stores its `Map` the
    /// same way, behind an `Arc` it needs for sharing and we do not.
    ///
    /// Same derive caveats as `Record`.
    Dict(
        #[omit_bounds]
        #[size_of(skip, skip_bounds)]
        BTreeMap<DynValue, DynValue>,
    ),
}

impl DynValue {
    pub fn is_none(&self) -> bool {
        matches!(self, DynValue::None)
    }

    pub fn record(fields: impl IntoIterator<Item = DynValue>) -> Self {
        DynValue::Record(fields.into_iter().collect())
    }

    pub fn str(s: &str) -> Self {
        DynValue::String(s.to_string())
    }

    /// The fields of a record, or `None` for any other value.
    pub fn fields(&self) -> Option<&[DynValue]> {
        match self {
            DynValue::Record(f) => Some(f),
            _ => None,
        }
    }

    /// Positional field access. Field *names* are resolved to indices during
    /// type checking, so nothing here compares strings.
    pub fn field(&self, index: usize) -> Option<&DynValue> {
        self.fields()?.get(index)
    }

    /// This value as a JSON object key.
    ///
    /// A dict encodes as an object, whose keys are strings, so every key needs
    /// one spelling that [`TypeDesc::parse_dict_key`] reads back. `None` where
    /// there is none: a non-finite float, or a composite value, which
    /// [`TypeDesc::is_dict_key`] does not admit as a key type anyway.
    ///
    /// This is shared by the JSON codec and by `cast(d, json)` so the two
    /// cannot spell a key differently.
    pub fn dict_key_string(&self) -> Option<String> {
        match self {
            DynValue::String(s) => Some(s.clone()),
            DynValue::I64(n) => Some(n.to_string()),
            DynValue::Bool(b) => Some(b.to_string()),
            DynValue::F64(f) => {
                let f = f.into_inner();
                f.is_finite().then(|| f.to_string())
            }
            _ => Option::None,
        }
    }

    /// The name of this value's variant, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            DynValue::None => "NONE",
            DynValue::Bool(_) => "bool",
            DynValue::I64(_) => "i64",
            DynValue::F64(_) => "f64",
            DynValue::String(_) => "string",
            DynValue::Record(_) => "record",
            DynValue::Array(_) => "array",
            DynValue::Dict(_) => "dict",
            DynValue::Json(_) => "json",
        }
    }
}

/// The accumulator for the linear aggregators (`sum`, `avg`, `count`).
///
/// `aggregate_linear_postprocess` requires an accumulator that is `DBWeight` —
/// additive, with a zero — which [`DynValue`] cannot be: there is no sensible
/// `String + String`. Hence a separate type, which never enters a stream.
///
/// The sum is integral because floating point is excluded from the linear path:
/// fp addition is not associative, so an incrementally maintained fp sum would
/// depend on the order additions and retractions arrive in. The type checker
/// rejects a floating-point projection for `sum` and `avg`. `avg` still yields
/// `f64` — the division happens in the postprocess, not the accumulator.
///
/// `rows` is what lets a linear aggregate tell "the group summed to zero" from
/// "the group is empty" — see the aggregation section of `docs/grasp-dbsp/mapping.md`.
/// It is also `count` itself.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    SizeOf,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    IsNone,
)]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
pub struct Acc {
    pub sum: i64,
    /// Rows contributing a projection that is not `NONE`. This is `count`, and
    /// it is what separates "summed to zero" from "every row was `NONE`".
    pub rows: i64,
    /// Every row in the group, `NONE` projection or not.
    ///
    /// `dbsp` drops a group whose accumulator is zero, so without this a group
    /// of entirely `NONE` projections would sum to zero in every field and
    /// disappear rather than reporting `NONE`. Feldera's SQL compiler carries
    /// the same extra counter, for the same reason.
    pub present: i64,
}

impl Acc {
    pub fn value(v: i64) -> Acc {
        Acc {
            sum: v,
            rows: 1,
            present: 1,
        }
    }

    /// A row whose projection was `NONE`: counted by neither `sum` nor `count`,
    /// but still present, so the group does not vanish.
    pub fn none() -> Acc {
        Acc {
            sum: 0,
            rows: 0,
            present: 1,
        }
    }
}

impl HasZero for Acc {
    fn zero() -> Acc {
        Acc::default()
    }

    fn is_zero(&self) -> bool {
        self.sum == 0 && self.rows == 0 && self.present == 0
    }
}

impl AddByRef for Acc {
    fn add_by_ref(&self, other: &Acc) -> Acc {
        Acc {
            sum: self.sum.wrapping_add(other.sum),
            rows: self.rows.wrapping_add(other.rows),
            present: self.present.wrapping_add(other.present),
        }
    }
}

impl AddAssignByRef for Acc {
    fn add_assign_by_ref(&mut self, other: &Acc) {
        *self = self.add_by_ref(other);
    }
}

/// Scaling by a Z-weight is what makes the aggregate linear: a row with weight
/// `w` contributes `w` times its projection, and a retraction subtracts it.
impl MulByRef<ZWeight> for Acc {
    type Output = Acc;

    fn mul_by_ref(&self, w: &ZWeight) -> Acc {
        Acc {
            sum: self.sum.wrapping_mul(*w),
            rows: self.rows.wrapping_mul(*w),
            present: self.present.wrapping_mul(*w),
        }
    }
}

/// The accumulator for a *floating-point* `sum` or `avg`.
///
/// Floats do not take the linear path: fp addition is not associative, so an
/// incrementally maintained sum would depend on the order changes arrived in.
/// Feldera splits them the same way — `AggregateCompiler.java` chooses the
/// linear path only `if (this.linearAllowed && !this.fp())` — and routes floats
/// through a fold that replays the group in cursor order, which is
/// deterministic.
///
/// Only two counters here, unlike [`Acc`]: the fold is told whether the group
/// is empty, so it needs no third to keep an all-absent group from vanishing.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    SizeOf,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
    IsNone,
)]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
pub struct FpAcc {
    pub sum: F64,
    /// Rows contributing a projection that is not `NONE`.
    pub rows: i64,
}

/// Combines partial folds, which `dbsp` may compute over subsets.
#[derive(Clone)]
pub struct FpAccSemigroup;

impl dbsp::algebra::Semigroup<FpAcc> for FpAccSemigroup {
    fn combine(left: &FpAcc, right: &FpAcc) -> FpAcc {
        FpAcc {
            sum: left.sum + right.sum,
            rows: left.rows + right.rows,
        }
    }
}

/// The static type of a value: the language's `value_type`.
///
/// This is the schema half of the value model. Record field names live here and
/// never in a [`DynValue`], which is what keeps column-name strings out of every
/// row of every batch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeDesc {
    Bool,
    I64,
    F64,
    /// Plain `String`.
    String,
    /// `optional(T)`. `T` may not itself be optional.
    Optional(Box<TypeDesc>),
    /// `record(name: T, ...)`, in declaration order.
    Record(Vec<(String, TypeDesc)>),
    /// `array(T)`. Every element has type `T`.
    Array(Box<TypeDesc>),
    /// `dict(K,V)`. `K` is restricted to scalars — see [`TypeDesc::is_dict_key`].
    Dict(Box<TypeDesc>, Box<TypeDesc>),
    /// `json`. A document of any shape — never `optional`, because a document
    /// carries its own null.
    Json,
}

impl TypeDesc {
    /// A record type, with its fields in canonical order.
    ///
    /// **Field order is not part of a record's identity.** It is still
    /// load-bearing at runtime — the value is positional, and the order is the
    /// column order the JSON codec writes — but two records naming the same
    /// fields are one type however they were written, which is what lets a
    /// frontend build the same record on two code paths without canonicalising
    /// its own output first.
    ///
    /// Sorting is by field name. Duplicates are a parse error, so the order is
    /// unambiguous. Anything building a record *value* must sort its fields the
    /// same way, since the two stay aligned by position.
    pub fn record(fields: impl IntoIterator<Item = (String, TypeDesc)>) -> Self {
        let mut fields: Vec<(String, TypeDesc)> = fields.into_iter().collect();
        fields.sort_by(|(a, _), (b, _)| a.cmp(b));
        TypeDesc::Record(fields)
    }

    /// The index of a named field, for resolving `row.name` at type-check time.
    pub fn field_index(&self, name: &str) -> Option<usize> {
        match self {
            TypeDesc::Record(fields) => fields.iter().position(|(n, _)| n == name),
            _ => None,
        }
    }

    pub fn field_type(&self, name: &str) -> Option<&TypeDesc> {
        match self {
            TypeDesc::Record(fields) => fields.iter().find(|(n, _)| n == name).map(|(_, t)| t),
            _ => None,
        }
    }

    /// Whether this type may be a `dict` key.
    ///
    /// Scalars only. A dict encodes as a JSON object, whose keys are strings, so
    /// a key type has to have one string spelling that its own type can parse
    /// back — which `string`, `i64`, `f64` and `bool` do and no composite type
    /// does. Absence has no spelling either, so `optional` is out.
    ///
    /// This is the *type* rule; [`DynValue::Dict`] can structurally hold any key,
    /// which is what the ordering proptests exercise.
    pub fn is_dict_key(&self) -> bool {
        matches!(
            self,
            TypeDesc::Bool | TypeDesc::I64 | TypeDesc::F64 | TypeDesc::String
        )
    }

    /// Reads a dict key back from its object-key spelling — the inverse of
    /// [`DynValue::dict_key_string`], with this type saying what to parse.
    pub fn parse_dict_key(&self, s: &str) -> Option<DynValue> {
        match self {
            TypeDesc::String => Some(DynValue::String(s.to_string())),
            TypeDesc::I64 => s.parse().ok().map(DynValue::I64),
            TypeDesc::Bool => s.parse().ok().map(DynValue::Bool),
            TypeDesc::F64 => s
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(|f| DynValue::F64(dbsp::algebra::F64::new(f))),
            _ => Option::None,
        }
    }

    /// Strips one layer of `Option`, so nullability does not have to be handled
    /// at every comparison site.
    pub fn non_null(&self) -> &TypeDesc {
        match self {
            TypeDesc::Optional(inner) => inner,
            other => other,
        }
    }

    pub fn is_optional(&self) -> bool {
        matches!(self, TypeDesc::Optional(_))
    }
}

impl std::fmt::Display for TypeDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeDesc::Bool => write!(f, "bool"),
            TypeDesc::I64 => write!(f, "i64"),
            TypeDesc::F64 => write!(f, "f64"),
            TypeDesc::String => write!(f, "string"),
            TypeDesc::Optional(inner) => write!(f, "optional({inner})"),
            TypeDesc::Array(elem) => write!(f, "array({elem})"),
            TypeDesc::Dict(k, v) => write!(f, "dict({k}, {v})"),
            TypeDesc::Json => write!(f, "json"),
            TypeDesc::Record(fields) => {
                write!(f, "record(")?;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                write!(f, ")")
            }
        }
    }
}

/// The type of a stream's batches: the language's `batch_type`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BatchType {
    ZSet(TypeDesc),
    IndexedZSet(TypeDesc, TypeDesc),
}

impl std::fmt::Display for BatchType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchType::ZSet(t) => write!(f, "zset({t})"),
            BatchType::IndexedZSet(k, v) => write!(f, "indexed_zset({k}, {v})"),
        }
    }
}
