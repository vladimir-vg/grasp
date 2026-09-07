//! Type-checked expressions and their evaluator.
//!
//! Parsing produces [`crate::lang::Expr`], which still refers to record fields
//! by name. Type checking lowers that to [`TypedExpr`], where every field access
//! is a positional index — which is what lets record *values* be positional and
//! keeps string comparison out of the hot path.
//!
//! Evaluation is a straightforward tree walk. Compiling to closures is a later
//! optimization; the shape of this module does not constrain that.

use crate::lang::{BinOp, UnOp};
use crate::value::{DynValue, TypeDesc};
use feldera_sqllib::{FlatVariant, SqlString, Variant};
use std::sync::Arc;
// Aliased because `use DynValue::*` inside several functions below would
// otherwise shadow this type with the `F64` *variant*.
use dbsp::algebra::F64 as Flt;

/// A builtin function. `cast` is not implemented in this cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Coalesce,
    /// `if(cond, a, b)`. The only branching construct in the language, and the
    /// only builtin that does not evaluate all of its arguments.
    If,
    Abs,
    Floor,
    Ceil,
    Round,
    Length,
    Concat,
    Lower,
    Upper,
    Trim,
    /// `get(doc, key)` — a document's member, by string key or 0-based array
    /// index. Total: anything that does not resolve is the absent sentinel.
    Get,
    /// `has_key(doc, key)` — what separates an absent key from one holding an
    /// explicit JSON null, which `get` alone cannot.
    HasKey,
    /// `keys(doc)` — an object's keys, or `NONE` for anything else.
    Keys,
}

impl Builtin {
    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "coalesce" => Builtin::Coalesce,
            "if" => Builtin::If,
            "abs" => Builtin::Abs,
            "floor" => Builtin::Floor,
            "ceil" => Builtin::Ceil,
            "round" => Builtin::Round,
            "length" => Builtin::Length,
            "concat" => Builtin::Concat,
            "lower" => Builtin::Lower,
            "upper" => Builtin::Upper,
            "trim" => Builtin::Trim,
            "get" => Builtin::Get,
            "has_key" => Builtin::HasKey,
            "keys" => Builtin::Keys,
            _ => return None,
        })
    }

    /// Every builtin name, so the reserved-word list cannot drift from it.
    pub const ALL: &'static [&'static str] = &[
        "coalesce", "if", "abs", "floor", "ceil", "round", "length", "concat", "lower", "upper",
        "trim", "get", "has_key", "keys",
    ];

    /// Number of arguments, or `None` if variadic.
    pub fn arity(self) -> Option<usize> {
        Some(match self {
            Builtin::If => 3,
            Builtin::Coalesce | Builtin::Concat | Builtin::Get | Builtin::HasKey => 2,
            _ => 1,
        })
    }
}

/// A resolved conversion — what a `cast(x, T)` turned out to mean.
///
/// One variant per source/target pair rather than one per target, so a `Conv`
/// names a computation exactly. That matters because it is part of a node's
/// content id.
///
/// A conversion is **fallible** when the target type cannot hold every source
/// value. Those are exactly the ones the checker requires an `optional` target
/// for, which is what keeps a declared type a promise: nothing here returns
/// absence into a column that forbids it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conv {
    /// The value already has the target type; only its optionality changed.
    Identity,
    IntToFloat,
    /// Fallible: NaN, the infinities and anything outside `i64` have no value.
    FloatToInt,
    BoolToString,
    IntToString,
    FloatToString,
    /// Fallible: parsing.
    StringToBool,
    /// Fallible: parsing.
    StringToInt,
    /// Fallible: parsing, and a parsed infinity or NaN has no JSON form.
    StringToFloat,

    /// Extract a document into the target type, which the variant carries
    /// because extraction is driven by what is wanted rather than by what the
    /// document happens to be. Always fallible: a document need not hold the
    /// shape asked of it.
    FromJson(Arc<TypeDesc>),
    /// Build a document from a value of the carried source type. Total — a
    /// record's field names live in the type and not in the value, which is why
    /// the type has to travel with the conversion.
    ToJson(Arc<TypeDesc>),
}

