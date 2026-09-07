//! Typing expressions — the bodies of `fun(...)` arguments.
//!
//! Separate from operator checking: this decides what an *expression* is worth,
//! while `ops` decides what an operator does with the streams it is given.

use super::{Functions, TResult, err};
use std::fmt;
use crate::diag::Span;
use crate::expr::{Builtin, Conv, TypedExpr};
use crate::lang::{BinOp, Expr, ExprKind, UnOp};
use crate::value::{DynValue, TypeDesc};

/// The type of an expression.
///
/// Three of the four cases have no type of their own and take one from context.
/// That is the language's single mechanism for deferred typing — the same one
/// `empty()` uses for streams — rather than a solver: a literal is resolved by
/// the operand beside it, in one bottom-up pass, and settles to a default only
/// where nothing else decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ty {
    Known(TypeDesc),
    /// The `NONE` literal.
    None,
    /// An integer literal, which inhabits any numeric type.
    Int,
    /// A float literal, which inhabits any floating type.
    Float,
}

impl Ty {
    /// The concrete type this settles to with no context: `NONE` alone has
    /// none, and a bare numeric literal defaults.
    fn settle(&self) -> Option<TypeDesc> {
        match self {
            Ty::Known(t) => Some(t.clone()),
            Ty::Int => Some(TypeDesc::I64),
            Ty::Float => Some(TypeDesc::F64),
            Ty::None => Option::None,
        }
    }
}

/// Rewrites the unpinned numeric literals inside `e` as values of `ty`.
///
/// Only reached for a subexpression whose own type was still a literal, so the
/// leaves it rewrites are exactly the literals that were waiting for context.
fn pin(e: &mut TypedExpr, ty: &TypeDesc) {
    match e {
        TypedExpr::IntLit(v) => {
            *e = TypedExpr::Const(match ty.non_null() {
                TypeDesc::F64 => DynValue::F64(dbsp::algebra::F64::new(*v as f64)),
                _ => DynValue::I64(*v),
            });
        }
        TypedExpr::FloatLit(v) => {
            *e = TypedExpr::Const(DynValue::F64(dbsp::algebra::F64::new(*v)));
        }
        // The nodes that can carry an unpinned type upward. `Record` and
        // `Field` cannot: their types are concrete, so their children were
        // committed already and must not be rewritten from outside.
        TypedExpr::Unary(_, inner) => pin(inner, ty),
        TypedExpr::Binary(_, l, r) => {
            pin(l, ty);
            pin(r, ty);
        }
        TypedExpr::Call(_, args) => {
            for a in args {
                pin(a, ty);
            }
        }
        TypedExpr::Const(_)
        | TypedExpr::Var(_)
        | TypedExpr::Field(..)
        | TypedExpr::Record(_)
        | TypedExpr::Array(_)
        | TypedExpr::Cast(..) => {}
    }
}

/// Settles an expression and its type together: this is the one place a type
/// must become concrete, so it is also the one place a literal's default
/// applies. Keeping the two in one function is what makes it impossible to
/// commit a type without pinning the expression that carries it.
pub(super) fn commit(
    e: &mut TypedExpr,
    ty: Ty,
    span: Span,
    what: impl fmt::Display,
) -> TResult<TypeDesc> {
    match ty.settle() {
        Some(t) => {
            pin(e, &t);
            Ok(t)
        }
        Option::None => err(span, format!("cannot infer a type for `NONE` in {what}")),
    }
}

