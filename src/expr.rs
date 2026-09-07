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
        Builtin::Coalesce => unreachable!("handled above"),
    }
}