impl Conv {
    /// Whether the target type must be `optional`, because the conversion has
    /// inputs it cannot represent.
    pub fn is_fallible(&self) -> bool {
        matches!(
            self,
            Conv::FloatToInt
                | Conv::StringToBool
                | Conv::StringToInt
                | Conv::StringToFloat
                | Conv::FromJson(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedExpr {
    Const(DynValue),
    /// An integer literal that has not yet taken a type from its context. The
    /// type checker rewrites every one of these into a `Const` before lowering
    /// — see `typecheck::infer::commit` — so reaching the evaluator means the
    /// literal settled to its default.
    IntLit(i64),
    /// A float literal awaiting its context, as `IntLit`.
    FloatLit(f64),
    /// A bound parameter, by position in the function's parameter list.
    Var(usize),
    /// Positional field access.
    Field(Box<TypedExpr>, usize),
    /// A record literal. Field names live in the node's `TypeDesc`.
    Record(Vec<TypedExpr>),
    /// An array literal. Every element has the array's one element type.
    Array(Vec<TypedExpr>),
    /// `cast(x, T)`, resolved to the conversion it means.
    Cast(Box<TypedExpr>, Conv),
    Unary(UnOp, Box<TypedExpr>),
    Binary(BinOp, Box<TypedExpr>, Box<TypedExpr>),
    Call(Builtin, Vec<TypedExpr>),
}

/// Evaluate an expression against positionally-bound arguments.
///
/// **Absence does not propagate.** `NONE` is a value: comparisons against it
/// yield a definite `bool`, and it sorts before every other value. Arithmetic
/// on an optional operand is rejected by the type checker rather than yielding
/// absence, so this evaluator should never see one — the fallbacks below are
/// defensive.
pub fn eval(e: &TypedExpr, args: &[&DynValue]) -> DynValue {
    match e {
        TypedExpr::Const(v) => v.clone(),
        // Unreachable in a checked plan: `commit` pins every literal. Rendering
        // the default keeps the evaluator total rather than trusting that.
        TypedExpr::IntLit(v) => DynValue::I64(*v),
        TypedExpr::FloatLit(v) => DynValue::F64(Flt::new(*v)),
        TypedExpr::Var(i) => args[*i].clone(),
        TypedExpr::Field(base, index) => match eval(base, args) {
            DynValue::Record(fields) => fields.get(*index).cloned().unwrap_or(DynValue::None),
            _ => DynValue::None,
        },
        TypedExpr::Record(fields) => {
            DynValue::Record(fields.iter().map(|f| eval(f, args)).collect())
        }
        TypedExpr::Array(items) => {
            DynValue::Array(items.iter().map(|i| eval(i, args)).collect())
        }
        // Absence converts to absence. The checker only admits an absent input
        // where the target type is optional, so this cannot contradict a type.
        TypedExpr::Cast(inner, conv) => match eval(inner, args) {
            DynValue::None => DynValue::None,
            v => convert(conv, v),
        },
        TypedExpr::Unary(op, inner) => eval_unary(*op, eval(inner, args)),
        TypedExpr::Binary(op, l, r) => eval_binary(*op, l, r, args),
        TypedExpr::Call(f, call_args) => eval_call(*f, call_args, args),
    }
}

/// Whether a value counts as true. `NONE` is not true.
pub fn is_true(v: &DynValue) -> bool {
    matches!(v, DynValue::Bool(true))
}

fn eval_unary(op: UnOp, v: DynValue) -> DynValue {
    if v.is_none() {
        return DynValue::None;
    }
    match (op, v) {
        (UnOp::Neg, DynValue::I64(n)) => DynValue::I64(n.wrapping_neg()),
        (UnOp::Neg, DynValue::F64(f)) => DynValue::F64(Flt::new(-f.into_inner())),
        (UnOp::Not, DynValue::Bool(b)) => DynValue::Bool(!b),
        _ => DynValue::None,
    }
}

fn eval_binary(op: BinOp, l: &TypedExpr, r: &TypedExpr, args: &[&DynValue]) -> DynValue {
    // `and`/`or` short-circuit, so they evaluate the right side lazily.
    match op {
        BinOp::And => {
            let lhs = eval(l, args);
            if matches!(lhs, DynValue::Bool(false)) {
                return DynValue::Bool(false);
            }
            let rhs = eval(r, args);
            return match (&lhs, &rhs) {
                (DynValue::Bool(a), DynValue::Bool(b)) => DynValue::Bool(*a && *b),
                _ if matches!(rhs, DynValue::Bool(false)) => DynValue::Bool(false),
                _ => DynValue::None,
            };
        }
        BinOp::Or => {
            let lhs = eval(l, args);
            if matches!(lhs, DynValue::Bool(true)) {
                return DynValue::Bool(true);
            }
            let rhs = eval(r, args);
            return match (&lhs, &rhs) {
                (DynValue::Bool(a), DynValue::Bool(b)) => DynValue::Bool(*a || *b),
                _ if matches!(rhs, DynValue::Bool(true)) => DynValue::Bool(true),
                _ => DynValue::None,
            };
        }
        _ => {}
    }

    let lhs = eval(l, args);
    let rhs = eval(r, args);

    match op {
        // Comparisons are total: `NONE` is a value, so they always decide.
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            match compare(&lhs, &rhs) {
                Some(ord) => DynValue::Bool(match op {
                    BinOp::Eq => ord == std::cmp::Ordering::Equal,
                    BinOp::Ne => ord != std::cmp::Ordering::Equal,
                    BinOp::Lt => ord == std::cmp::Ordering::Less,
                    BinOp::Le => ord != std::cmp::Ordering::Greater,
                    BinOp::Gt => ord == std::cmp::Ordering::Greater,
                    BinOp::Ge => ord != std::cmp::Ordering::Less,
                    _ => unreachable!(),
                }),
                None => DynValue::None,
            }
        }
        // The type checker rejects arithmetic on an optional operand, so this
        // is defensive rather than a propagation rule.
        _ if lhs.is_none() || rhs.is_none() => DynValue::None,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => arith(op, &lhs, &rhs),
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

/// Compares two values, promoting integers to floats when mixed.
///
/// Total wherever `NONE` is involved: it equals itself and sorts before every
/// other value, matching `DynValue`'s own ordering — which is what `min`/`max`
/// use, so expressions and aggregates agree about where absence sits.
///
/// `Option::None` means the two are not comparable at all, which the type
/// checker rejects before evaluation. It has to be written out: `use
/// DynValue::*` below brings `DynValue::None` into scope, which would otherwise
/// shadow the prelude's.
fn compare(l: &DynValue, r: &DynValue) -> Option<std::cmp::Ordering> {
    use DynValue::*;
    match (l, r) {
        (None, None) => Some(std::cmp::Ordering::Equal),
        (None, _) => Some(std::cmp::Ordering::Less),
        (_, None) => Some(std::cmp::Ordering::Greater),
        (I64(a), I64(b)) => Some(a.cmp(b)),
        (F64(a), F64(b)) => Some(a.cmp(b)),
        (I64(a), F64(b)) => Flt::new(*a as f64).partial_cmp(b),
        (F64(a), I64(b)) => a.partial_cmp(&Flt::new(*b as f64)),
        (Bool(a), Bool(b)) => Some(a.cmp(b)),
        (String(a), String(b)) => Some(a.cmp(b)),
        (Record(a), Record(b)) => Some(a.cmp(b)),
        (Array(a), Array(b)) => Some(a.cmp(b)),
        // Byte comparison over the canonical encoding, so two documents written
        // with their keys in different orders are equal. It distinguishes `5`
        // from `5.0`, which are genuinely different documents. Only `==` and
        // `!=` reach here: the type checker rejects ordering over documents,
        // because the encoding sorts by type tag.
        (Json(a), Json(b)) => Some(a.cmp(b)),
        _ => Option::None,
    }
}

fn arith(op: BinOp, l: &DynValue, r: &DynValue) -> DynValue {
    use DynValue::*;
    match (l, r) {
        // Overflow wraps; only a zero divisor yields absence, which is why `/`
        // and `%` are the one arithmetic whose result type is `optional`.
        (I64(a), I64(b)) => match op {
            BinOp::Add => I64(a.wrapping_add(*b)),
            BinOp::Sub => I64(a.wrapping_sub(*b)),
            BinOp::Mul => I64(a.wrapping_mul(*b)),
            BinOp::Div if *b == 0 => None,
            BinOp::Rem if *b == 0 => None,
            BinOp::Div => I64(a.wrapping_div(*b)),
            BinOp::Rem => I64(a.wrapping_rem(*b)),
            _ => None,
        },
        _ => match (as_f64(l), as_f64(r)) {
            (Some(_), Some(b)) if matches!(op, BinOp::Div | BinOp::Rem) && b == 0.0 => None,
            (Some(a), Some(b)) => {
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Rem => a % b,
                    _ => return None,
                };
                F64(Flt::new(v))
            }
            _ => None,
        },
    }
}

/// Applies a resolved conversion to a definite value.
///
/// A fallible conversion yields `NONE`, which is legal because the checker
/// required an `optional` target for exactly these.
fn convert(conv: &Conv, v: DynValue) -> DynValue {
    use DynValue::*;
    // `i64` has values `f64` cannot name and vice versa, so the bound is
    // written as 2^63 rather than `i64::MAX as f64`, which rounds *up* to it.
    const TWO_63: f64 = 9223372036854775808.0;
    match (conv, v) {
        (Conv::Identity, v) => v,
        (Conv::FromJson(ty), Json(fv)) => from_json(&fv, ty).unwrap_or(None),
        (Conv::ToJson(ty), v) => Json(to_json(&v, ty)),
        (Conv::IntToFloat, I64(n)) => F64(Flt::new(n as f64)),
        (Conv::FloatToInt, F64(f)) => {
            let x = f.into_inner();
            if x.is_finite() && x >= -TWO_63 && x < TWO_63 { I64(x as i64) } else { None }
        }
        (Conv::BoolToString, Bool(b)) => String(b.to_string()),
        (Conv::IntToString, I64(n)) => String(n.to_string()),
        // `{:?}` rather than `{}` so a whole number keeps its point and the
        // text parses back as the same float.
        (Conv::FloatToString, F64(f)) => String(format!("{:?}", f.into_inner())),
        (Conv::StringToBool, String(s)) => match s.as_str() {
            "true" => Bool(true),
            "false" => Bool(false),
            _ => None,
        },
        (Conv::StringToInt, String(s)) => s.parse::<i64>().map(I64).unwrap_or(None),
        (Conv::StringToFloat, String(s)) => match s.parse::<f64>() {
            // An infinity or NaN has no JSON form, so it is not a value this
            // conversion may produce.
            Ok(f) if f.is_finite() => F64(Flt::new(f)),
            _ => None,
        },
        // The checker chose the conversion from the operand's type, so a
        // mismatch means the two passes disagree.
        _ => None,
    }
}

/// Whether a document is the **absent** sentinel — a missing key, or a non-object
/// navigated into — as distinct from holding JSON null, which is a value.
///
/// `FlatVariant`'s derived `IsNone` answers "never": the struct is not an
/// `Option`, and absence lives in the encoding's tag instead. Comparing against
/// the one-byte sentinel disagrees on the tag immediately, so this stays cheap
/// even for a large document.
fn is_absent(fv: &FlatVariant) -> bool {
    *fv == FlatVariant::sql_null()
}

/// Extracts a document into `ty`, or `None` if it does not hold that shape.
///
/// A record target navigates with `FlatVariant::index_string`, which shares the
/// buffer rather than cloning and yields the absent sentinel for a missing key
/// or a non-object. That is what keeps pulling two fields out of a large
/// document proportional to the fields rather than to the document — decoding at
/// the root would be O(document) per row.
///
/// One imprecision worth naming: a non-object behaves like an object missing
/// every key, because `FlatVariant` exposes no way to read a value's tag without
/// decoding it. So a record whose fields are *all* optional extracts from a
/// non-object as an all-absent record rather than failing.
fn from_json(fv: &FlatVariant, ty: &TypeDesc) -> Option<DynValue> {
    let target = ty.non_null();

    // The absent sentinel is only a value where the type allows absence.
    if is_absent(fv) {
        return ty.is_optional().then_some(DynValue::None);
    }

    if let TypeDesc::Record(fields) = target {
        let mut out = Vec::with_capacity(fields.len());
        for (name, fty) in fields {
            out.push(from_json(&fv.index_string(name), fty)?);
        }
        return Some(DynValue::Record(out));
    }

    // Everything else needs the value itself, so decode it. For a leaf — which
    // is what navigation lands on — that is cheap.
    let decoded = Variant::from(fv);
    let value = match (&decoded, target) {
        // JSON null is a value, and it converts to nothing but itself.
        (Variant::VariantNull, TypeDesc::Json) => DynValue::Json(fv.clone()),
        (Variant::VariantNull, _) => return ty.is_optional().then_some(DynValue::None),

        (_, TypeDesc::Json) => DynValue::Json(fv.clone()),
        (Variant::Boolean(b), TypeDesc::Bool) => DynValue::Bool(*b),
        (Variant::String(s), TypeDesc::String) => DynValue::String(s.str().to_string()),
        (v, TypeDesc::I64) => DynValue::I64(json_i64(v)?),
        (v, TypeDesc::F64) => DynValue::F64(Flt::new(json_f64(v)?)),
        (Variant::Array(items), TypeDesc::Array(elem)) => DynValue::Array(
            items
                .iter()
                .map(|i| from_json(&FlatVariant::from(i), elem))
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => return Option::None,
    };
    Some(value)
}

/// A document's integer value, exactly. Unlike `f64` extraction this does not
/// widen a float, so a 64-bit key survives a round trip through a document.
fn json_i64(v: &Variant) -> Option<i64> {
    Some(match v {
        Variant::TinyInt(n) => *n as i64,
        Variant::SmallInt(n) => *n as i64,
        Variant::Int(n) => *n as i64,
        Variant::BigInt(n) => *n,
        Variant::UTinyInt(n) => *n as i64,
        Variant::USmallInt(n) => *n as i64,
        Variant::UInt(n) => *n as i64,
        Variant::UBigInt(n) => i64::try_from(*n).ok()?,
        _ => return Option::None,
    })
}

/// A document's numeric value as `f64`. Accepts every numeric tag, so `5` and
/// `5.0` both convert — which is the point of having both extractions.
fn json_f64(v: &Variant) -> Option<f64> {
    if let Variant::Double(d) = v {
        return Some(d.into_inner());
    }
    if let Variant::Real(r) = v {
        return Some(r.into_inner() as f64);
    }
    json_i64(v).map(|n| n as f64)
}

/// Builds a document from a value of `ty`.
///
/// Total, which needs one thing said: JSON cannot represent NaN or an infinity,
/// so those become JSON null. That does not contradict the result type the way
/// writing `null` into an `f64` column would — the column here *is* `json`, and
/// JSON null is one of its values.
fn to_json(v: &DynValue, ty: &TypeDesc) -> FlatVariant {
    FlatVariant::from(to_variant(v, ty))
}

fn to_variant(v: &DynValue, ty: &TypeDesc) -> Variant {
    match (v, ty.non_null()) {
        (DynValue::None, _) => Variant::VariantNull,
        (DynValue::Json(fv), _) => Variant::from(fv),
        (DynValue::Bool(b), _) => Variant::Boolean(*b),
        (DynValue::I64(n), _) => Variant::BigInt(*n),
        (DynValue::F64(f), _) if !f.into_inner().is_finite() => Variant::VariantNull,
        (DynValue::F64(f), _) => Variant::Double(*f),
        (DynValue::String(s), _) => Variant::String(SqlString::from_ref(s)),
        (DynValue::Record(values), TypeDesc::Record(fields)) => Variant::Map(
            fields
                .iter()
                .zip(values)
                .map(|((name, fty), value)| {
                    (Variant::String(SqlString::from_ref(name)), to_variant(value, fty))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
                .into(),
        ),
        (DynValue::Array(items), TypeDesc::Array(elem)) => Variant::Array(
            items.iter().map(|i| to_variant(i, elem)).collect::<Vec<_>>().into(),
        ),
        // The checker pairs the value with its own type, so a mismatch means the
        // two passes disagree.
        _ => Variant::VariantNull,
    }
}

fn as_f64(v: &DynValue) -> Option<f64> {
    match v {
        DynValue::I64(n) => Some(*n as f64),
        DynValue::F64(f) => Some(f.into_inner()),
        _ => Option::None,
    }
}

fn as_str(v: &DynValue) -> Option<&str> {
    match v {
        DynValue::String(s) => Some(s.as_str()),
        _ => Option::None,
    }
}

fn eval_call(f: Builtin, call_args: &[TypedExpr], args: &[&DynValue]) -> DynValue {
    // `if` is the one builtin that must not evaluate all of its arguments: a
    // branch is only worth having if the untaken side does not run. Handled
    // before the arguments are computed, for exactly that reason.
    if f == Builtin::If {
        let taken = if is_true(&eval(&call_args[0], args)) { 1 } else { 2 };
        return eval(&call_args[taken], args);
    }

    let vals: Vec<DynValue> = call_args.iter().map(|a| eval(a, args)).collect();

    // `coalesce` is the one builtin that inspects absence rather than being
    // rejected for it; everything else needs a definite value.
    if f == Builtin::Coalesce {
        return if vals[0].is_none() { vals[1].clone() } else { vals[0].clone() };
    }

    if vals.iter().any(|v| v.is_none()) {
        return DynValue::None;
    }

    match f {
        Builtin::Abs => match &vals[0] {
            DynValue::I64(n) => DynValue::I64(n.wrapping_abs()),
            DynValue::F64(v) => DynValue::F64(Flt::new(v.into_inner().abs())),
            _ => DynValue::None,
        },
        Builtin::Floor | Builtin::Ceil | Builtin::Round => match &vals[0] {
            DynValue::I64(n) => DynValue::I64(*n),
            DynValue::F64(v) => {
                let x = v.into_inner();
                DynValue::F64(Flt::new(match f {
                    Builtin::Floor => x.floor(),
                    Builtin::Ceil => x.ceil(),
                    _ => x.round(),
                }))
            }
            _ => DynValue::None,
        },
        Builtin::Length => match &vals[0] {
            DynValue::String(s) => DynValue::I64(s.chars().count() as i64),
            DynValue::Array(items) => DynValue::I64(items.len() as i64),
            _ => DynValue::None,
        },
        Builtin::Concat => match (as_str(&vals[0]), as_str(&vals[1])) {
            (Some(a), Some(b)) => DynValue::str(&format!("{a}{b}")),
            _ => DynValue::None,
        },
        Builtin::Lower | Builtin::Upper | Builtin::Trim => match as_str(&vals[0]) {
            Some(s) => {
                let out = match f {
                    Builtin::Lower => s.to_lowercase(),
                    Builtin::Upper => s.to_uppercase(),
                    _ => s.trim().to_string(),
                };
                DynValue::String(out)
            }
            None => DynValue::None,
        },
        // Navigation is total, so a document that is not an object, or an index
        // that does not land, is the absent sentinel rather than a failure.
        Builtin::Get => match (&vals[0], &vals[1]) {
            (DynValue::Json(fv), DynValue::String(k)) => DynValue::Json(fv.index_string(k)),
            (DynValue::Json(fv), DynValue::I64(i)) => {
                // 0-based here; `index_from_one` is SQL's convention, and the
                // language has no other 1-based indexing.
                let one_based = i.checked_add(1).map(Variant::BigInt).map(FlatVariant::from);
                let found = one_based.and_then(|k| fv.index_from_one(&k));
                DynValue::Json(found.unwrap_or_else(FlatVariant::sql_null))
            }
            _ => DynValue::None,
        },
        // A key holding an explicit JSON null is present, and `index_string`
        // returns that null rather than the absent sentinel — which is exactly
        // the difference this reports.
        Builtin::HasKey => match (&vals[0], &vals[1]) {
            (DynValue::Json(fv), DynValue::String(k)) => {
                DynValue::Bool(!is_absent(&fv.index_string(k)))
            }
            _ => DynValue::None,
        },
        Builtin::Keys => match &vals[0] {
            DynValue::Json(fv) => match Variant::from(fv) {
                Variant::Map(m) => DynValue::Array(
                    m.keys()
                        .map(|k| match k {
                            Variant::String(s) => DynValue::String(s.str().to_string()),
                            other => DynValue::String(format!("{other:?}")),
                        })
                        .collect(),
                ),
                _ => DynValue::None,
            },
            _ => DynValue::None,
        },
        Builtin::Coalesce | Builtin::If => unreachable!("handled above"),
    }
}
