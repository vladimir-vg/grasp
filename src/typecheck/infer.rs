//! Typing expressions — the bodies of `fun(...)` arguments.
//!
//! Separate from operator checking: this decides what an *expression* is worth,
//! while `ops` decides what an operator does with the streams it is given.

use super::{TResult, err};
use std::fmt;
use crate::diag::Span;
use crate::expr::{Builtin, TypedExpr};
use crate::lang::{BinOp, Expr, ExprKind, FunLit, UnOp};
use crate::value::{DynValue, TypeDesc};

/// The type of an expression. `None` is the type of a bare `null` literal: it
/// has no type of its own and takes one from context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ty {
    Known(TypeDesc),
    None,
}

impl Ty {
    pub(super) fn into_known(self, span: Span, what: impl fmt::Display) -> TResult<TypeDesc> {
        match self {
            Ty::Known(t) => Ok(t),
            Ty::None => err(span, format!("cannot infer a type for `NONE` in {what}")),
        }
    }
}

/// Unifies two types, as `coalesce` needs. An none side makes the other
/// side's type optional.
fn unify(a: Ty, b: Ty, span: Span) -> TResult<Ty> {
    Ok(match (a, b) {
        (Ty::None, Ty::None) => Ty::None,
        (Ty::None, Ty::Known(t)) | (Ty::Known(t), Ty::None) => Ty::Known(optional(t)),
        (Ty::Known(x), Ty::Known(y)) => {
            if x == y {
                Ty::Known(x)
            } else if x.non_null() == y.non_null() {
                Ty::Known(optional(x.non_null().clone()))
            } else if is_numeric(&x) && is_numeric(&y) {
                // Mixing i64 and f64 promotes to f64.
                let t = if x.is_optional() || y.is_optional() {
                    optional(TypeDesc::F64)
                } else {
                    TypeDesc::F64
                };
                Ty::Known(t)
            } else {
                return err(span, format!("incompatible types `{x}` and `{y}`"));
            }
        }
    })
}

pub(super) fn optional(t: TypeDesc) -> TypeDesc {
    if t.is_optional() { t } else { TypeDesc::Optional(Box::new(t)) }
}

pub(super) fn is_numeric(t: &TypeDesc) -> bool {
    matches!(t.non_null(), TypeDesc::I64 | TypeDesc::F64)
}

pub(super) fn is_stringy(t: &TypeDesc) -> bool {
    matches!(t.non_null(), TypeDesc::String | TypeDesc::SqlString)
}

pub(super) fn check_fun(fun: &FunLit, params: &[TypeDesc], span: Span) -> TResult<(TypedExpr, Ty)> {
    check_fun_body(fun, &fun.body, params, span)
}

pub(super) fn check_fun_body(
    fun: &FunLit,
    body: &Expr,
    params: &[TypeDesc],
    span: Span,
) -> TResult<(TypedExpr, Ty)> {
    if fun.params.len() != params.len() {
        return err(
            span,
            format!(
                "this function takes {} parameter(s) but the operator supplies {}",
                fun.params.len(),
                params.len()
            ),
        );
    }
    let env: Vec<(&str, &TypeDesc)> = fun
        .params
        .iter()
        .map(|s| s.as_str())
        .zip(params.iter())
        .collect();
    infer(body, &env)
}

