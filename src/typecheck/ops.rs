//! Checking one operator call, and resolving its arguments to nodes.

use super::infer::{check_fun, check_fun_body, is_numeric, optional};
use super::{Env, TResult, err, lookup, push_node};
use super::plan::{Agg, Plan, PlanOp};
use crate::diag::Span;
use crate::lang::{Arg, ExprKind, FunLit, OpCall};
use crate::value::{BatchType, TypeDesc};
use std::sync::Arc;

/// An argument after nested calls have been resolved into nodes.
pub(super) enum RArg<'a> {
    /// A node index — either a declared name or a nested call already pushed.
    Stream(usize),
    /// A bare name that is not a declared node: an aggregator, or a mistake.
    Name(&'a str),
    /// `empty()`, whose type comes from where it sits rather than from itself.
    /// Left unresolved here so the operator can supply one.
    Empty(Span),
    /// Only the shape matters: the `input` arm reads the table name from the
    /// unresolved argument.
    Str,
    Fun(&'a FunLit),
}


/// Resolves one argument, checking and pushing a node for a nested call.
///
/// Recursion is depth-first and pushes before returning, so a nested node
/// always lands at a lower index than the node using it — which is what
/// `lower.rs` needs, since it builds `plan.nodes` in order.
pub(super) fn resolve_arg<'a>(
    arg: &'a Arg,
    plan: &mut Plan,
    env: Env<'_>,
) -> TResult<RArg<'a>> {
    Ok(match arg {
        Arg::Str(_) => RArg::Str,
        Arg::Fun(f) => RArg::Fun(f),
        Arg::Field(r) => RArg::Stream(lookup(r, plan, env.scope, env.prefix)?),
        Arg::Name(n) => match env.scope.get(n.as_str()).or_else(|| plan.by_name.get(n.as_str())) {
            Some(i) => RArg::Stream(*i),
            // Not a node: an aggregator name, or an error the caller reports
            // with the context to say what was expected.
            None => RArg::Name(n),
        },
        Arg::Op(call) if call.op == "empty" => {
            if !call.args.is_empty() {
                return err(call.span, "`empty()` takes no arguments; its type comes from where it is used");
            }
            RArg::Empty(call.span)
        }
        Arg::Op(call) => {
            if call.op == "input" {
                return err(
                    call.span,
                    "`input` cannot be nested: its schema comes from a `::` typespec, \
                     which needs a name to attach to. Bind it to one first.",
                );
            }
            // Anonymous, so it is named for diagnostics only and deliberately
            // kept out of `by_name`: it cannot be an output and cannot collide.
            let name = format!("{}@{}", call.op, call.span);
            let (ty, op) = check_op(&name, call, call.span, plan, env)?;
            RArg::Stream(push_node(plan, name, ty, op, call.span))
        }
    })
}

struct Ctx<'a> {
    plan: &'a Plan,
    span: Span,
}

impl Ctx<'_> {
    /// Already resolved; this exists so the operator arms read uniformly.
    fn stream(&self, i: usize) -> TResult<usize> {
        Ok(i)
    }

    fn zset(&self, i: usize) -> TResult<(usize, TypeDesc)> {
        let node = &self.plan.nodes[i];
        match &node.ty {
            BatchType::ZSet(t) => Ok((i, t.clone())),
            other => err(
                self.span,
                format!("`{}` is `{other}`, but a flat zset is required here", node.name),
            ),
        }
    }

    fn indexed(&self, i: usize) -> TResult<(usize, TypeDesc, TypeDesc)> {
        let node = &self.plan.nodes[i];
        match &node.ty {
            BatchType::IndexedZSet(k, v) => Ok((i, k.clone(), v.clone())),
            other => err(
                self.span,
                format!("`{}` is `{other}`, but an indexed_zset is required here", node.name),
            ),
        }
    }
}

