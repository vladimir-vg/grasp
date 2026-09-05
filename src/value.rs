//! The runtime value model.
//!
//! Every stream in a lowered circuit carries the same Rust type, [`DynValue`].
//! The language's type system lives at the level of [`TypeDesc`], which the type
//! checker computes per node and the JSON codec reads.
//!
//! See `docs/design/mapping.md` for the invariants this type must uphold.

use dbsp::ZWeight;
use dbsp::algebra::{AddAssignByRef, AddByRef, F64, HasZero, MulByRef};
use feldera_macros::IsNone;
use feldera_sqllib::SqlString;
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
    SqlString(SqlString),
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
    Record(#[omit_bounds] #[size_of(skip, skip_bounds)] Vec<DynValue>),
}

impl DynValue {
    pub fn is_none(&self) -> bool {
        matches!(self, DynValue::None)
    }

    pub fn record(fields: impl IntoIterator<Item = DynValue>) -> Self {
        DynValue::Record(fields.into_iter().collect())
    }

    pub fn str(s: &str) -> Self {
        DynValue::SqlString(SqlString::from_ref(s))
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

    /// The name of this value's variant, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            DynValue::None => "NONE",
            DynValue::Bool(_) => "bool",
            DynValue::I64(_) => "i64",
            DynValue::F64(_) => "f64",
            DynValue::String(_) => "String",
            DynValue::SqlString(_) => "sql.SqlString",
            DynValue::Record(_) => "record",
        }
    }
}

/// The accumulator for the linear aggregators (`sum`, `avg`, `count`).
///
/// `aggregate_linear_postprocess` requires an accumulator that is `DBWeight` —
/// additive, with a zero — which [`DynValue`] cannot be: there is no sensible
/// `String + String`. Hence a separate type, which never enters a stream.
///
/// It carries both an integer and a float sum rather than being an enum that
/// picks one, because `HasZero::zero()` takes no context and so could not
/// choose a variant. Only the field matching the projection's statically-known
/// type is ever read; the other stays zero and is added harmlessly.
///
/// `rows` is what lets a linear aggregate tell "the group summed to zero" from
/// "the group is empty" — see the aggregation section of `docs/design/mapping.md`.
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
    pub sum_int: i64,
    pub sum_float: F64,
    /// Rows contributing a non-null projection. This is `count`, and it is what
    /// separates "summed to zero" from "every row was null".
    pub rows: i64,
    /// Every row in the group, null projection or not.
    ///
    /// `dbsp` drops a group whose accumulator is zero, so without this a group
    /// of entirely null projections would sum to zero in every field and
    /// disappear rather than reporting null. Feldera's SQL compiler carries the
    /// same extra counter, for the same reason.
    pub present: i64,
}

impl Acc {
    pub fn int(v: i64) -> Acc {
        Acc { sum_int: v, sum_float: F64::new(0.0), rows: 1, present: 1 }
    }

    pub fn float(v: f64) -> Acc {
        Acc { sum_int: 0, sum_float: F64::new(v), rows: 1, present: 1 }
    }

    /// A row whose projection was null: counted by neither `sum` nor `count`,
    /// but still present, so the group does not vanish.
    pub fn null() -> Acc {
        Acc { sum_int: 0, sum_float: F64::new(0.0), rows: 0, present: 1 }
    }
}

impl HasZero for Acc {
    fn zero() -> Acc {
        Acc::default()
    }

    fn is_zero(&self) -> bool {
        self.sum_int == 0
            && self.sum_float == F64::new(0.0)
            && self.rows == 0
            && self.present == 0
    }
}

impl AddByRef for Acc {
    fn add_by_ref(&self, other: &Acc) -> Acc {
        Acc {
            sum_int: self.sum_int.wrapping_add(other.sum_int),
            sum_float: F64::new(self.sum_float.into_inner() + other.sum_float.into_inner()),
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
            sum_int: self.sum_int.wrapping_mul(*w),
            sum_float: F64::new(self.sum_float.into_inner() * (*w as f64)),
            rows: self.rows.wrapping_mul(*w),
            present: self.present.wrapping_mul(*w),
        }
    }
}

/// The static type of a value: the language's `value_type`.
///
/// This is the schema half of the value model. Record field names live here and
/// never in a [`DynValue`], which is what keeps column-name strings out of every
/// row of every batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeDesc {
    Bool,
    I64,
    F64,
    /// Plain `String`.
    String,
    /// `sql.SqlString`.
    SqlString,
    /// `optional(T)`. `T` may not itself be optional.
    Optional(Box<TypeDesc>),
    /// `record(name: T, ...)`, in declaration order.
    Record(Vec<(String, TypeDesc)>),
}

impl TypeDesc {
    pub fn record(fields: impl IntoIterator<Item = (String, TypeDesc)>) -> Self {
        TypeDesc::Record(fields.into_iter().collect())
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
            TypeDesc::Record(fields) => {
                fields.iter().find(|(n, _)| n == name).map(|(_, t)| t)
            }
            _ => None,
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
            TypeDesc::String => write!(f, "String"),
            TypeDesc::SqlString => write!(f, "sql.SqlString"),
            TypeDesc::Optional(inner) => write!(f, "optional({inner})"),
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
