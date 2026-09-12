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
    /// index, as `optional(json)`.
    ///
    /// Absence means *no such member*; a member holding JSON null comes back as
    /// a document that is null. Those are different, and this is what tells
    /// them apart.
    ///
    /// It is also the one builtin that accepts absence and passes it on. That
    /// is not the propagation arithmetic refuses: `get`'s domain genuinely
    /// includes absence and has one sensible answer there, whereas `+` on an
    /// unknown has no answer without inventing SQL's semantics.
    Get,
    /// `keys(doc)` — an object's keys, or `NONE` for anything else. On a dict,
    /// an `array(K)` — definite, because a dict is always a dict.
    Keys,
    /// `dict_entries(d)` — a dict as an `array(record(key: K, value: V))`.
    ///
    /// The inverse of `dict(a)`, and what turns a dict into rows: `flat_map`
    /// over it emits one row per entry.
    Entries,
    /// `contains(a, x)` — whether the array holds the element.
    ///
    /// `length(filter_array(a, function((e) -> e == x))) > 0` says the same
    /// thing and allocates an array to answer a boolean, which is why this is a
    /// builtin rather than a composition.
    Contains,
    /// `slice(a, start, stop, step)` — Python's slice, over a typed array.
    ///
    /// Negative indices count from the end, the bounds **clamp** rather than
    /// fail, and a negative step reverses. `start` and `stop` are
    /// `optional(i64)` because "no bound here" is not a number: which end it
    /// means depends on the sign of the step, so `NONE` is the only faithful
    /// spelling — the same reason Python's own slice carries `None` there.
    ///
    /// [`Builtin::Get`] and this are the only two that accept absence, and this
    /// one accepts it in arguments rather than passing it on: the result is
    /// definite, an out-of-range slice being empty rather than absent.
    ///
    /// `filter_array` with the index expresses the positive-step case and
    /// **cannot reverse**, so a step below zero needs this whether or not the
    /// rest does.
    Slice,

    /// `make_date(y, m, d)`, `make_time(h, m, s, us)` — a temporal value from
    /// its components.
    ///
    /// **`optional`**, because not every triple of integers is a date:
    /// `make_date(2024, 2, 31)` has no answer, and neither does an hour of 25.
    /// That is the rule division already follows, and the one a frontend turns
    /// into a dropped row.
    MakeDate,
    MakeTime,
    /// `make_timestamp(d, t)` — a date and a time-of-day as one UTC instant.
    /// Total: every pair is one.
    MakeTimestamp,
    /// `timestamp_from_micros(n)` and `epoch_micros(ts)` — the instant as
    /// microseconds since the Unix epoch, both ways. Total.
    TimestampFromMicros,
    EpochMicros,
    /// `epoch_days(d)` — the date as days since the Unix epoch. Total, and the
    /// integer a difference between two dates is counted in.
    EpochDays,
    /// The components. `year`/`month`/`day` read a `date` and the rest read a
    /// `time`, so a component of a `timestamp` is `year(cast(ts, date))` —
    /// which keeps each name over one type instead of two.
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Microsecond,
    /// `make_interval(micros)` — a span of time. Total: every integer is one.
    MakeInterval,
    /// `octet_length(b)` — how many bytes.
    OctetLength,
    /// `bytes_concat(a, b)` — one after the other.
    BytesConcat,
    /// `bytes_and(a, b)`, `bytes_or`, `bytes_xor` — bytewise, and **`optional`**:
    /// two payloads of different lengths have no answer, and `ByteArray`'s own
    /// operations panic there rather than saying so.
    BytesAnd,
    BytesOr,
    BytesXor,
    /// `to_base64(b)` / `from_base64(s)`, `to_hex(b)` / `from_hex(s)`,
    /// `to_utf8(b)` / `from_utf8(s)` — text both ways. Each reading is
    /// `optional`, since not every string is one.
    ToBase64,
    FromBase64,
    ToHex,
    FromHex,
    ToUtf8,
    FromUtf8,
    /// `total_days(iv)` … `total_microseconds(iv)` — the span in whole units of
    /// one size. Named apart from the components because they answer a
    /// different question: `minute(14:30)` is 30, `total_minutes(<100 hours>)`
    /// is 6000.
    TotalDays,
    TotalHours,
    TotalMinutes,
    TotalSeconds,
    TotalMicroseconds,
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
            "keys" => Builtin::Keys,
            "dict_entries" => Builtin::Entries,
            "contains" => Builtin::Contains,
            "slice" => Builtin::Slice,
            "make_date" => Builtin::MakeDate,
            "make_time" => Builtin::MakeTime,
            "make_timestamp" => Builtin::MakeTimestamp,
            "timestamp_from_micros" => Builtin::TimestampFromMicros,
            "epoch_micros" => Builtin::EpochMicros,
            "epoch_days" => Builtin::EpochDays,
            "year" => Builtin::Year,
            "month" => Builtin::Month,
            "day" => Builtin::Day,
            "hour" => Builtin::Hour,
            "minute" => Builtin::Minute,
            "second" => Builtin::Second,
            "microsecond" => Builtin::Microsecond,
            "make_interval" => Builtin::MakeInterval,
            "total_days" => Builtin::TotalDays,
            "total_hours" => Builtin::TotalHours,
            "total_minutes" => Builtin::TotalMinutes,
            "total_seconds" => Builtin::TotalSeconds,
            "total_microseconds" => Builtin::TotalMicroseconds,
            "octet_length" => Builtin::OctetLength,
            "bytes_concat" => Builtin::BytesConcat,
            "bytes_and" => Builtin::BytesAnd,
            "bytes_or" => Builtin::BytesOr,
            "bytes_xor" => Builtin::BytesXor,
            "to_base64" => Builtin::ToBase64,
            "from_base64" => Builtin::FromBase64,
            "to_hex" => Builtin::ToHex,
            "from_hex" => Builtin::FromHex,
            "to_utf8" => Builtin::ToUtf8,
            "from_utf8" => Builtin::FromUtf8,
            _ => return None,
        })
    }

    /// Every builtin name, so the reserved-word list cannot drift from it.
    pub const ALL: &'static [&'static str] = &[
        "coalesce",
        "if",
        "abs",
        "floor",
        "ceil",
        "round",
        "length",
        "concat",
        "lower",
        "upper",
        "trim",
        "get",
        "keys",
        "dict_entries",
        "contains",
        "slice",
        "make_date",
        "make_time",
        "make_timestamp",
        "timestamp_from_micros",
        "epoch_micros",
        "epoch_days",
        "year",
        "month",
        "day",
        "hour",
        "minute",
        "second",
        "microsecond",
        "make_interval",
        "total_days",
        "total_hours",
        "total_minutes",
        "total_seconds",
        "total_microseconds",
        "octet_length",
        "bytes_concat",
        "bytes_and",
        "bytes_or",
        "bytes_xor",
        "to_base64",
        "from_base64",
        "to_hex",
        "from_hex",
        "to_utf8",
        "from_utf8",
    ];

    /// Number of arguments, or `None` if variadic.
    pub fn arity(self) -> Option<usize> {
        Some(match self {
            Builtin::Slice | Builtin::MakeTime => 4,
            Builtin::If | Builtin::MakeDate => 3,
            Builtin::Coalesce
            | Builtin::Concat
            | Builtin::Get
            | Builtin::Contains
            | Builtin::MakeTimestamp
            | Builtin::BytesConcat
            | Builtin::BytesAnd
            | Builtin::BytesOr
            | Builtin::BytesXor => 2,
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

    /// Fallible: parsing. The carried type says which of the three.
    ///
    /// Written text is how a temporal value arrives from outside — the JSON
    /// codec and a dict key already read the same spelling, and this is that
    /// round trip put where a program can reach it.
    StringToTemporal(TypeDesc),
    /// Total. The written form, which is what the codec emits.
    TemporalToString,
    /// Total: an instant always has both halves. `make_timestamp` is the
    /// inverse, and takes the two together.
    TimestampToDate,
    TimestampToTime,

    /// Extract a document into the target type, which the variant carries
    /// because extraction is driven by what is wanted rather than by what the
    /// document happens to be. Always fallible: a document need not hold the
    /// shape asked of it.
    FromJson(Arc<TypeDesc>),
    /// Into a `dynamic`, from a value of the carried source type. Total, and
    /// **tagged**: unlike [`Conv::ToJson`] it keeps the `Variant` tag that says
    /// which type the value was, which is what lets the narrowing back out be
    /// exact rather than a guess that succeeds.
    ToDynamic(Arc<TypeDesc>),
    /// Out of a `dynamic`, into the carried target type. Fallible: a dynamic
    /// holds *some* type and need not hold the one asked for.
    FromDynamic(Arc<TypeDesc>),
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
                | Conv::StringToTemporal(_)
                | Conv::FromJson(_)
                | Conv::FromDynamic(_)
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
    /// `{k => v, ...}` — the literal form, as `(key, value)` pairs.
    Dict(Vec<(TypedExpr, TypedExpr)>),
    /// `dict(a)` — the array form. The operand is an
    /// `array(record(key: K, value: V))`, whose fields are positional by then:
    /// `key` sorts before `value`, so they are fields 0 and 1.
    DictFrom(Box<TypedExpr>),
    /// `cast(x, T)`, resolved to the conversion it means.
    Cast(Box<TypedExpr>, Conv),
    Unary(UnOp, Box<TypedExpr>),
    Binary(BinOp, Box<TypedExpr>, Box<TypedExpr>),
    Call(Builtin, Vec<TypedExpr>),
    /// A `select` binder, by position in the frame — the same index space
    /// [`TypedExpr::Var`] uses, since both read one slot of the argument list.
    ///
    /// It is a variant of its own all the same, because inlining treats the two
    /// oppositely: a `Var` is *replaced* by the argument written at the call
    /// site, and an `Elem` is *shifted* into the caller's frame. Telling them
    /// apart by the size of the index would make a magnitude carry meaning.
    Elem(usize),
    /// `map_array(array, function((e, i) -> body))`.
    ///
    /// The binders have no names here. Two identical computations written with
    /// different binder names must be one node: `push_node` deduplicates on
    /// equality and the content id is a structural hash, so a name would give
    /// them two ids and two `persistent_id`s.
    ///
    /// Nor is there a flag for whether the index was named. The frame grows by
    /// two slots either way, so a body that does not read the index is the same
    /// expression whichever arity was written — and is the same node.
    MapArray {
        array: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    /// `filter_array(array, function((e, i) -> body))`, whose `body` is a
    /// `bool` and whose result is the elements it kept, in order.
    FilterArray {
        array: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
}

/// `body`, once per element, with the element and its index appended to the
/// frame.
///
/// Two slots, always — the language binds both whether or not the written
/// function named the second, so that an expression ignoring the index is the
/// same expression whichever arity produced it.
///
/// The indices are materialised because the frame holds *references*: a value
/// built inside the loop would not outlive the frame that points at it. One
/// `Vec` of them per evaluation, beside the one `Vec` for the frame — a real
/// per-row cost in an interpreted walk, named here so it is a known one.
fn over_elements(args: &[&DynValue], items: &[DynValue], body: &TypedExpr) -> Vec<DynValue> {
    let indices: Vec<DynValue> = (0..items.len() as i64).map(DynValue::I64).collect();
    let mut frame: Vec<&DynValue> = Vec::with_capacity(args.len() + 2);
    frame.extend_from_slice(args);
    frame.push(&DynValue::None);
    frame.push(&DynValue::None);
    let element = frame.len() - 2;
    items
        .iter()
        .zip(indices.iter())
        .map(|(item, index)| {
            frame[element] = item;
            frame[element + 1] = index;
            eval(body, &frame)
        })
        .collect()
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
        TypedExpr::Var(i) | TypedExpr::Elem(i) => args[*i].clone(),
        // The two places an expression is evaluated more than once under
        // different bindings. The frame grows by exactly two slots, appended —
        // the element and its index — so every index already in scope keeps its
        // meaning, and it grows by two whether or not the index was named.
        //
        // One `Vec` per evaluation, reused across the elements. That is a real
        // per-row cost in an interpreted walk, named here so it is a known one.
        TypedExpr::MapArray { array, body } => match eval(array, args) {
            DynValue::Array(items) => DynValue::Array(over_elements(args, &items, body)),
            // The checker admits only an array, so this is defensive — and
            // absence rather than an empty array, as `DictFrom` has it.
            _ => DynValue::None,
        },
        TypedExpr::FilterArray { array, body } => match eval(array, args) {
            DynValue::Array(items) => DynValue::Array(
                over_elements(args, &items, body)
                    .into_iter()
                    .zip(items.iter())
                    .filter(|(keep, _)| matches!(keep, DynValue::Bool(true)))
                    .map(|(_, item)| item.clone())
                    .collect(),
            ),
            _ => DynValue::None,
        },
        TypedExpr::Field(base, index) => match eval(base, args) {
            DynValue::Record(fields) => fields.get(*index).cloned().unwrap_or(DynValue::None),
            _ => DynValue::None,
        },
        TypedExpr::Record(fields) => {
            DynValue::Record(fields.iter().map(|f| eval(f, args)).collect())
        }
        TypedExpr::Array(items) => DynValue::Array(items.iter().map(|i| eval(i, args)).collect()),
        // Collecting into a `BTreeMap` is what canonicalises: entries sort, and
        // a repeated key keeps the last value written, as a later insert wins.
        TypedExpr::Dict(entries) => DynValue::Dict(
            entries
                .iter()
                .map(|(k, v)| (eval(k, args), eval(v, args)))
                .collect(),
        ),
        // The operand is an `array(record(key: K, value: V))`. `key` sorts
        // before `value`, so the record is positional as fields 0 and 1.
        TypedExpr::DictFrom(inner) => match eval(inner, args) {
            DynValue::Array(items) => DynValue::Dict(
                items
                    .iter()
                    .filter_map(|e| match e.fields() {
                        Some([k, v]) => Some((k.clone(), v.clone())),
                        _ => Option::None,
                    })
                    .collect(),
            ),
            _ => DynValue::None,
        },
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
        BinOp::Add | BinOp::Sub => {
            temporal(op, &lhs, &rhs).unwrap_or_else(|| arith(op, &lhs, &rhs))
        }
        BinOp::Mul | BinOp::Div | BinOp::Rem => arith(op, &lhs, &rhs),
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
        // Each orders as the instant it names, which is the ordering its own
        // newtype derives — so unlike a document there is nothing arbitrary
        // about it, and `<` means what a reader expects.
        (Date(a), Date(b)) => Some(a.cmp(b)),
        (Time(a), Time(b)) => Some(a.cmp(b)),
        (Timestamp(a), Timestamp(b)) => Some(a.cmp(b)),
        // One integer, so the order is the span it names.
        (Interval(a), Interval(b)) => Some(a.cmp(b)),
        // Lexicographic, which is the order the payload already has.
        (Bytes(a), Bytes(b)) => Some(a.cmp(b)),
        // Byte comparison over the canonical encoding, as for a document — and
        // like one, only `==` and `!=` reach here.
        (Dynamic(a), Dynamic(b)) => Some(a.cmp(b)),
        _ => Option::None,
    }
}

/// `+` and `-` over temporal values, or `None` where the pair is not one of the
/// shapes and ordinary arithmetic should have it.
///
/// **Every one is total.** A `date` has no sub-day resolution, so an interval
/// applied to one truncates toward zero: a shift of 30 hours moves it a day and
/// a shift of one hour leaves it alone. A `time` wraps at midnight.
fn temporal(op: BinOp, l: &DynValue, r: &DynValue) -> Option<DynValue> {
    use DynValue::*;
    use feldera_sqllib as sql;
    let sub = op == BinOp::Sub;
    // Shifting reads as well in either order, and the operand that is not the
    // interval is the one whose type comes back. Subtraction does not commute:
    // a moment less a duration is a moment, a duration less a moment is
    // nothing, which the checker refuses before this runs.
    let (subject, iv) = match (l, r) {
        (_, Interval(iv)) => (l, *iv),
        (Interval(iv), _) if !sub => (r, *iv),
        _ => (l, sql::ShortInterval::from_microseconds(0)),
    };
    let signed = sql::ShortInterval::from_microseconds(if sub {
        iv.microseconds().wrapping_neg()
    } else {
        iv.microseconds()
    });
    Some(match (l, r) {
        (Date(a), Date(b)) if sub => Interval(sql::minus_ShortInterval_Date_Date__(*a, *b)),
        (Time(a), Time(b)) if sub => Interval(sql::minus_ShortInterval_Time_Time__(*a, *b)),
        (Timestamp(a), Timestamp(b)) if sub => {
            Interval(sql::minus_ShortInterval_Timestamp_Timestamp__(*a, *b))
        }
        (Interval(a), Interval(b)) => Interval(sql::ShortInterval::from_microseconds(if sub {
            a.microseconds().wrapping_sub(b.microseconds())
        } else {
            a.microseconds().wrapping_add(b.microseconds())
        })),
        _ => match subject {
            Timestamp(t) => Timestamp(sql::plus_Timestamp_Timestamp_ShortInterval__(*t, signed)),
            // Truncating toward zero, which is `plus_Date_Date_ShortInterval__`.
            Date(d) => Date(sql::plus_Date_Date_ShortInterval__(*d, signed)),
            Time(t) => Time(sql::plus_Time_Time_ShortInterval__(*t, signed)),
            _ => return Option::None,
        },
    })
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
        (Conv::ToDynamic(ty), v) => Dynamic(to_dynamic(&v, ty)),
        (Conv::FromDynamic(ty), Dynamic(fv)) => from_dynamic(&fv, ty).unwrap_or(None),
        (Conv::IntToFloat, I64(n)) => F64(Flt::new(n as f64)),
        (Conv::FloatToInt, F64(f)) => {
            let x = f.into_inner();
            if x.is_finite() && (-TWO_63..TWO_63).contains(&x) {
                I64(x as i64)
            } else {
                None
            }
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
        (Conv::StringToTemporal(t), String(s)) => t.parse_dict_key(s.as_str()).unwrap_or(None),
        (Conv::TemporalToString, v @ (Date(_) | Time(_) | Timestamp(_) | Interval(_))) => String(
            v.dict_key_string()
                .expect("a temporal value has a written form"),
        ),
        (Conv::TimestampToDate, Timestamp(t)) => Date(t.get_date()),
        (Conv::TimestampToTime, Timestamp(t)) => match feldera_sqllib::cast_to_Time_Timestamp(t) {
            Ok(time) => Time(time),
            Err(_) => None,
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
        // A temporal value is a string in a document, read in the one spelling
        // its own type writes — the same pairing a dict key uses just below.
        (v, TypeDesc::I64) => DynValue::I64(json_i64(v)?),
        (v, TypeDesc::F64) => DynValue::F64(Flt::new(json_f64(v)?)),
        (Variant::Array(items), TypeDesc::Array(elem)) => DynValue::Array(
            items
                .iter()
                .map(|i| from_json(&FlatVariant::from(i), elem))
                .collect::<Option<Vec<_>>>()?,
        ),
        // A document's object keys are strings, and the dict's key type says
        // what to read them as — the same pairing the JSON codec uses.
        (Variant::Map(entries), TypeDesc::Dict(kt, vt)) => DynValue::Dict(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        Variant::String(s) => kt.parse_dict_key(s.str())?,
                        other => kt.parse_dict_key(&format!("{other:?}"))?,
                    };
                    Some((key, from_json(&FlatVariant::from(v), vt)?))
                })
                .collect::<Option<_>>()?,
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

/// Into a `dynamic` — the same payload a document uses, with the tag kept.
///
/// Where [`to_variant`] has no date arm at all, because JSON has no date and a
/// document holds what JSON holds, this writes `Variant::Date`. That is the
/// whole of what separates the two types: a document says what a value looks
/// like once encoded, a dynamic says what it is.
fn to_dynamic(v: &DynValue, ty: &TypeDesc) -> FlatVariant {
    FlatVariant::from(to_tagged(v, ty))
}

fn to_tagged(v: &DynValue, ty: &TypeDesc) -> Variant {
    match (v, ty.non_null()) {
        (DynValue::Date(d), _) => Variant::Date(*d),
        (DynValue::Time(t), _) => Variant::Time(*t),
        (DynValue::Timestamp(t), _) => Variant::Timestamp(*t),
        (DynValue::Interval(iv), _) => Variant::ShortInterval(*iv),
        (DynValue::Bytes(b), _) => Variant::Binary(b.clone()),
        (DynValue::Array(items), TypeDesc::Array(elem)) => Variant::Array(
            items
                .iter()
                .map(|i| to_tagged(i, elem))
                .collect::<Vec<_>>()
                .into(),
        ),
        // A record and a dict both become a map, which is the one place a
        // dynamic is as lenient as a document: `Variant` has a single `Map`.
        (DynValue::Record(values), TypeDesc::Record(fields)) => Variant::Map(
            fields
                .iter()
                .zip(values)
                .map(|((name, fty), value)| {
                    (
                        Variant::String(SqlString::from_ref(name)),
                        to_tagged(value, fty),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
                .into(),
        ),
        (DynValue::Dict(entries), TypeDesc::Dict(_, vt)) => Variant::Map(
            entries
                .iter()
                .filter_map(|(k, value)| {
                    let key = k.dict_key_string()?;
                    Some((
                        Variant::String(SqlString::from_ref(&key)),
                        to_tagged(value, vt),
                    ))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
                .into(),
        ),
        // Everything else a document already spells exactly.
        _ => to_variant(v, ty),
    }
}

/// Whether a `Variant` is something JSON can spell — the same boundary
/// `extractable` draws over types, drawn over a value.
fn json_able(v: &Variant) -> bool {
    match v {
        Variant::SqlNull
        | Variant::VariantNull
        | Variant::Boolean(_)
        | Variant::BigInt(_)
        | Variant::Double(_)
        | Variant::String(_) => true,
        Variant::Array(items) => items.iter().all(json_able),
        Variant::Map(entries) => entries.iter().all(|(_, v)| json_able(v)),
        _ => false,
    }
}

/// Out of a `dynamic`. `None` where it does not hold that type — and unlike a
/// document, *holding* is an exact question: a dynamic carrying a `date` is not
/// a `string`, where a document carrying `"2024-01-15"` is both.
fn from_dynamic(fv: &FlatVariant, ty: &TypeDesc) -> Option<DynValue> {
    let target = ty.non_null();
    if is_absent(fv) {
        return ty.is_optional().then_some(DynValue::None);
    }
    let decoded = Variant::from(fv);
    let value = match (&decoded, target) {
        (Variant::VariantNull, _) => return ty.is_optional().then_some(DynValue::None),
        (Variant::Boolean(b), TypeDesc::Bool) => DynValue::Bool(*b),
        (Variant::BigInt(n), TypeDesc::I64) => DynValue::I64(*n),
        (Variant::Double(f), TypeDesc::F64) => DynValue::F64(*f),
        (Variant::String(s), TypeDesc::String) => DynValue::String(s.str().to_string()),
        (Variant::Date(d), TypeDesc::Date) => DynValue::Date(*d),
        (Variant::Time(t), TypeDesc::Time) => DynValue::Time(*t),
        (Variant::Timestamp(t), TypeDesc::Timestamp) => DynValue::Timestamp(*t),
        (Variant::ShortInterval(iv), TypeDesc::Interval) => DynValue::Interval(*iv),
        (Variant::Binary(b), TypeDesc::Bytes) => DynValue::Bytes(b.clone()),
        (Variant::Array(items), TypeDesc::Array(elem)) => DynValue::Array(
            items
                .iter()
                .map(|i| from_dynamic(&FlatVariant::from(i), elem))
                .collect::<Option<Vec<_>>>()?,
        ),
        (Variant::Map(entries), TypeDesc::Record(fields)) => {
            let mut out = Vec::with_capacity(fields.len());
            for (name, fty) in fields {
                let found = entries
                    .iter()
                    .find(|(k, _)| matches!(k, Variant::String(s) if s.str() == name))
                    .map(|(_, v)| v)?;
                out.push(from_dynamic(&FlatVariant::from(found), fty)?);
            }
            DynValue::Record(out)
        }
        (Variant::Map(entries), TypeDesc::Dict(kt, vt)) => DynValue::Dict(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        Variant::String(s) => kt.parse_dict_key(s.str())?,
                        other => kt.parse_dict_key(&format!("{other:?}"))?,
                    };
                    Some((key, from_dynamic(&FlatVariant::from(v), vt)?))
                })
                .collect::<Option<std::collections::BTreeMap<_, _>>>()?,
        ),
        // A dynamic is a document when what it holds is one — which is how a
        // program reaches the lenient reading on purpose. A dynamic holding a
        // date is not: JSON has no date, so the same rule applies one level in.
        (v, TypeDesc::Json) if json_able(v) => DynValue::Json(fv.clone()),
        _ => return Option::None,
    };
    Some(value)
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
                    (
                        Variant::String(SqlString::from_ref(name)),
                        to_variant(value, fty),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
                .into(),
        ),
        (DynValue::Array(items), TypeDesc::Array(elem)) => Variant::Array(
            items
                .iter()
                .map(|i| to_variant(i, elem))
                .collect::<Vec<_>>()
                .into(),
        ),
        // Keys become strings, matching `DynValue::dict_key_string` and so the
        // JSON codec: a dict cast to a document and then written out spells its
        // keys the same way as one written out directly.
        (DynValue::Dict(entries), TypeDesc::Dict(_, vt)) => Variant::Map(
            entries
                .iter()
                .filter_map(|(k, v)| {
                    let key = k.dict_key_string()?;
                    Some((
                        Variant::String(SqlString::from_ref(&key)),
                        to_variant(v, vt),
                    ))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
                .into(),
        ),
        // No temporal arm and no `bytes` arm, and that is the rule rather than
        // an omission: JSON has neither, so `extractable` refuses the cast
        // before it reaches here. A program that wants a date in a document
        // writes the encoding it means, `cast(cast(d, string), json)`.
        //
        // The checker pairs the value with its own type, so anything reaching
        // this means the two passes disagree.
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
        let taken = if is_true(&eval(&call_args[0], args)) {
            1
        } else {
            2
        };
        return eval(&call_args[taken], args);
    }

    let vals: Vec<DynValue> = call_args.iter().map(|a| eval(a, args)).collect();

    // `coalesce` is the one builtin that inspects absence rather than being
    // rejected for it; everything else needs a definite value.
    if f == Builtin::Coalesce {
        return if vals[0].is_none() {
            vals[1].clone()
        } else {
            vals[0].clone()
        };
    }

    // `slice` is the other one, and it reads absence rather than propagating it:
    // a `NONE` bound means "no bound here", so only its array and its step have
    // to be definite.
    let absent = match f {
        Builtin::Slice => vals[0].is_none() || vals[3].is_none(),
        _ => vals.iter().any(|v| v.is_none()),
    };
    if absent {
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
            DynValue::Dict(entries) => DynValue::I64(entries.len() as i64),
            _ => DynValue::None,
        },
        // Entries are already sorted by key: a `BTreeMap` iterates in order, so
        // the array is deterministic without sorting anything here.
        Builtin::Entries => match &vals[0] {
            DynValue::Dict(entries) => DynValue::Array(
                entries
                    .iter()
                    .map(|(k, v)| DynValue::record([k.clone(), v.clone()]))
                    .collect(),
            ),
            _ => DynValue::None,
        },
        Builtin::Contains => match &vals[0] {
            DynValue::Array(items) => DynValue::Bool(items.contains(&vals[1])),
            _ => DynValue::None,
        },
        Builtin::Slice => match &vals[0] {
            DynValue::Array(items) => DynValue::Array(slice(items, &vals[1], &vals[2], &vals[3])),
            _ => DynValue::None,
        },
        Builtin::MakeDate => match (&vals[0], &vals[1], &vals[2]) {
            (DynValue::I64(y), DynValue::I64(m), DynValue::I64(d)) => {
                match feldera_sqllib::make_date___(*y as i32, *m as i32, *d as i32) {
                    Some(date) => DynValue::Date(date),
                    _ => DynValue::None,
                }
            }
            _ => DynValue::None,
        },
        // Built through the written form rather than from components, so that
        // construction and parsing cannot disagree about what a time is — and
        // because the component constructor that takes a fraction takes it as
        // an `f64`, where a microsecond is not exactly representable.
        Builtin::MakeTime => match (&vals[0], &vals[1], &vals[2], &vals[3]) {
            (DynValue::I64(h), DynValue::I64(m), DynValue::I64(s), DynValue::I64(us))
                if (0..24).contains(h)
                    && (0..60).contains(m)
                    && (0..60).contains(s)
                    && (0..1_000_000).contains(us) =>
            {
                crate::value::parse_time(&format!("{h:02}:{m:02}:{s:02}.{us:06}"))
                    .unwrap_or(DynValue::None)
            }
            _ => DynValue::None,
        },
        Builtin::MakeTimestamp => match (&vals[0], &vals[1]) {
            (DynValue::Date(d), DynValue::Time(t)) => {
                match feldera_sqllib::make_timestampNN(Some(*d), Some(*t)) {
                    Some(ts) => DynValue::Timestamp(ts),
                    _ => DynValue::None,
                }
            }
            _ => DynValue::None,
        },
        Builtin::TimestampFromMicros => match &vals[0] {
            DynValue::I64(n) => {
                DynValue::Timestamp(feldera_sqllib::Timestamp::from_microseconds(*n))
            }
            _ => DynValue::None,
        },
        Builtin::EpochMicros => match &vals[0] {
            DynValue::Timestamp(t) => DynValue::I64(t.microseconds()),
            _ => DynValue::None,
        },
        Builtin::EpochDays => match &vals[0] {
            DynValue::Date(d) => DynValue::I64(d.days() as i64),
            _ => DynValue::None,
        },
        // Each reads either type that holds the component, so a `timestamp` is
        // taken directly rather than through a written conversion.
        Builtin::Year | Builtin::Month | Builtin::Day => {
            let date = match &vals[0] {
                DynValue::Date(d) => *d,
                DynValue::Timestamp(t) => t.get_date(),
                _ => return DynValue::None,
            };
            DynValue::I64(match f {
                Builtin::Year => feldera_sqllib::extract_year_Date(date),
                Builtin::Month => feldera_sqllib::extract_month_Date(date),
                _ => feldera_sqllib::extract_day_Date(date),
            })
        }
        Builtin::Hour | Builtin::Minute | Builtin::Second | Builtin::Microsecond => {
            let time = match &vals[0] {
                DynValue::Time(t) => *t,
                DynValue::Timestamp(t) => match feldera_sqllib::cast_to_Time_Timestamp(*t) {
                    Ok(time) => time,
                    Err(_) => return DynValue::None,
                },
                _ => return DynValue::None,
            };
            DynValue::I64(match f {
                Builtin::Hour => feldera_sqllib::extract_hour_Time(time),
                Builtin::Minute => feldera_sqllib::extract_minute_Time(time),
                Builtin::Second => feldera_sqllib::extract_second_Time(time),
                // SQL's `EXTRACT(MICROSECOND)` folds the seconds in, and
                // returns 5_123_456 for `…:05.123456`. Here it is the
                // sub-second part, so that `microsecond` is the inverse of
                // the argument `make_time` takes and the two round-trip.
                _ => feldera_sqllib::extract_microsecond_Time(time) % 1_000_000,
            })
        }
        Builtin::OctetLength => match &vals[0] {
            DynValue::Bytes(b) => DynValue::I64(b.length() as i64),
            _ => DynValue::None,
        },
        Builtin::ToBase64 => match &vals[0] {
            DynValue::Bytes(b) => DynValue::str(&crate::value::to_base64(b.as_slice())),
            _ => DynValue::None,
        },
        Builtin::ToHex => match &vals[0] {
            DynValue::Bytes(b) => DynValue::String(
                b.as_slice()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            ),
            _ => DynValue::None,
        },
        Builtin::ToUtf8 => match &vals[0] {
            DynValue::Bytes(b) => match std::str::from_utf8(b.as_slice()) {
                Ok(s) => DynValue::str(s),
                Err(_) => DynValue::None,
            },
            _ => DynValue::None,
        },
        Builtin::FromBase64 => match as_str(&vals[0]) {
            Some(s) => crate::value::parse_base64(s).unwrap_or(DynValue::None),
            _ => DynValue::None,
        },
        Builtin::FromHex => match as_str(&vals[0]) {
            Some(s) if s.len() % 2 == 0 => {
                let bytes: Option<Vec<u8>> = (0..s.len() / 2)
                    .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
                    .collect();
                match bytes {
                    Some(b) => DynValue::Bytes(feldera_sqllib::ByteArray::from_vec(b)),
                    _ => DynValue::None,
                }
            }
            _ => DynValue::None,
        },
        Builtin::FromUtf8 => match as_str(&vals[0]) {
            Some(s) => DynValue::Bytes(feldera_sqllib::ByteArray::new(s.as_bytes())),
            _ => DynValue::None,
        },
        Builtin::BytesConcat => match (&vals[0], &vals[1]) {
            (DynValue::Bytes(a), DynValue::Bytes(b)) => DynValue::Bytes(a.concat(b)),
            _ => DynValue::None,
        },
        // Checked here rather than left to `ByteArray`, whose `and`/`or`/`xor`
        // panic on a length mismatch. Two payloads of different lengths have no
        // bytewise answer, and this is how the language says so.
        Builtin::BytesAnd | Builtin::BytesOr | Builtin::BytesXor => match (&vals[0], &vals[1]) {
            (DynValue::Bytes(a), DynValue::Bytes(b)) if a.length() == b.length() => {
                DynValue::Bytes(match f {
                    Builtin::BytesAnd => a.and(b),
                    Builtin::BytesOr => a.or(b),
                    _ => a.xor(b),
                })
            }
            _ => DynValue::None,
        },
        Builtin::MakeInterval => match &vals[0] {
            DynValue::I64(n) => {
                DynValue::Interval(feldera_sqllib::ShortInterval::from_microseconds(*n))
            }
            _ => DynValue::None,
        },
        Builtin::TotalDays
        | Builtin::TotalHours
        | Builtin::TotalMinutes
        | Builtin::TotalSeconds
        | Builtin::TotalMicroseconds => match &vals[0] {
            DynValue::Interval(iv) => DynValue::I64(match f {
                Builtin::TotalDays => iv.days(),
                Builtin::TotalHours => iv.hours(),
                Builtin::TotalMinutes => iv.minutes(),
                Builtin::TotalSeconds => iv.seconds(),
                _ => iv.microseconds(),
            }),
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
        // Navigation is total: a missing key, an index past the end, or a
        // document that is not a container yields absence. The encoding's absent
        // sentinel is converted here rather than escaping as a `json` value —
        // it would print as `null` and not be the null document, which is a
        // third kind of nothing nobody can see.
        Builtin::Get => {
            // A dict lookup is exact: the key is a value, not a path, so there
            // is no navigation to be total about.
            if let DynValue::Dict(entries) = &vals[0] {
                return entries.get(&vals[1]).cloned().unwrap_or(DynValue::None);
            }
            // An array index is exact too, and 0-based. Out of range — either
            // end — is absence rather than an error, which is the same answer a
            // document gives for a member that is not there.
            if let (DynValue::Array(items), DynValue::I64(i)) = (&vals[0], &vals[1]) {
                return usize::try_from(*i)
                    .ok()
                    .and_then(|i| items.get(i))
                    .cloned()
                    .unwrap_or(DynValue::None);
            }
            let found = match (&vals[0], &vals[1]) {
                (DynValue::Json(fv), DynValue::String(k)) => Some(fv.index_string(k)),
                (DynValue::Json(fv), DynValue::I64(i)) => {
                    // 0-based here; `index_from_one` is SQL's convention, and
                    // the language has no other 1-based indexing.
                    let one_based = i.checked_add(1).map(Variant::BigInt).map(FlatVariant::from);
                    one_based.and_then(|k| fv.index_from_one(&k))
                }
                _ => Option::None,
            };
            match found {
                Some(fv) if !is_absent(&fv) => DynValue::Json(fv),
                _ => DynValue::None,
            }
        }
        Builtin::Keys => match &vals[0] {
            DynValue::Dict(entries) => DynValue::Array(entries.keys().cloned().collect()),
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

/// Python's slice, over an array — `CPython`'s `slice.indices` and the loop that
/// follows it.
///
/// `start` and `stop` are `NONE` for "no bound here", which is not a number:
/// with a positive step the missing start is 0 and the missing stop is the
/// length, and with a negative one they are the last index and one before the
/// first. That is why the bounds are `optional(i64)` rather than the plain ones
/// a caller could have defaulted itself.
///
/// A step of zero selects nothing. Python raises there; every expression in this
/// language is total, so the empty array is the answer, and it is the one place
/// this differs from the semantics it copies.
fn slice(items: &[DynValue], start: &DynValue, stop: &DynValue, step: &DynValue) -> Vec<DynValue> {
    let n = items.len() as i64;
    let DynValue::I64(step) = *step else {
        return Vec::new();
    };
    if step == 0 {
        return Vec::new();
    }
    // A bound outside the array clamps to its nearer end rather than failing,
    // and which end that is depends on the direction of travel.
    let adjust = |i: i64| -> i64 {
        let i = if i < 0 { i + n } else { i };
        if i < 0 {
            if step < 0 { -1 } else { 0 }
        } else if i >= n {
            if step < 0 { n - 1 } else { n }
        } else {
            i
        }
    };
    let bound = |v: &DynValue, absent: i64| match v {
        DynValue::I64(i) => adjust(*i),
        _ => absent,
    };
    let (first, last) = if step < 0 {
        (bound(start, n - 1), bound(stop, -1))
    } else {
        (bound(start, 0), bound(stop, n))
    };

    let mut out = Vec::new();
    let mut i = first;
    while if step < 0 { i > last } else { i < last } {
        out.push(items[i as usize].clone());
        i += step;
    }
    out
}
