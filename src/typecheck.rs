//! Type inference and name resolution.
//!
//! Produces a [`Plan`]: nodes in dependency order, each with a [`BatchType`] and
//! an operator whose function arguments have been lowered to [`TypedExpr`].
//!
//! Resolving `row.name` to a positional index happens here. That is what allows
//! record *values* to be positional at runtime, and it is why nothing in the hot
//! path compares field-name strings.

use crate::diag::{Diagnostic, Pass, Span};
use crate::expr::{Builtin, TypedExpr};
use crate::lang::{Arg, BinOp, Decl, Expr, ExprKind, FunLit, OpCall, Program, UnOp};
use crate::value::{BatchType, DynValue, TypeDesc};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

type TResult<T> = Result<T, Diagnostic>;

fn err<T>(span: Span, message: impl Into<String>) -> TResult<T> {
    Err(Diagnostic::error(Pass::Typecheck, span, message))
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Min,
    Max,
}

#[derive(Debug, Clone)]
pub enum PlanOp {
    Input { table: String },
    Map { input: usize, f: Arc<TypedExpr> },
    Filter { input: usize, f: Arc<TypedExpr> },
    MapIndex { input: usize, key: Arc<TypedExpr>, value: Arc<TypedExpr> },
    Join { left: usize, right: usize, f: Arc<TypedExpr> },
    Antijoin { left: usize, right: usize },
    Distinct { input: usize },
    Aggregate { input: usize, agg: Agg, f: Arc<TypedExpr> },
    WeightedCount { input: usize },
    Neg { input: usize },
    Plus { left: usize, right: usize },
    Minus { left: usize, right: usize },
    Sum { inputs: Vec<usize> },
}

#[derive(Debug, Clone)]
pub struct PlanNode {
    pub name: String,
    pub ty: BatchType,
    pub op: PlanOp,
    /// Where the node was declared, so lowering failures have a location.
    pub span: Span,
}

/// A checked program: nodes in dependency order.
#[derive(Debug, Clone)]
pub struct Plan {
    pub nodes: Vec<PlanNode>,
    pub by_name: HashMap<String, usize>,
}

impl Plan {
    pub fn node(&self, name: &str) -> Option<&PlanNode> {
        self.by_name.get(name).map(|i| &self.nodes[*i])
    }