pub(super) fn check_op(
    name: &str,
    call: &OpCall,
    span: Span,
    plan: &mut Plan,
    env: Env<'_>,
) -> TResult<(BatchType, PlanOp)> {
    let op = call.op.as_str();
    let args = &call.args;

    // Resolve nested calls first, so the mutable borrow of `plan` ends before
    // anything below reads it.
    let mut rargs = Vec::with_capacity(args.len());
    for a in args {
        rargs.push(resolve_arg(a, plan, env)?);
    }

    let cx = Ctx { plan, span };

    let want = |n: usize| -> TResult<()> {
        if args.len() == n {
            Ok(())
        } else {
            err(span, format!("`{op}` takes {n} argument(s), found {}", args.len()))
        }
    };
    let stream_arg = |i: usize| -> TResult<usize> {
        match &rargs[i] {
            RArg::Stream(idx) => Ok(*idx),
            RArg::Name(n) => err(span, format!("unknown stream `{n}`")),
            RArg::Empty(espan) => err(
                *espan,
                format!(
                    "`empty()` has no type here: `{op}` does not determine one. It is \
                     allowed as a circuit argument, or beside a typed operand of \
                     `plus`, `minus` or `sum`."
                ),
            ),
            _ => err(span, format!("argument {} of `{op}` must name a stream", i + 1)),
        }
    };
    let fun_arg = |i: usize| -> TResult<&FunLit> {
        match &rargs[i] {
            RArg::Fun(f) => Ok(f),
            _ => err(span, format!("argument {} of `{op}` must be a `fun(...)`", i + 1)),
        }
    };

    match op {
        "input" => {
            want(1)?;
            let Arg::Str(table) = &args[0] else {
                return err(span, "`input` takes a table name in quotes");
            };
            let Some((spec, _)) = env.specs.get(name) else {
                return err(span, format!("`{name}` is an input and needs a `::` typespec"));
            };
            match spec {
                BatchType::ZSet(TypeDesc::Record(_)) => {}
                other => {
                    return err(
                        span,
                        format!(
                            "an input must be `zset(record(...))`, found `{other}`"
                        ),
                    );
                }
            }
            Ok(((*spec).clone(), PlanOp::Input { table: table.clone() }))
        }

        // Reached only for a standalone `x := empty()`; as an argument it is
        // handled during resolution, where the surrounding operator supplies a
        // type. Here the typespec is the only thing that can.
        "empty" => {
            want(0)?;
            let Some((ty, _)) = env.specs.get(name) else {
                return err(
                    span,
                    format!(
                        "`empty()` has no type here: add `{name} :: ...`, or use it \
                         where an operator determines one"
                    ),
                );
            };
            Ok(((*ty).clone(), PlanOp::Empty))
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
            let input = cx.stream(stream_arg(0)?)?;
            // A flat stream feeds the row; an indexed one feeds (key, value),
            // matching `dbsp`'s `ItemRef` for each shape.
            let params: Vec<TypeDesc> = match &plan.nodes[input].ty {
                BatchType::ZSet(t) => vec![t.clone()],
                BatchType::IndexedZSet(k, v) => vec![k.clone(), v.clone()],
            };
            let (f, out) = check_fun(fun_arg(1)?, &params, span)?;
            let out = out.into_known(span, "the body of `filter`")?;
            if out.non_null() != &TypeDesc::Bool {
                return err(span, format!("`filter`'s function must return bool, found `{out}`"));
            }
            Ok((plan.nodes[input].ty.clone(), PlanOp::Filter { input, f: Arc::new(f) }))
        }

        "flat_map" => {
            want(2)?;
            let (input, elem) = cx.zset(stream_arg(0)?)?;
            let fun = fun_arg(1)?;
            let ExprKind::List(items) = &fun.body.kind else {
                return err(span, "`flat_map`'s function must return a list of rows");
            };
            if items.is_empty() {
                return err(span, "`flat_map`'s list must have at least one element, \
                                  or the output type cannot be inferred");
            }
            let mut outputs = Vec::with_capacity(items.len());
            let mut out_ty: Option<TypeDesc> = None;
            for item in items {
                let (e, t) = check_fun_body(fun, item, std::slice::from_ref(&elem), span)?;
                let t = t.into_known(span, "an element of a `flat_map` list")?;
                match &out_ty {
                    None => out_ty = Some(t),
                    Some(prev) if *prev == t => {}
                    Some(prev) => {
                        return err(
                            span,
                            format!("`flat_map`'s rows must share one type, found `{prev}` and `{t}`"),
                        );
                    }
                }
                outputs.push(Arc::new(e));
            }
            Ok((
                BatchType::ZSet(out_ty.expect("non-empty")),
                PlanOp::FlatMap { input, outputs },
            ))
        }

        "flat_map_index" => {
            want(2)?;
            let (input, elem) = cx.zset(stream_arg(0)?)?;
            let fun = fun_arg(1)?;
            let ExprKind::List(items) = &fun.body.kind else {
                return err(span, "`flat_map_index`'s function must return a list of pairs");
            };
            if items.is_empty() {
                return err(span, "`flat_map_index`'s list must have at least one element, \
                                  or the output type cannot be inferred");
            }
            let mut pairs = Vec::with_capacity(items.len());
            let mut kv: Option<(TypeDesc, TypeDesc)> = None;
            for item in items {
                let ExprKind::Tuple(parts) = &item.kind else {
                    return err(item.span, "every element must be a `(key, value)` pair");
                };
                if parts.len() != 2 {
                    return err(item.span, "a pair has exactly two elements");
                }
                let (k, kt) = check_fun_body(fun, &parts[0], std::slice::from_ref(&elem), span)?;
                let (v, vt) = check_fun_body(fun, &parts[1], std::slice::from_ref(&elem), span)?;
                let kt = kt.into_known(item.span, "a `flat_map_index` key")?;
                let vt = vt.into_known(item.span, "a `flat_map_index` value")?;
                match &kv {
                    None => kv = Some((kt, vt)),
                    Some((pk, pv)) if *pk == kt && *pv == vt => {}
                    Some((pk, pv)) => {
                        return err(
                            item.span,
                            format!("every pair must have the same types, found \
                                     `({pk}, {pv})` and `({kt}, {vt})`"),
                        );
                    }
                }
                pairs.push((Arc::new(k), Arc::new(v)));
            }
            let (kt, vt) = kv.expect("non-empty");
            Ok((BatchType::IndexedZSet(kt, vt), PlanOp::FlatMapIndex { input, pairs }))
        }

        "integrate" | "differentiate" | "delay" => {
            want(1)?;
            let input = cx.stream(stream_arg(0)?)?;
            let ty = plan.nodes[input].ty.clone();
            let plan_op = match op {
                "integrate" => PlanOp::Integrate { input },
                "differentiate" => PlanOp::Differentiate { input },
                _ => PlanOp::Delay { input },
            };
            Ok((ty, plan_op))
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

        "join_index" => {
            want(3)?;
            let (left, k1, v1) = cx.indexed(stream_arg(0)?)?;
            let (right, k2, v2) = cx.indexed(stream_arg(1)?)?;
            if k1 != k2 {
                return err(
                    span,
                    format!("`join_index` needs equal key types, found `{k1}` and `{k2}`"),
                );
            }
            let fun = fun_arg(2)?;
            let ExprKind::Tuple(parts) = &fun.body.kind else {
                return err(span, "`join_index`'s function must return a `(key, value)` pair");
            };
            if parts.len() != 2 {
                return err(span, "`join_index`'s function must return exactly two elements");
            }
            let params = [k1, v1, v2];
            let (key, kt) = check_fun_body(fun, &parts[0], &params, span)?;
            let (value, vt) = check_fun_body(fun, &parts[1], &params, span)?;
            let kt = kt.into_known(span, "a `join_index` key")?;
            let vt = vt.into_known(span, "a `join_index` value")?;
            Ok((
                BatchType::IndexedZSet(kt, vt),
                PlanOp::JoinIndex { left, right, key: Arc::new(key), value: Arc::new(value) },
            ))
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
                "sum" => Agg::Sum,
                "avg" => Agg::Avg,
                "count" => Agg::Count,
                other => {
                    return err(
                        span,
                        format!(
                            "unknown aggregator `{other}`; expected \
                             min, max, sum, avg or count"
                        ),
                    );
                }
            };
            let (f, out) = check_fun(fun_arg(2)?, &[v], span)?;
            let out = out.into_known(span, "the body of `aggregate`")?;
            let optional_in = out.is_optional();

            // `min`/`max` return the projected value; the linear aggregators
            // impose their own result types.
            let result = match agg {
                Agg::Min | Agg::Max => out.clone(),
                Agg::Count => TypeDesc::I64,
                Agg::Sum | Agg::Avg => {
                    if !is_numeric(&out) {
                        return err(
                            span,
                            format!("`{agg_name}` needs a numeric projection, found `{out}`"),
                        );
                    }
                    // Floating point is excluded from the linear aggregation
                    // path: fp addition is not associative, so an incrementally
                    // maintained sum would depend on the order additions and
                    // retractions arrive in. `min`/`max` over `f64` are fine —
                    // they are the non-linear path.
                    if out.non_null() == &TypeDesc::F64 {
                        return err(
                            span,
                            format!(
                                "`{agg_name}` cannot be applied to `f64`: floating-point \
                                 addition is not associative, so an incrementally maintained \
                                 sum would depend on the order rows arrive in"
                            ),
                        );
                    }
                    // `avg` always yields f64 rather than truncating.
                    let base = if agg == Agg::Avg { TypeDesc::F64 } else { out.non_null().clone() };
                    // A projection that can be null can leave a group with no
                    // contributing rows, and then there is no sum to report.
                    if optional_in { optional(base) } else { base }
                }
            };
            Ok((
                BatchType::IndexedZSet(k, result),
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
            let (l, r) = resolve_pair(&rargs, plan, span, op)?;
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
            // At least one operand must be typed, since `sum` requires identical
            // batch types and an `empty()` takes its type from the others.
            let Some(typed) = rargs.iter().find_map(|a| match a {
                RArg::Stream(i) => Some(plan.nodes[*i].ty.clone()),
                _ => None,
            }) else {
                return err(span, "`sum` needs at least one operand with a known type");
            };
            let mut inputs = Vec::new();
            for a in &rargs {
                inputs.push(materialize(a, &typed, plan, span, op)?);
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

/// Turns one argument into a node index, giving an `empty()` the type `want`.
pub(super) fn materialize(
    a: &RArg<'_>,
    want: &BatchType,
    plan: &mut Plan,
    span: Span,
    op: &str,
) -> TResult<usize> {
    match a {
        RArg::Stream(i) => Ok(*i),
        RArg::Empty(espan) => {
            Ok(push_node(plan, format!("empty@{espan}"), want.clone(), PlanOp::Empty, *espan))
        }
        RArg::Name(n) => err(span, format!("unknown stream `{n}`")),
        _ => err(span, format!("an argument of `{op}` must name a stream")),
    }
}

/// Resolves two operands where at most one may be an untyped `empty()`.
pub(super) fn resolve_pair(
    rargs: &[RArg<'_>],
    plan: &mut Plan,
    span: Span,
    op: &str,
) -> TResult<(usize, usize)> {
    let typed = rargs.iter().find_map(|a| match a {
        RArg::Stream(i) => Some(plan.nodes[*i].ty.clone()),
        _ => None,
    });
    let Some(ty) = typed else {
        return err(span, format!("`{op}` needs at least one operand with a known type"));
    };
    let l = materialize(&rargs[0], &ty, plan, span, op)?;
    let r = materialize(&rargs[1], &ty, plan, span, op)?;
    Ok((l, r))
}