fn infer(e: &Expr, env: &[(&str, &TypeDesc)]) -> TResult<(TypedExpr, Ty)> {
    // Every diagnostic below is reported against the offending expression, not
    // the enclosing declaration.
    let span = e.span;
    Ok(match &e.kind {
        ExprKind::None => (TypedExpr::Const(DynValue::None), Ty::None),
        ExprKind::Bool(b) => (TypedExpr::Const(DynValue::Bool(*b)), Ty::Known(TypeDesc::Bool)),
        ExprKind::Int(v) => (TypedExpr::Const(DynValue::I64(*v)), Ty::Known(TypeDesc::I64)),
        ExprKind::Float(v) => (
            TypedExpr::Const(DynValue::F64(dbsp::algebra::F64::new(*v))),
            Ty::Known(TypeDesc::F64),
        ),
        ExprKind::Str(s) => (TypedExpr::Const(DynValue::str(s)), Ty::Known(TypeDesc::SqlString)),

        ExprKind::Var(name) => {
            let Some(i) = env.iter().position(|(n, _)| n == name) else {
                return err(span, format!("unknown parameter `{name}`"));
            };
            (TypedExpr::Var(i), Ty::Known(env[i].1.clone()))
        }

        ExprKind::Field(base, field) => {
            let (be, bt) = infer(base, env)?;
            let bt = bt.into_known(span, "a field access")?;
            let rec = bt.non_null();
            let Some(index) = rec.field_index(field) else {
                return err(span, format!("`{rec}` has no field `{field}`"));
            };
            let fty = rec.field_type(field).unwrap().clone();
            // Reading a field of a possibly-null record yields a possibly-null
            // value.
            let fty = if bt.is_optional() { optional(fty) } else { fty };
            (TypedExpr::Field(Box::new(be), index), Ty::Known(fty))
        }

        ExprKind::Record(fields) => {
            let mut exprs = Vec::with_capacity(fields.len());
            let mut types = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                let (te, ty) = infer(value, env)?;
                let ty = ty.into_known(span, format!("field `{name}`"))?;
                exprs.push(te);
                types.push((name.clone(), ty));
            }
            (TypedExpr::Record(exprs), Ty::Known(TypeDesc::Record(types)))
        }

        ExprKind::Tuple(_) => {
            return err(
                span,
                "a `(key, value)` pair is only allowed as the body of a `map_index` \
                 or `join_index` function, or as an element of a `flat_map_index` list",
            );
        }

        ExprKind::List(_) => {
            return err(
                span,
                "a list is only allowed as the body of a `flat_map` or \
                 `flat_map_index` function",
            );
        }

        ExprKind::Unary(op, inner) => {
            let (ie, it) = infer(inner, env)?;
            // Like arithmetic, both unary operators need a definite value.
            let name = match op {
                UnOp::Neg => "-",
                UnOp::Not => "not",
            };
            let ty = match it {
                Ty::None => {
                    return err(span, format!("`{name}` needs a value, but this is `NONE`"));
                }
                Ty::Known(t) if t.is_optional() => {
                    return err(
                        span,
                        format!(
                            "`{name}` needs a value, but `{t}` may be none; \
                             use `coalesce` to supply a default first"
                        ),
                    );
                }
                Ty::Known(t) => {
                    let ok = match op {
                        UnOp::Neg => is_numeric(&t),
                        UnOp::Not => t == TypeDesc::Bool,
                    };
                    if !ok {
                        return err(span, format!("cannot apply `{name}` to `{t}`"));
                    }
                    Ty::Known(t)
                }
            };
            (TypedExpr::Unary(*op, Box::new(ie)), ty)
        }

        ExprKind::Binary(op, l, r) => {
            let (le, lt) = infer(l, env)?;
            let (re, rt) = infer(r, env)?;
            let ty = infer_binop(*op, lt, rt, span)?;
            (TypedExpr::Binary(*op, Box::new(le), Box::new(re)), ty)
        }

        ExprKind::Call(name, args) => {
            let Some(builtin) = Builtin::from_name(name) else {
                return err(span, format!("unknown function `{name}`"));
            };
            if let Some(arity) = builtin.arity()
                && args.len() != arity {
                    return err(
                        span,
                        format!("`{name}` takes {arity} argument(s), found {}", args.len()),
                    );
                }
            let mut exprs = Vec::with_capacity(args.len());
            let mut types = Vec::with_capacity(args.len());
            for a in args {
                let (te, ty) = infer(a, env)?;
                exprs.push(te);
                types.push(ty);
            }
            let ty = infer_builtin(builtin, name, &types, span)?;
            (TypedExpr::Call(builtin, exprs), ty)
        }
    })
}