    /// Input nodes, as `(node index, table name)`.
    pub fn inputs(&self) -> Vec<(usize, &str)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| match &n.op {
                PlanOp::Input { table } => Some((i, table.as_str())),
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Expression types
// ---------------------------------------------------------------------------

/// The type of an expression. `Null` is the type of a bare `null` literal: it
/// has no type of its own and takes one from context.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ty {
    Known(TypeDesc),
    Null,
}

impl Ty {
    fn into_known(self, span: Span, what: impl fmt::Display) -> TResult<TypeDesc> {
        match self {
            Ty::Known(t) => Ok(t),
            Ty::Null => err(span, format!("cannot infer a type for `null` in {what}")),
        }
    }
}

/// Unifies two branch types, as `if`/`coalesce` need. A null branch makes the
/// other branch's type nullable.
fn unify(a: Ty, b: Ty, span: Span) -> TResult<Ty> {
    Ok(match (a, b) {
        (Ty::Null, Ty::Null) => Ty::Null,
        (Ty::Null, Ty::Known(t)) | (Ty::Known(t), Ty::Null) => Ty::Known(nullable(t)),
        (Ty::Known(x), Ty::Known(y)) => {
            if x == y {
                Ty::Known(x)
            } else if x.non_null() == y.non_null() {
                Ty::Known(nullable(x.non_null().clone()))
            } else if is_numeric(&x) && is_numeric(&y) {
                // Mixing i64 and f64 promotes to f64.
                let t = if x.is_nullable() || y.is_nullable() {
                    nullable(TypeDesc::F64)
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

fn nullable(t: TypeDesc) -> TypeDesc {
    if t.is_nullable() { t } else { TypeDesc::Option(Box::new(t)) }
}

fn is_numeric(t: &TypeDesc) -> bool {
    matches!(t.non_null(), TypeDesc::I64 | TypeDesc::F64)
}

fn is_stringy(t: &TypeDesc) -> bool {
    matches!(t.non_null(), TypeDesc::String | TypeDesc::SqlString)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn check(program: &Program) -> TResult<Plan> {
    // Collect declarations. Names resolve after the whole program is parsed, so
    // forward references are legal.
    let mut node_decls: Vec<(&String, &OpCall, Span)> = Vec::new();
    let mut specs: HashMap<&str, (&BatchType, Span)> = HashMap::new();
    for decl in &program.decls {
        match decl {
            Decl::Node { name, op, span } => {
                if node_decls.iter().any(|(n, _, _)| *n == name) {
                    return err(*span, format!("`{name}` is declared more than once"));
                }
                node_decls.push((name, op, *span));
            }
            Decl::TypeSpec { name, ty, span } => {
                if specs.insert(name, (ty, *span)).is_some() {
                    return err(*span, format!("`{name}` has more than one typespec"));
                }
            }
        }
    }

    for (name, (_, span)) in &specs {
        if !node_decls.iter().any(|(n, _, _)| n.as_str() == *name) {
            return err(*span, format!("typespec for `{name}`, which is not declared"));
        }
    }

    let order = topo_order(&node_decls)?;

    let mut plan = Plan { nodes: Vec::new(), by_name: HashMap::new() };
    for decl_idx in order {
        let (name, op, span) = node_decls[decl_idx];
        let (ty, plan_op) = check_op(name, op, span, &plan, &specs)?;

        // An explicit typespec on a non-input node is checked, not used to drive
        // inference.
        if let Some((expected, spec_line)) = specs.get(name.as_str())
            && !matches!(plan_op, PlanOp::Input { .. }) && **expected != ty {
                return err(
                    *spec_line,
                    format!("`{name}` is declared as `{expected}` but is inferred as `{ty}`"),
                );
            }

        plan.by_name.insert(name.clone(), plan.nodes.len());
        plan.nodes.push(PlanNode { name: name.clone(), ty, op: plan_op, span });
    }
    Ok(plan)
}

/// Dependency order, rejecting cycles. Recursion needs `delay`, which this cut
/// does not implement, so any cycle is an error rather than a fixpoint.
fn topo_order(decls: &[(&String, &OpCall, Span)]) -> TResult<Vec<usize>> {
    let index: HashMap<&str, usize> =
        decls.iter().enumerate().map(|(i, (n, _, _))| (n.as_str(), i)).collect();

    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        None,
        Active,
        Done,
    }
    let mut marks = vec![Mark::None; decls.len()];
    let mut order = Vec::with_capacity(decls.len());

    // Iterative DFS, so a deep program cannot blow the stack.
    for start in 0..decls.len() {
        if marks[start] != Mark::None {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        while let Some((node, child)) = stack.pop() {
            let (name, op, span) = decls[node];
            if child == 0 {
                if marks[node] == Mark::Done {
                    continue;
                }
                marks[node] = Mark::Active;
            }
            let deps: Vec<&String> = op
                .args
                .iter()
                .filter_map(|a| match a {
                    Arg::Name(n) => Some(n),
                    _ => None,
                })
                .collect();
            if child < deps.len() {
                stack.push((node, child + 1));
                let dep = deps[child];
                // Aggregator names are not node references; skip anything that
                // does not name a declared node and let `check_op` report it.
                if let Some(&d) = index.get(dep.as_str()) {
                    match marks[d] {
                        Mark::Active => {
                            return err(
                                span,
                                format!("`{name}` participates in a cycle through `{dep}`; \
                                         recursion needs `delay`, which is not implemented"),
                            );
                        }
                        Mark::None => stack.push((d, 0)),
                        Mark::Done => {}
                    }
                }
            } else {
                marks[node] = Mark::Done;
                order.push(node);
            }
        }
    }
    Ok(order)
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    plan: &'a Plan,
    span: Span,
}

impl Ctx<'_> {
    fn stream(&self, name: &str) -> TResult<usize> {
        self.plan
            .by_name
            .get(name)
            .copied()
            .ok_or_else(|| {
                Diagnostic::error(Pass::Typecheck, self.span, format!("unknown stream `{name}`"))
            })
    }

    fn zset(&self, name: &str) -> TResult<(usize, TypeDesc)> {
        let i = self.stream(name)?;
        match &self.plan.nodes[i].ty {
            BatchType::ZSet(t) => Ok((i, t.clone())),
            other => err(
                self.span,
                format!("`{name}` is `{other}`, but a flat OrdZSet is required here"),
            ),
        }
    }

    fn indexed(&self, name: &str) -> TResult<(usize, TypeDesc, TypeDesc)> {
        let i = self.stream(name)?;
        match &self.plan.nodes[i].ty {
            BatchType::IndexedZSet(k, v) => Ok((i, k.clone(), v.clone())),
            other => err(
                self.span,
                format!("`{name}` is `{other}`, but an OrdIndexedZSet is required here"),
            ),
        }
    }
}

fn check_op(
    name: &str,
    call: &OpCall,
    span: Span,
    plan: &Plan,
    specs: &HashMap<&str, (&BatchType, Span)>,
) -> TResult<(BatchType, PlanOp)> {
    let cx = Ctx { plan, span };
    let op = call.op.as_str();
    let args = &call.args;

    let want = |n: usize| -> TResult<()> {
        if args.len() == n {
            Ok(())
        } else {
            err(span, format!("`{op}` takes {n} argument(s), found {}", args.len()))
        }
    };
    let stream_arg = |i: usize| -> TResult<&String> {
        match &args[i] {
            Arg::Name(n) => Ok(n),
            _ => err(span, format!("argument {} of `{op}` must name a stream", i + 1)),
        }
    };
    let fun_arg = |i: usize| -> TResult<&FunLit> {
        match &args[i] {
            Arg::Fun(f) => Ok(f),
            _ => err(span, format!("argument {} of `{op}` must be a `fun(...)`", i + 1)),
        }
    };

    match op {
        "input" => {
            want(1)?;
            let Arg::Str(table) = &args[0] else {
                return err(span, "`input` takes a table name in quotes");
            };
            let Some((spec, _)) = specs.get(name) else {
                return err(span, format!("`{name}` is an input and needs a `::` typespec"));
            };
            match spec {
                BatchType::ZSet(TypeDesc::Record(_)) => {}
                other => {
                    return err(
                        span,
                        format!(
                            "an input must be `OrdZSet(record(...))`, found `{other}`"
                        ),
                    );
                }
            }
            Ok(((*spec).clone(), PlanOp::Input { table: table.clone() }))
        }

        "map" => {
            want(2)?;
            let (input, elem) = cx.zset(stream_arg(0)?)?;
            let (f, out) = check_fun(fun_arg(1)?, &[elem], span)?;
            let out = out.into_known(span, "the body of `map`")?;
            Ok((BatchType::ZSet(out), PlanOp::Map { input, f: Arc::new(f) }))
        }

        "filter" => {
            want(2)?;
            let sname = stream_arg(0)?;
            let (input, elem) = cx.zset(sname)?;
            let (f, out) = check_fun(fun_arg(1)?, std::slice::from_ref(&elem), span)?;
            let out = out.into_known(span, "the body of `filter`")?;
            if out.non_null() != &TypeDesc::Bool {
                return err(span, format!("`filter`'s function must return bool, found `{out}`"));
            }
            Ok((BatchType::ZSet(elem), PlanOp::Filter { input, f: Arc::new(f) }))
        }

        "map_index" => {
            want(2)?;
            let (input, elem) = cx.zset(stream_arg(0)?)?;
            let fun = fun_arg(1)?;
            let ExprKind::Tuple(parts) = &fun.body.kind else {
                return err(span, "`map_index`'s function must return a `(key, value)` pair");
            };
            if parts.len() != 2 {
                return err(span, "`map_index`'s function must return exactly two elements");
            }
            let (key, kt) = check_fun_body(fun, &parts[0], std::slice::from_ref(&elem), span)?;
            let (value, vt) = check_fun_body(fun, &parts[1], &[elem], span)?;
            let kt = kt.into_known(span, "a `map_index` key")?;
            let vt = vt.into_known(span, "a `map_index` value")?;
            Ok((
                BatchType::IndexedZSet(kt, vt),
                PlanOp::MapIndex { input, key: Arc::new(key), value: Arc::new(value) },
            ))
        }

        "join" => {
            want(3)?;
            let (left, k1, v1) = cx.indexed(stream_arg(0)?)?;
            let (right, k2, v2) = cx.indexed(stream_arg(1)?)?;
            if k1 != k2 {
                return err(
                    span,
                    format!("`join` needs equal key types, found `{k1}` and `{k2}`"),
                );
            }
            let (f, out) = check_fun(fun_arg(2)?, &[k1, v1, v2], span)?;
            let out = out.into_known(span, "the body of `join`")?;
            Ok((BatchType::ZSet(out), PlanOp::Join { left, right, f: Arc::new(f) }))
        }

        "antijoin" => {
            want(2)?;
            let (left, k1, v1) = cx.indexed(stream_arg(0)?)?;
            let (right, k2, _) = cx.indexed(stream_arg(1)?)?;
            if k1 != k2 {
                return err(
                    span,
                    format!("`antijoin` needs equal key types, found `{k1}` and `{k2}`"),
                );
            }
            Ok((BatchType::IndexedZSet(k1, v1), PlanOp::Antijoin { left, right }))
        }

        "distinct" => {
            want(1)?;
            let i = cx.stream(stream_arg(0)?)?;
            Ok((plan.nodes[i].ty.clone(), PlanOp::Distinct { input: i }))
        }

        "aggregate" => {
            want(3)?;
            let (input, k, v) = cx.indexed(stream_arg(0)?)?;
            let Arg::Name(agg_name) = &args[1] else {
                return err(span, "`aggregate`'s second argument must be an aggregator name");
            };
            let agg = match agg_name.as_str() {
                "min" => Agg::Min,
                "max" => Agg::Max,
                other => {
                    return err(
                        span,
                        format!(
                            "unknown aggregator `{other}`; this build has `min` and `max` \
                             (use the `weighted_count` operator to count rows)"
                        ),
                    );
                }
            };
            let (f, out) = check_fun(fun_arg(2)?, &[v], span)?;
            let out = out.into_known(span, "the body of `aggregate`")?;
            Ok((
                BatchType::IndexedZSet(k, out),
                PlanOp::Aggregate { input, agg, f: Arc::new(f) },
            ))
        }

        "weighted_count" => {
            want(1)?;
            let (input, elem) = cx.zset(stream_arg(0)?)?;
            Ok((
                BatchType::IndexedZSet(elem, TypeDesc::I64),
                PlanOp::WeightedCount { input },
            ))
        }

        "neg" => {
            want(1)?;
            let i = cx.stream(stream_arg(0)?)?;
            Ok((plan.nodes[i].ty.clone(), PlanOp::Neg { input: i }))
        }

        "plus" | "minus" => {
            want(2)?;
            let l = cx.stream(stream_arg(0)?)?;
            let r = cx.stream(stream_arg(1)?)?;
            let (lt, rt) = (&plan.nodes[l].ty, &plan.nodes[r].ty);
            if lt != rt {
                return err(
                    span,
                    format!("`{op}` needs identical batch types, found `{lt}` and `{rt}`"),
                );
            }
            let ty = lt.clone();
            Ok((
                ty,
                if op == "plus" {
                    PlanOp::Plus { left: l, right: r }
                } else {
                    PlanOp::Minus { left: l, right: r }
                },
            ))
        }

        "sum" => {
            if args.len() < 2 {
                return err(span, "`sum` takes at least two streams");
            }
            let mut inputs = Vec::new();
            for i in 0..args.len() {
                inputs.push(cx.stream(stream_arg(i)?)?);
            }
            let ty = plan.nodes[inputs[0]].ty.clone();
            for &i in &inputs[1..] {
                if plan.nodes[i].ty != ty {
                    return err(
                        span,
                        format!(
                            "`sum` needs identical batch types, found `{ty}` and `{}`",
                            plan.nodes[i].ty
                        ),
                    );
                }
            }
            Ok((ty, PlanOp::Sum { inputs }))
        }

        other => err(
            span,
            format!("unknown operator `{other}`"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

fn check_fun(fun: &FunLit, params: &[TypeDesc], span: Span) -> TResult<(TypedExpr, Ty)> {
    check_fun_body(fun, &fun.body, params, span)
}

fn check_fun_body(
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
        ExprKind::Null => (TypedExpr::Const(DynValue::Null), Ty::Null),
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
            let fty = if bt.is_nullable() { nullable(fty) } else { fty };
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
                "a tuple is only allowed as the body of a `map_index` function",
            );
        }

        ExprKind::Unary(op, inner) => {
            let (ie, it) = infer(inner, env)?;
            let ty = match it {
                Ty::Null => Ty::Null,
                Ty::Known(t) => {
                    let ok = match op {
                        UnOp::Neg => is_numeric(&t),
                        UnOp::Not => t.non_null() == &TypeDesc::Bool,
                    };
                    if !ok {
                        return err(span, format!("cannot apply this operator to `{t}`"));
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

        ExprKind::If(c, t, f) => {
            let (ce, ct) = infer(c, env)?;
            if let Ty::Known(ct) = &ct
                && ct.non_null() != &TypeDesc::Bool {
                    return err(span, format!("`if` condition must be bool, found `{ct}`"));
                }
            let (te, tt) = infer(t, env)?;
            let (fe, ft) = infer(f, env)?;
            let ty = unify(tt, ft, span)?;
            (TypedExpr::If(Box::new(ce), Box::new(te), Box::new(fe)), ty)
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
    let nullable_result = matches!(lt, Ty::Null) || matches!(rt, Ty::Null) || {
        matches!((&lt, &rt), (Ty::Known(a), Ty::Known(b)) if a.is_nullable() || b.is_nullable())
    };

    match op {
        And | Or => {
            for t in [&lt, &rt] {
                if let Ty::Known(t) = t
                    && t.non_null() != &TypeDesc::Bool {
                        return err(span, format!("`and`/`or` need bool operands, found `{t}`"));
                    }
            }
            Ok(Ty::Known(maybe_null(TypeDesc::Bool, nullable_result)))
        }
        Eq | Ne | Lt | Le | Gt | Ge => {
            if let (Ty::Known(a), Ty::Known(b)) = (&lt, &rt) {
                let comparable = a.non_null() == b.non_null()
                    || (is_numeric(a) && is_numeric(b))
                    || (is_stringy(a) && is_stringy(b));
                if !comparable {
                    return err(span, format!("cannot compare `{a}` with `{b}`"));
                }
            }
            Ok(Ty::Known(maybe_null(TypeDesc::Bool, nullable_result)))
        }
        Add | Sub | Mul | Div | Rem => {
            match (&lt, &rt) {
                (Ty::Known(a), Ty::Known(b)) => {
                    // `+` doubles as string concatenation.
                    if op == Add && is_stringy(a) && is_stringy(b) {
                        return Ok(Ty::Known(maybe_null(TypeDesc::SqlString, nullable_result)));
                    }
                    if !is_numeric(a) || !is_numeric(b) {
                        return err(
                            span,
                            format!("cannot apply this arithmetic operator to `{a}` and `{b}`"),
                        );
                    }
                    let base = if a.non_null() == &TypeDesc::F64 || b.non_null() == &TypeDesc::F64 {
                        TypeDesc::F64
                    } else {
                        TypeDesc::I64
                    };
                    Ok(Ty::Known(maybe_null(base, nullable_result)))
                }
                // Arithmetic on an untyped `null` has no inferable type.
                _ => Ok(Ty::Null),
            }
        }
    }
}

fn maybe_null(t: TypeDesc, null: bool) -> TypeDesc {
    if null { nullable(t) } else { t }
}

fn infer_builtin(b: Builtin, name: &str, args: &[Ty], span: Span) -> TResult<Ty> {
    let known = |i: usize| -> Option<&TypeDesc> {
        match &args[i] {
            Ty::Known(t) => Some(t),
            Ty::Null => None,
        }
    };
    Ok(match b {
        Builtin::IsNull | Builtin::IsNotNull => Ty::Known(TypeDesc::Bool),
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
                (Ty::Null, other) | (other, Ty::Null) => other,
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
            None => Ty::Null,
        },
    })
}