/// The one rule for bringing two types together, shared by arithmetic,
/// comparison and `coalesce`.
///
/// **There is no implicit conversion.** Two known types unify only if they are
/// the same underlying type; `i64` and `f64` do not meet. That is what keeps
/// typing predictable enough for an emitter to compute a result type without
/// running the checker, and it is what lets one function body serve every
/// numeric type — the literals in it take the type of whatever they are used
/// with, rather than dragging the expression to `f64`.
fn unify(a: Ty, b: Ty, span: Span) -> TResult<Ty> {
    use Ty::*;
    Ok(match (a, b) {
        (None, None) => None,
        (None, Known(t)) | (Known(t), None) => Known(optional(t)),

        // A literal takes the other side's type, if that type can hold it.
        (Int, Int) => Int,
        (Float, Float) | (Int, Float) | (Float, Int) => Float,
        (None, Int) | (Int, None) => Known(optional(TypeDesc::I64)),
        (None, Float) | (Float, None) => Known(optional(TypeDesc::F64)),
        (Int, Known(t)) | (Known(t), Int) => {
            if !is_numeric(&t) {
                return err(span, format!("an integer literal has no `{t}` value"));
            }
            Known(t)
        }
        (Float, Known(t)) | (Known(t), Float) => {
            if t.non_null() != &TypeDesc::F64 {
                return err(span, format!("a float literal has no `{t}` value"));
            }
            Known(t)
        }

        (Known(x), Known(y)) => {
            if x == y {
                Known(x)
            } else if x.non_null() == y.non_null() {
                Known(optional(x.non_null().clone()))
            } else if is_numeric(&x) && is_numeric(&y) {
                return err(
                    span,
                    format!(
                        "`{x}` and `{y}` are different types and there is no implicit \
                         conversion; a literal would take either type, but a value \
                         must already be the one you need"
                    ),
                );
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
    matches!(t.non_null(), TypeDesc::String)
}

/// Checks one function body against the types at a call site, and settles its
/// type. `what` names the position for the diagnostic, and reaching a concrete
/// type here is also what pins any literal the body left waiting for context.
pub(super) fn check_fun(
    names: &[String],
    body: &Expr,
    params: &[TypeDesc],
    span: Span,
    what: impl fmt::Display,
    funcs: &Functions<'_>,
) -> TResult<(TypedExpr, TypeDesc)> {
    if names.len() != params.len() {
        return err(
            span,
            format!(
                "this function takes {} parameter(s) but the operator supplies {}",
                names.len(),
                params.len()
            ),
        );
    }
    let env: Vec<(&str, &TypeDesc)> =
        names.iter().map(|s| s.as_str()).zip(params.iter()).collect();
    let (mut e, ty) = infer(body, &env, funcs)?;
    let t = commit(&mut e, ty, body.span, what)?;
    Ok((e, t))
}

/// Replaces each parameter reference in an instantiated body with the argument
/// written at the call site.
///
/// This is the whole of inlining. An argument used twice in the body is
/// therefore evaluated twice; sharing it would need a slot table, which is what
/// body bindings will bring.
fn substitute(e: &TypedExpr, args: &[TypedExpr]) -> TypedExpr {
    match e {
        TypedExpr::Var(i) => args[*i].clone(),
        TypedExpr::Field(b, i) => TypedExpr::Field(Box::new(substitute(b, args)), *i),
        TypedExpr::Record(f) => TypedExpr::Record(f.iter().map(|x| substitute(x, args)).collect()),
        TypedExpr::Array(f) => TypedExpr::Array(f.iter().map(|x| substitute(x, args)).collect()),
        TypedExpr::Unary(op, i) => TypedExpr::Unary(*op, Box::new(substitute(i, args))),
        TypedExpr::Binary(op, l, r) => TypedExpr::Binary(
            *op,
            Box::new(substitute(l, args)),
            Box::new(substitute(r, args)),
        ),
        TypedExpr::Call(f, a) => {
            TypedExpr::Call(*f, a.iter().map(|x| substitute(x, args)).collect())
        }
        TypedExpr::Cast(i, c) => TypedExpr::Cast(Box::new(substitute(i, args)), c.clone()),
        leaf @ (TypedExpr::Const(_) | TypedExpr::IntLit(_) | TypedExpr::FloatLit(_)) => leaf.clone(),
    }
}

fn infer(e: &Expr, env: &[(&str, &TypeDesc)], funcs: &Functions<'_>) -> TResult<(TypedExpr, Ty)> {
    // Every diagnostic below is reported against the offending expression, not
    // the enclosing declaration.
    let span = e.span;
    Ok(match &e.kind {
        ExprKind::None => (TypedExpr::Const(DynValue::None), Ty::None),
        ExprKind::Bool(b) => (TypedExpr::Const(DynValue::Bool(*b)), Ty::Known(TypeDesc::Bool)),
        // A numeric literal has no type of its own; the operand beside it
        // decides, and `commit` supplies the default where nothing does.
        ExprKind::Int(v) => (TypedExpr::IntLit(*v), Ty::Int),
        ExprKind::Float(v) => (TypedExpr::FloatLit(*v), Ty::Float),
        ExprKind::Str(s) => (TypedExpr::Const(DynValue::str(s)), Ty::Known(TypeDesc::String)),

        ExprKind::Var(name) => {
            let Some(i) = env.iter().position(|(n, _)| n == name) else {
                return err(span, format!("unknown parameter `{name}`"));
            };
            (TypedExpr::Var(i), Ty::Known(env[i].1.clone()))
        }

        ExprKind::Field(base, field) => {
            let (mut be, bt) = infer(base, env, funcs)?;
            let bt = commit(&mut be, bt, span, "a field access")?;
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

        ExprKind::Cast(inner, to) => {
            let (mut ie, it) = infer(inner, env, funcs)?;
            let conv = conversion(&it, to, span)?;
            // A literal operand settles before converting, so `cast(1, f64)`
            // goes i64 -> f64 rather than the literal simply being an f64.
            // Predictable, and it keeps one rule for what a literal does.
            if !matches!(it, Ty::None) {
                commit(&mut ie, it, span, "the value being cast")?;
            }
            (TypedExpr::Cast(Box::new(ie), conv), Ty::Known(to.clone()))
        }

        ExprKind::Record(fields) => {
            let mut exprs = Vec::with_capacity(fields.len());
            let mut types = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                let (mut te, ty) = infer(value, env, funcs)?;
                let ty = commit(&mut te, ty, span, format!("field `{name}`"))?;
                exprs.push(te);
                types.push((name.clone(), ty));
            }
            (TypedExpr::Record(exprs), Ty::Known(TypeDesc::Record(types)))
        }

        ExprKind::List(items) => {
            let mut exprs = Vec::with_capacity(items.len());
            let mut elem: Option<Ty> = Option::None;
            for item in items {
                let (te, ty) = infer(item, env, funcs)?;
                // One element type for the whole array, by the same `unify`
                // arithmetic and comparison use.
                elem = Some(match elem {
                    Option::None => ty,
                    Some(prev) => unify(prev, ty, item.span)?,
                });
                exprs.push(te);
            }
            let Some(elem) = elem else {
                return err(
                    span,
                    "an empty array has no element type; write at least one element, \
                     or bind the array to a name carrying an `array(...)` typespec",
                );
            };
            // Settling the first element settles the array, and the rest follow
            // it — so a literal in any position takes the same type as the others.
            let elem = commit(&mut exprs[0], elem, span, "an array element")?;
            for e in exprs.iter_mut().skip(1) {
                pin(e, elem.non_null());
            }
            (TypedExpr::Array(exprs), Ty::Known(TypeDesc::Array(Box::new(elem))))
        }

        ExprKind::Unary(op, inner) => {
            let (ie, it) = infer(inner, env, funcs)?;
            // Like arithmetic, both unary operators need a definite value.
            let name = match op {
                UnOp::Neg => "-",
                UnOp::Not => "not",
            };
            let ty = match it {
                Ty::None => {
                    return err(span, format!("`{name}` needs a value, but this is `NONE`"));
                }
                // A negated literal is still a literal, so `-1` takes its type
                // from whatever it is used with.
                lit @ (Ty::Int | Ty::Float) => {
                    if *op == UnOp::Not {
                        return err(span, format!("cannot apply `{name}` to a number"));
                    }
                    lit
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
            let (mut le, lt) = infer(l, env, funcs)?;
            let (mut re, rt) = infer(r, env, funcs)?;
            let ty = infer_binop(*op, &mut le, lt, &mut re, rt, span)?;
            (TypedExpr::Binary(*op, Box::new(le), Box::new(re)), ty)
        }

        ExprKind::Call(name, args) => {
            let Some(builtin) = Builtin::from_name(name) else {
                let Some(def) = funcs.get(name.as_str()) else {
                    return err(span, format!("unknown function `{name}`"));
                };
                return instantiate(def, args, env, funcs, span);
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
                let (te, ty) = infer(a, env, funcs)?;
                exprs.push(te);
                types.push(ty);
            }
            let ty = infer_builtin(builtin, name, &mut exprs, &types, span)?;
            (TypedExpr::Call(builtin, exprs), ty)
        }
    })
}

/// Whether a document can be extracted as this type.
///
/// Everything in the vocabulary except a nested `optional`, which the type
/// grammar already forbids — the list is written out so that adding a type
/// forces a decision here rather than silently inheriting one.
fn extractable(t: &TypeDesc) -> bool {
    match t {
        TypeDesc::Bool | TypeDesc::I64 | TypeDesc::F64 | TypeDesc::String | TypeDesc::Json => true,
        TypeDesc::Record(fields) => fields.iter().all(|(_, f)| extractable(f.non_null())),
        TypeDesc::Array(elem) => extractable(elem.non_null()),
        TypeDesc::Optional(_) => false,
    }
}

/// The conversion `cast(x, to)` means, or why there is none.
///
/// **`cast(x, T)` yields exactly `T`.** Where the conversion has inputs the
/// target cannot hold, that is an error naming the fix rather than a silently
/// optional result — which is what keeps a declared type a promise, and follows
/// the rule division already set.
///
/// An `optional` *target* is what admits an absent input, so absence needs no
/// propagation rule of its own: the written type says whether it is allowed
/// through.
fn conversion(from: &Ty, to: &TypeDesc, span: Span) -> TResult<Conv> {
    use TypeDesc::*;
    let target = to.non_null();
    let optional_target = to.is_optional();

    // `NONE` has no type of its own; the target gives it one, exactly as it
    // does for a numeric literal. This is how a definite value's absent
    // counterpart is written.
    let from = match from {
        Ty::None => {
            return if optional_target {
                Ok(Conv::Identity)
            } else {
                err(span, format!("`NONE` is not a `{to}`; write `cast(NONE, optional({target}))`"))
            };
        }
        other => other.settle().expect("Ty::None handled above"),
    };

    if from.is_optional() && !optional_target {
        return err(
            span,
            format!(
                "`{from}` may be absent, so it cannot become `{to}`; \
                 write `cast(..., optional({target}))`"
            ),
        );
    }

    let source = from.non_null();

    // A document is converted by what is *wanted*, not by what it happens to
    // hold, so these are two rows rather than a matrix. Extraction is always
    // fallible — a document need not have the shape asked of it — and building
    // one always succeeds.
    if source == &TypeDesc::Json && target != &TypeDesc::Json {
        if !extractable(target) {
            return err(
                span,
                format!("a document cannot be extracted as `{target}`"),
            );
        }
        if !optional_target {
            return err(
                span,
                format!(
                    "extracting `{target}` from a document can fail — it need not hold \
                     that shape; write `cast(..., optional({target}))`"
                ),
            );
        }
        return Ok(Conv::FromJson(std::sync::Arc::new(target.clone())));
    }
    if target == &TypeDesc::Json && source != &TypeDesc::Json {
        return Ok(Conv::ToJson(std::sync::Arc::new(from.clone())));
    }

    let conv = match (source, target) {
        (a, b) if a == b => Conv::Identity,
        (I64, F64) => Conv::IntToFloat,
        (F64, I64) => Conv::FloatToInt,
        (Bool, String) => Conv::BoolToString,
        (I64, String) => Conv::IntToString,
        (F64, String) => Conv::FloatToString,
        (String, Bool) => Conv::StringToBool,
        (String, I64) => Conv::StringToInt,
        (String, F64) => Conv::StringToFloat,
        _ => {
            return err(span, format!("there is no conversion from `{source}` to `{target}`"));
        }
    };

    if conv.is_fallible() && !optional_target {
        let why = match conv {
            Conv::FloatToInt => "NaN, infinity, or a value outside `i64`",
            _ => "the text may not parse",
        };
        return err(
            span,
            format!(
                "converting `{source}` to `{target}` can fail ({why}); \
                 write `cast(..., optional({target}))`"
            ),
        );
    }
    Ok(conv)
}

/// Instantiates a named function at one call site.
///
/// A function is a **template**: its parameters carry no types, so the body is
/// checked afresh against the types written here. The same `scale(x)` is `i64`
/// beside an `i64` and `f64` beside an `f64`, because the literals inside it
/// resolve against the parameter type like any other operand.
///
/// Two consequences worth taking deliberately. A function nobody calls is never
/// checked. And an error in a body is caused by a *call site*, so the
/// diagnostic names it — the span still points into the body, which is where
/// the offending expression is.
fn instantiate(
    def: &crate::lang::FunctionDef,
    args: &[Expr],
    env: &[(&str, &TypeDesc)],
    funcs: &Functions<'_>,
    span: Span,
) -> TResult<(TypedExpr, Ty)> {
    if def.params.len() != args.len() {
        return err(
            span,
            format!(
                "`{}` takes {} argument(s), found {}",
                def.name,
                def.params.len(),
                args.len()
            ),
        );
    }

    // The argument types are what the body is checked against, so each must be
    // concrete: a bare literal argument settles to its default here rather than
    // staying open across the call.
    let mut arg_exprs = Vec::with_capacity(args.len());
    let mut param_types = Vec::with_capacity(args.len());
    for (a, p) in args.iter().zip(&def.params) {
        let (mut e, ty) = infer(a, env, funcs)?;
        param_types.push(commit(&mut e, ty, a.span, format!("argument `{p}` of `{}`", def.name))?);
        arg_exprs.push(e);
    }

    let body_env: Vec<(&str, &TypeDesc)> = def
        .params
        .iter()
        .map(|s| s.as_str())
        .zip(param_types.iter())
        .collect();
    let (body, ty) = infer(&def.body, &body_env, funcs).map_err(|mut d| {
        d.message = format!("in `{}`, instantiated at {span}: {}", def.name, d.message);
        d
    })?;
    Ok((substitute(&body, &arg_exprs), ty))
}

/// Operations that need a definite value reject an optional operand rather than
/// yielding absence, which would be propagation by another name.
fn definite(t: &Ty, op: &str, span: Span) -> TResult<()> {
    match t {
        Ty::None => err(span, format!("`{op}` needs a value, but this is `NONE`")),
        // A literal is a definite value; it just has not chosen a type yet.
        Ty::Int | Ty::Float => Ok(()),
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

fn infer_binop(
    op: BinOp,
    le: &mut TypedExpr,
    lt: Ty,
    re: &mut TypedExpr,
    rt: Ty,
    span: Span,
) -> TResult<Ty> {
    use BinOp::*;

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
            // The two sides must still meet at one type, by the same `unify`
            // arithmetic uses — so there is one answer in the language to
            // "can these two types come together", not two.
            let shared = unify(lt, rt, span).map_err(|mut d| {
                d.message = format!("cannot compare them: {}", d.message);
                d
            })?;
            // Operands settle here rather than outward, because the result is
            // `bool` and carries no numeric type to the enclosing expression.
            if let Some(t) = shared.settle() {
                // Documents may be compared for equality — the encoding is
                // canonical, so that is sound — but not ordered. `cmp_values`
                // compares the type tag first, making the order well defined and
                // arbitrary, and this language's ordering decides query results
                // rather than only batch layout.
                if t.non_null() == &TypeDesc::Json && !matches!(op, Eq | Ne) {
                    return err(
                        span,
                        "documents have no meaningful order: comparison sorts by type tag, \
                         so every number would precede every string. Only `==` and `!=` are \
                         allowed; extract a value with `cast` and compare that.",
                    );
                }
                pin(le, t.non_null());
                pin(re, t.non_null());
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
            let shared = unify(lt, rt, span)?;
            if let Ty::Known(t) = &shared
                && !is_numeric(t)
            {
                return err(span, format!("cannot apply `{name}` to `{t}`"));
            }

            // A zero divisor has no value to return, so `/` and `%` are the one
            // arithmetic whose result may be absent. Everything else is total:
            // integer overflow wraps, float arithmetic is IEEE.
            //
            // Division also settles its operands, because `optional` has to wrap
            // a concrete type — `1 / 2` cannot stay open the way `1 + 2` does.
            if matches!(op, Div | Rem) {
                let t = shared.settle().expect("definite() rejected the none case");
                pin(le, t.non_null());
                pin(re, t.non_null());
                return Ok(Ty::Known(optional(t)));
            }
            if let Ty::Known(t) = &shared {
                pin(le, t.non_null());
                pin(re, t.non_null());
            }
            // Two literals stay open, so `x * (1 + 2)` is whatever `x` is.
            Ok(shared)
        }
    }
}

fn infer_builtin(
    b: Builtin,
    name: &str,
    exprs: &mut [TypedExpr],
    args: &[Ty],
    span: Span,
) -> TResult<Ty> {
    Ok(match b {
        // The condition is an ordinary definite `bool` — comparisons already
        // yield one even when an operand is optional. The arms meet under the
        // same `unify` as everything else, so `if(c, x, NONE)` widens to
        // `optional(T)` and `if(c, i64_val, f64_val)` is rejected like any
        // other mixed-type expression.
        Builtin::If => {
            definite(&args[0], name, span)?;
            let cond = args[0].settle().expect("definite() rejected the none case");
            if cond != TypeDesc::Bool {
                return err(span, format!("`if` needs a bool condition, found `{cond}`"));
            }
            let out = unify(args[1].clone(), args[2].clone(), span).map_err(|mut d| {
                d.message = format!("`if`'s two arms must agree: {}", d.message);
                d
            })?;
            if let Some(t) = out.settle() {
                for e in exprs.iter_mut().skip(1) {
                    pin(e, t.non_null());
                }
            }
            out
        }

        // Navigation into a document. `get` takes two different key types, the
        // way `length` takes a string or an array: a string names an object
        // member, an `i64` a 0-based array element.
        //
        // It is also the one builtin whose first argument may be absent, so it
        // does not go through `definite`. Chaining is what needs that — the
        // result of one `get` is the input to the next — and absence passing
        // through is not the propagation arithmetic refuses: navigating into
        // nothing has one sensible answer.
        Builtin::Get => {
            let Some(doc) = args[0].settle() else {
                return err(span, "`get` needs a document, but this is `NONE`");
            };
            if doc.non_null() != &TypeDesc::Json {
                return err(span, format!("`get` needs a document, found `{doc}`"));
            }
            definite(&args[1], name, span)?;
            let key = args[1].settle().expect("definite() rejected the none case");
            if !matches!(key.non_null(), TypeDesc::String | TypeDesc::I64) {
                return err(
                    span,
                    format!("`get` needs a string key or an i64 index, found `{key}`"),
                );
            }
            // An `i64` key settles here, so a bare literal index is an index and
            // not something waiting on further context.
            pin(&mut exprs[1], key.non_null());
            // A member may not be there, and the type says so — rather than
            // handing back the encoding's absent sentinel dressed as a document.
            Ty::Known(optional(TypeDesc::Json))
        }

        // `NONE` for anything that is not an object, so "not an object" is not
        // silently the same as "an object with no keys".
        Builtin::Keys => {
            definite(&args[0], name, span)?;
            let doc = args[0].settle().expect("definite() rejected the none case");
            if doc.non_null() != &TypeDesc::Json {
                return err(span, format!("`keys` needs a document, found `{doc}`"));
            }
            Ty::Known(optional(TypeDesc::Array(Box::new(TypeDesc::String))))
        }

        Builtin::Coalesce => {
            // The result is non-null when the fallback is, so the first
            // argument contributes its type without its optionality.
            let first = match args[0].clone() {
                Ty::Known(x) => Ty::Known(x.non_null().clone()),
                other => other,
            };
            let out = unify(first, args[1].clone(), span)?;
            if let Some(t) = out.settle() {
                for e in exprs.iter_mut() {
                    pin(e, t.non_null());
                }
            }
            out
        }
        // Every string builtin checks each argument, so a value that is absent
        // or not a string is rejected here rather than becoming a `NONE` in a
        // column whose type says it cannot be one.
        Builtin::Length | Builtin::Concat | Builtin::Lower | Builtin::Upper | Builtin::Trim => {
            for (i, arg) in args.iter().enumerate() {
                definite(arg, name, span)?;
                let t = arg.settle().expect("definite() rejected the none case");
                let ok = is_stringy(&t)
                    || (b == Builtin::Length && matches!(t.non_null(), TypeDesc::Array(_)));
                if !ok {
                    let n = i + 1;
                    let want = if b == Builtin::Length { "a string or an array" } else { "a string" };
                    return err(span, format!("argument {n} of `{name}` must be {want}, found `{t}`"));
                }
            }
            if b == Builtin::Length { Ty::Known(TypeDesc::I64) } else { Ty::Known(TypeDesc::String) }
        }
        // Type-preserving, so a literal argument keeps its type open: `abs(-1)`
        // is still whatever it is used with.
        Builtin::Abs | Builtin::Floor | Builtin::Ceil | Builtin::Round => {
            definite(&args[0], name, span)?;
            match &args[0] {
                lit @ (Ty::Int | Ty::Float) => lit.clone(),
                Ty::Known(t) if is_numeric(t) => Ty::Known(t.clone()),
                Ty::Known(t) => return err(span, format!("`{name}` needs a number, found `{t}`")),
                Ty::None => unreachable!("definite() rejected the none case"),
            }
        }
    })
}