fn infer_binop(op: BinOp, lt: Ty, rt: Ty, span: Span) -> TResult<Ty> {
    use BinOp::*;

    /// Operators that need a definite value reject an optional operand rather
    /// than yielding absence, which would be propagation by another name.
    fn definite(t: &Ty, op: &str, span: Span) -> TResult<()> {
        match t {
            Ty::None => err(
                span,
                format!("`{op}` needs a value, but this is `NONE`"),
            ),
            Ty::Known(t) if t.is_optional() => err(
                span,
                format!(
                    "`{op}` needs a value, but `{t}` may be none; \
                     use `coalesce` to supply a default first"
                ),
            ),
            Ty::Known(_) => Ok(()),
        }
    }

    match op {
        And | Or => {
            for t in [&lt, &rt] {
                definite(t, if op == And { "and" } else { "or" }, span)?;
                if let Ty::Known(t) = t
                    && t != &TypeDesc::Bool
                {
                    return err(span, format!("`and`/`or` need bool operands, found `{t}`"));
                }
            }
            Ok(Ty::Known(TypeDesc::Bool))
        }

        // Comparisons are total. `NONE` is a value: it equals itself, sorts
        // before every other value, and comparing against it always decides.
        // So the result is a plain bool even when an operand is optional, and
        // `filter` can consume it directly.
        Eq | Ne | Lt | Le | Gt | Ge => {
            if let (Ty::Known(a), Ty::Known(b)) = (&lt, &rt) {
                let comparable = a.non_null() == b.non_null()
                    || (is_numeric(a) && is_numeric(b))
                    || (is_stringy(a) && is_stringy(b));
                if !comparable {
                    return err(span, format!("cannot compare `{a}` with `{b}`"));
                }
            }
            Ok(Ty::Known(TypeDesc::Bool))
        }

        Add | Sub | Mul | Div | Rem => {
            let name = match op {
                Add => "+",
                Sub => "-",
                Mul => "*",
                Div => "/",
                _ => "%",
            };
            definite(&lt, name, span)?;
            definite(&rt, name, span)?;
            let (Ty::Known(a), Ty::Known(b)) = (&lt, &rt) else {
                unreachable!("definite() rejected the none cases");
            };
            // `+` doubles as string concatenation.
            if op == Add && is_stringy(a) && is_stringy(b) {
                return Ok(Ty::Known(TypeDesc::SqlString));
            }
            if !is_numeric(a) || !is_numeric(b) {
                return err(
                    span,
                    format!("cannot apply `{name}` to `{a}` and `{b}`"),
                );
            }
            Ok(Ty::Known(if a == &TypeDesc::F64 || b == &TypeDesc::F64 {
                TypeDesc::F64
            } else {
                TypeDesc::I64
            }))
        }
    }
}

fn infer_builtin(b: Builtin, name: &str, args: &[Ty], span: Span) -> TResult<Ty> {
    let known = |i: usize| -> Option<&TypeDesc> {
        match &args[i] {
            Ty::Known(t) => Some(t),
            Ty::None => None,
        }
    };
    Ok(match b {
        Builtin::Coalesce => {
            // The result is non-null when the fallback is.
            let a = args[0].clone();
            let b2 = args[1].clone();
            match (a, b2) {
                (Ty::Known(x), Ty::Known(y)) => {
                    
                    unify(
                        Ty::Known(x.non_null().clone()),
                        Ty::Known(y.clone()),
                        span,
                    )?
                }
                (Ty::None, other) | (other, Ty::None) => other,
            }
        }
        Builtin::Length => Ty::Known(TypeDesc::I64),
        Builtin::Concat | Builtin::Lower | Builtin::Upper | Builtin::Trim => {
            if let Some(t) = known(0)
                && !is_stringy(t) {
                    return err(span, format!("`{name}` needs a string, found `{t}`"));
                }
            Ty::Known(TypeDesc::SqlString)
        }
        Builtin::Abs | Builtin::Floor | Builtin::Ceil | Builtin::Round => match known(0) {
            Some(t) if is_numeric(t) => Ty::Known(t.clone()),
            Some(t) => return err(span, format!("`{name}` needs a number, found `{t}`")),
            None => Ty::None,
        },
    })
}
