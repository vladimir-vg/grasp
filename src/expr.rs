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
use crate::value::DynValue;
// Aliased because `use DynValue::*` inside several functions below would
// otherwise shadow this type with the `F64` *variant*.
use dbsp::algebra::F64 as Flt;
use feldera_sqllib::SqlString;

/// A builtin function. `cast` is not implemented in this cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Coalesce,
    Abs,
    Floor,
    Ceil,
    Round,
    Length,
    Concat,
    Lower,
    Upper,
    Trim,
}

impl Builtin {
    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "coalesce" => Builtin::Coalesce,
            "abs" => Builtin::Abs,
            "floor" => Builtin::Floor,
            "ceil" => Builtin::Ceil,
            "round" => Builtin::Round,
            "length" => Builtin::Length,
            "concat" => Builtin::Concat,
            "lower" => Builtin::Lower,
            "upper" => Builtin::Upper,
            "trim" => Builtin::Trim,
            _ => return None,
        })
    }

    /// Every builtin name, so the reserved-word list cannot drift from it.
    pub const ALL: &'static [&'static str] = &[
        "coalesce", "abs", "floor", "ceil", "round", "length", "concat", "lower", "upper", "trim",
    ];

    /// Number of arguments, or `None` if variadic.
    pub fn arity(self) -> Option<usize> {
        Some(match self {
            Builtin::Coalesce => 2,
            Builtin::Concat => 2,
            _ => 1,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedExpr {
    Const(DynValue),
    /// A bound parameter, by position in the function's parameter list.
    Var(usize),
    /// Positional field access.
    Field(Box<TypedExpr>, usize),
    /// A record literal. Field names live in the node's `TypeDesc`.
    Record(Vec<TypedExpr>),
    Unary(UnOp, Box<TypedExpr>),
    Binary(BinOp, Box<TypedExpr>, Box<TypedExpr>),
    Call(Builtin, Vec<TypedExpr>),
}

/// Evaluate an expression against positionally-bound arguments.
///
/// **Absence does not propagate.** `ABSENT` is a value: comparisons against it
/// yield a definite `bool`, and it sorts before every other value. Arithmetic
/// on an optional operand is rejected by the type checker rather than yielding
/// absence, so this evaluator should never see one — the fallbacks below are
/// defensive.
pub fn eval(e: &TypedExpr, args: &[&DynValue]) -> DynValue {
    match e {
        TypedExpr::Const(v) => v.clone(),
        TypedExpr::Var(i) => args[*i].clone(),
        TypedExpr::Field(base, index) => match eval(base, args) {
            DynValue::Record(fields) => fields.get(*index).cloned().unwrap_or(DynValue::Absent),
            _ => DynValue::Absent,
        },
        TypedExpr::Record(fields) => {
            DynValue::Record(fields.iter().map(|f| eval(f, args)).collect())
        }
        TypedExpr::Unary(op, inner) => eval_unary(*op, eval(inner, args)),
        TypedExpr::Binary(op, l, r) => eval_binary(*op, l, r, args),
        TypedExpr::Call(f, call_args) => eval_call(*f, call_args, args),
    }
}

/// Whether a value counts as true. `ABSENT` is not true.
pub fn is_true(v: &DynValue) -> bool {
    matches!(v, DynValue::Bool(true))
}

fn eval_unary(op: UnOp, v: DynValue) -> DynValue {
    if v.is_absent() {
        return DynValue::Absent;
    }
    match (op, v) {
        (UnOp::Neg, DynValue::I64(n)) => DynValue::I64(-n),
        (UnOp::Neg, DynValue::F64(f)) => DynValue::F64(Flt::new(-f.into_inner())),
        (UnOp::Not, DynValue::Bool(b)) => DynValue::Bool(!b),
        _ => DynValue::Absent,
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
                _ => DynValue::Absent,
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
                _ => DynValue::Absent,
            };
        }
        _ => {}
    }

    let lhs = eval(l, args);
    let rhs = eval(r, args);

    match op {
        // Comparisons are total: `ABSENT` is a value, so they always decide.
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
                None => DynValue::Absent,
            }
        }
        // The type checker rejects arithmetic on an optional operand, so this
        // is defensive rather than a propagation rule.
        _ if lhs.is_absent() || rhs.is_absent() => DynValue::Absent,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => arith(op, &lhs, &rhs),
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

/// Compares two values, promoting integers to floats when mixed.
///
/// Total wherever `ABSENT` is involved: it equals itself and sorts before every
/// other value, matching `DynValue`'s own ordering — which is what `min`/`max`
/// use, so expressions and aggregates agree about where absence sits.
///
/// `None` means the two are not comparable at all, which the type checker
/// rejects before evaluation.
fn compare(l: &DynValue, r: &DynValue) -> Option<std::cmp::Ordering> {
    use DynValue::*;
    match (l, r) {
        (Absent, Absent) => Some(std::cmp::Ordering::Equal),
        (Absent, _) => Some(std::cmp::Ordering::Less),
        (_, Absent) => Some(std::cmp::Ordering::Greater),
        (I64(a), I64(b)) => Some(a.cmp(b)),
        (F64(a), F64(b)) => Some(a.cmp(b)),
        (I64(a), F64(b)) => Flt::new(*a as f64).partial_cmp(b),
        (F64(a), I64(b)) => a.partial_cmp(&Flt::new(*b as f64)),
        (Bool(a), Bool(b)) => Some(a.cmp(b)),
        (String(a), String(b)) => Some(a.cmp(b)),
        (SqlString(a), SqlString(b)) => Some(a.cmp(b)),
        (String(a), SqlString(b)) => Some(a.as_str().cmp(b.str())),
        (SqlString(a), String(b)) => Some(a.str().cmp(b.as_str())),
        (Record(a), Record(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn arith(op: BinOp, l: &DynValue, r: &DynValue) -> DynValue {
    use DynValue::*;
    // Strings only support `+` as concatenation.
    if let (BinOp::Add, Some(a), Some(b)) = (op, as_str(l), as_str(r)) {
        return DynValue::str(&format!("{a}{b}"));
    }
    match (l, r) {
        (I64(a), I64(b)) => match op {
            BinOp::Add => a.checked_add(*b).map(I64).unwrap_or(Absent),
            BinOp::Sub => a.checked_sub(*b).map(I64).unwrap_or(Absent),
            BinOp::Mul => a.checked_mul(*b).map(I64).unwrap_or(Absent),
            BinOp::Div => a.checked_div(*b).map(I64).unwrap_or(Absent),
            BinOp::Rem => a.checked_rem(*b).map(I64).unwrap_or(Absent),
            _ => Absent,
        },
        _ => match (as_f64(l), as_f64(r)) {
            (Some(a), Some(b)) => {
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Rem => a % b,
                    _ => return Absent,
                };
                F64(Flt::new(v))
            }
            _ => Absent,
        },
    }
}

fn as_f64(v: &DynValue) -> Option<f64> {
    match v {
        DynValue::I64(n) => Some(*n as f64),
        DynValue::F64(f) => Some(f.into_inner()),
        _ => None,
    }
}

fn as_str(v: &DynValue) -> Option<&str> {
    match v {
        DynValue::String(s) => Some(s.as_str()),
        DynValue::SqlString(s) => Some(s.str()),
        _ => None,
    }
}

fn eval_call(f: Builtin, call_args: &[TypedExpr], args: &[&DynValue]) -> DynValue {
    let vals: Vec<DynValue> = call_args.iter().map(|a| eval(a, args)).collect();

    // `coalesce` is the one builtin that inspects absence rather than being
    // rejected for it; everything else needs a definite value.
    if f == Builtin::Coalesce {
        return if vals[0].is_absent() { vals[1].clone() } else { vals[0].clone() };
    }

    if vals.iter().any(|v| v.is_absent()) {
        return DynValue::Absent;
    }

    match f {
        Builtin::Abs => match &vals[0] {
            DynValue::I64(n) => n.checked_abs().map(DynValue::I64).unwrap_or(DynValue::Absent),
            DynValue::F64(v) => DynValue::F64(Flt::new(v.into_inner().abs())),
            _ => DynValue::Absent,
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
            _ => DynValue::Absent,
        },
        Builtin::Length => match &vals[0] {
            DynValue::String(s) => DynValue::I64(s.chars().count() as i64),
            DynValue::SqlString(s) => DynValue::I64(s.str().chars().count() as i64),
            DynValue::Record(fields) => DynValue::I64(fields.len() as i64),
            _ => DynValue::Absent,
        },
        Builtin::Concat => match (as_str(&vals[0]), as_str(&vals[1])) {
            (Some(a), Some(b)) => DynValue::str(&format!("{a}{b}")),
            _ => DynValue::Absent,
        },
        Builtin::Lower | Builtin::Upper | Builtin::Trim => match as_str(&vals[0]) {
            Some(s) => {
                let out = match f {
                    Builtin::Lower => s.to_lowercase(),
                    Builtin::Upper => s.to_uppercase(),
                    _ => s.trim().to_string(),
                };
                DynValue::SqlString(SqlString::from_ref(&out))
            }
            None => DynValue::Absent,
        },
        Builtin::Coalesce => unreachable!("handled above"),
    }
}
