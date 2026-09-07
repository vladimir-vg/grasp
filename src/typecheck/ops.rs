//! Checking one operator call, and resolving its arguments to nodes.

use super::infer::{check_fun, is_numeric, optional};
use super::{Env, Functions, TResult, err, lookup, push_node};
use super::plan::{Agg, KeyValue, Plan, PlanOp};
use crate::diag::Span;
use crate::lang::{Arg, Expr, FunLit, OpCall};
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

/// One operator call, with its arguments already resolved to nodes.
///
/// The accessors are what let the per-family checkers read uniformly: each
/// states what it needs of an argument and gets either that or a diagnostic
/// naming the operator and the position.
struct Ctx<'a> {
    op: &'a str,
    span: Span,
    args: &'a [Arg],
    rargs: Vec<RArg<'a>>,
    funcs: &'a Functions<'a>,
}

/// The function an operator was given: written inline, or named elsewhere.
///
/// Both reach the checker as parameter names plus one body expression, because
/// that is all a template is. Passing a named function directly — `map(a,
/// scale)` — is what makes one helper usable at several call sites.
struct FnArg<'a> {
    params: &'a [String],
    body: &'a Expr,
}

impl<'a> Ctx<'a> {
    fn want(&self, n: usize) -> TResult<()> {
        if self.args.len() == n {
            Ok(())
        } else {
            let (op, found) = (self.op, self.args.len());
            err(self.span, format!("`{op}` takes {n} argument(s), found {found}"))
        }
    }

    fn stream_arg(&self, i: usize) -> TResult<usize> {
        let (op, span) = (self.op, self.span);
        match &self.rargs[i] {
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
    }

    fn fun_arg(&self, i: usize) -> TResult<FnArg<'a>> {
        let (op, span) = (self.op, self.span);
        match &self.rargs[i] {
            RArg::Fun(f) => Ok(FnArg { params: &f.params, body: &f.body }),
            // A bare name that is not a stream may still be a function.
            RArg::Name(n) => match self.funcs.get(n) {
                Some(def) => Ok(FnArg { params: &def.params, body: &def.body }),
                None => err(span, format!("unknown stream or function `{n}`")),
            },
            _ => err(
                span,
                format!(
                    "argument {} of `{op}` must be a `function(...)` or the name of one",
                    i + 1
                ),
            ),
        }
    }

    /// Already resolved; this exists so the operator checkers read uniformly.
    fn stream(&self, i: usize) -> TResult<usize> {
        Ok(i)
    }

    /// The parameters an operator's function is fed for one element of `i`.
    ///
    /// A flat stream feeds the row; an indexed one feeds `(key, value)`,
    /// matching `dbsp`'s `ItemRef` for each shape. Every row-at-a-time operator
    /// follows this rule, so none of them cares which shape it was given.
    fn element(&self, plan: &Plan, i: usize) -> Vec<TypeDesc> {
        match &plan.nodes[i].ty {
            BatchType::ZSet(t) => vec![t.clone()],
            BatchType::IndexedZSet(k, v) => vec![k.clone(), v.clone()],
        }
    }

    fn zset(&self, plan: &Plan, i: usize) -> TResult<(usize, TypeDesc)> {
        let node = &plan.nodes[i];
        match &node.ty {
            BatchType::ZSet(t) => Ok((i, t.clone())),
            other => err(
                self.span,
                format!("`{}` is `{other}`, but a flat zset is required here", node.name),
            ),
        }
    }

    fn indexed(&self, plan: &Plan, i: usize) -> TResult<(usize, TypeDesc, TypeDesc)> {
        let node = &plan.nodes[i];
        match &node.ty {
            BatchType::IndexedZSet(k, v) => Ok((i, k.clone(), v.clone())),
            other => err(
                self.span,
                format!("`{}` is `{other}`, but an indexed_zset is required here", node.name),
            ),
        }
    }
}

/// The `record(key: ..., value: ...)` the three indexing operators return.
///
/// `key` and `value` are the one place in the language where a field *name*
/// carries meaning; everywhere else field names are their own unrestricted
/// namespace. They are matched by name, so the order they are written in does
/// not matter, and the two indices travel to the lowering in a [`KeyValue`].
fn key_value(ty: &TypeDesc, op: &str, span: Span) -> TResult<(KeyValue, TypeDesc, TypeDesc)> {
    let wanted = format!(
        "`{op}`'s function must return `record(key: ..., value: ...)`, found `{ty}`"
    );
    let TypeDesc::Record(fields) = ty else {
        return err(span, wanted);
    };
    let (Some(key), Some(value)) = (ty.field_index("key"), ty.field_index("value")) else {
        return err(span, wanted);
    };
    if fields.len() != 2 {
        let extra: Vec<&str> = fields
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| *n != "key" && *n != "value")
            .collect();
        return err(
            span,
            format!(
                "`{op}`'s record has no room for {}: it takes exactly `key` and `value`",
                extra.join("`, `")
            ),
        );
    }
    Ok((KeyValue { key, value }, fields[key].1.clone(), fields[value].1.clone()))
}

/// Checks one operator call.
///
/// The work is split by family, and each family answers `None` for a name it
/// does not handle. So every operator name appears in exactly one place, and a
/// name no family claims falls through to the diagnostic at the bottom — which
/// is what `tests/reserved.rs` pins against the `OPERATORS` list.
pub(super) fn check_op(
    name: &str,
    call: &OpCall,
    span: Span,
    plan: &mut Plan,
    env: Env<'_>,
) -> TResult<(BatchType, PlanOp)> {
    let op = call.op.as_str();

    // Resolve nested calls first, so the mutable borrow of `plan` ends before
    // anything below reads it.
    let mut rargs = Vec::with_capacity(call.args.len());
    for a in &call.args {
        rargs.push(resolve_arg(a, plan, env)?);
    }
    let cx = Ctx { op, span, args: &call.args, rargs, funcs: env.funcs };

    if let Some(r) = check_source(name, &cx, env)? {
        return Ok(r);
    }
    if let Some(r) = check_map_family(&cx, plan)? {
        return Ok(r);
    }
    if let Some(r) = check_join_family(&cx, plan)? {
        return Ok(r);
    }
    if let Some(r) = check_aggregate(&cx, plan)? {
        return Ok(r);
    }
    if let Some(r) = check_algebraic(&cx, plan)? {
        return Ok(r);
    }
    err(span, format!("unknown operator `{op}`"))
}

/// The operators whose type comes from a `::` typespec rather than from an
/// operand, and so are the only ones needing the node's own name.
fn check_source(name: &str, cx: &Ctx<'_>, env: Env<'_>) -> TResult<Option<(BatchType, PlanOp)>> {
    let (op, span) = (cx.op, cx.span);
    let out = match op {
        "input" => {
            cx.want(1)?;
            let Arg::Str(table) = &cx.args[0] else {
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
            cx.want(0)?;
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

        _ => return Ok(None),
    };
    out.map(Some)
}

/// Row-at-a-time operators.
///
/// Every one of these takes either stream shape, feeding the function one row
/// for a flat stream and `(key, value)` for an indexed one — see
/// [`Ctx::element`]. The *result* shape follows from what the function returns,
/// not from what it was given, so `map` over an indexed stream flattens it.
/// That is the only route out of an indexed shape other than a join, and it is
/// what makes an outer join expressible: `antijoin` yields an indexed stream
/// whose rows would otherwise be stuck there.
fn check_map_family(cx: &Ctx<'_>, plan: &Plan) -> TResult<Option<(BatchType, PlanOp)>> {
    let (op, span) = (cx.op, cx.span);
    let out = match op {

        "map" => {
            cx.want(2)?;
            let input = cx.stream(cx.stream_arg(0)?)?;
            let params = cx.element(plan, input);
            let (f, out) = {
                let f = cx.fun_arg(1)?;
                check_fun(f.params, f.body, &params, span, "the body of `map`", cx.funcs)?
            };
            // Flattening: an indexed stream mapped row-at-a-time produces a
            // plain zset, which is the only way out of an indexed shape other
            // than a join.
            Ok((BatchType::ZSet(out), PlanOp::Map { input, f: Arc::new(f) }))
        }

        "filter" => {
            cx.want(2)?;
            let input = cx.stream(cx.stream_arg(0)?)?;
            let params = cx.element(plan, input);
            let (f, out) = {
                let f = cx.fun_arg(1)?;
                check_fun(f.params, f.body, &params, span, "the body of `filter`", cx.funcs)?
            };
            if out.non_null() != &TypeDesc::Bool {
                return err(span, format!("`filter`'s function must return bool, found `{out}`"));
            }
            Ok((plan.nodes[input].ty.clone(), PlanOp::Filter { input, f: Arc::new(f) }))
        }

        "flat_map" => {
            cx.want(2)?;
            let input = cx.stream(cx.stream_arg(0)?)?;
            let params = cx.element(plan, input);
            let (f, out) = {
                let f = cx.fun_arg(1)?;
                check_fun(f.params, f.body, &params, span, "the body of `flat_map`", cx.funcs)?
            };
            // One row per element, so the fan-out follows the data.
            let TypeDesc::Array(row) = out else {
                return err(
                    span,
                    format!("`flat_map`'s function must return an array of rows, found `{out}`"),
                );
            };
            Ok((BatchType::ZSet(*row), PlanOp::FlatMap { input, f: Arc::new(f) }))
        }

        "flat_map_index" => {
            cx.want(2)?;
            let input = cx.stream(cx.stream_arg(0)?)?;
            let params = cx.element(plan, input);
            let (f, out) = {
                let f = cx.fun_arg(1)?;
                check_fun(
                    f.params, f.body, &params, span, "the body of `flat_map_index`", cx.funcs,
                )?
            };
            let TypeDesc::Array(row) = out else {
                return err(
                    span,
                    format!(
                        "`flat_map_index`'s function must return an array of \
                         `record(key: ..., value: ...)`, found `{out}`"
                    ),
                );
            };
            let (kv, kt, vt) = key_value(&row, op, span)?;
            Ok((BatchType::IndexedZSet(kt, vt), PlanOp::FlatMapIndex { input, f: Arc::new(f), kv }))
        }

        "map_index" => {
            cx.want(2)?;
            let input = cx.stream(cx.stream_arg(0)?)?;
            let params = cx.element(plan, input);
            let (f, out) = {
                let f = cx.fun_arg(1)?;
                check_fun(f.params, f.body, &params, span, "the body of `map_index`", cx.funcs)?
            };
            let (kv, kt, vt) = key_value(&out, op, span)?;
            Ok((
                BatchType::IndexedZSet(kt, vt),
                PlanOp::MapIndex { input, f: Arc::new(f), kv },
            ))
        }

        _ => return Ok(None),
    };
    out.map(Some)
}

/// Operators over two indexed streams, which must agree on their key type.
fn check_join_family(cx: &Ctx<'_>, plan: &Plan) -> TResult<Option<(BatchType, PlanOp)>> {
    let (op, span) = (cx.op, cx.span);
    let out = match op {

        "join" => {
            cx.want(3)?;
            let (left, k1, v1) = cx.indexed(plan, cx.stream_arg(0)?)?;
            let (right, k2, v2) = cx.indexed(plan, cx.stream_arg(1)?)?;
            if k1 != k2 {
                return err(
                    span,
                    format!("`join` needs equal key types, found `{k1}` and `{k2}`"),
                );
            }
            let (f, out) =
                {
                let f = cx.fun_arg(2)?;
                check_fun(f.params, f.body, &[k1, v1, v2], span, "the body of `join`", cx.funcs)?
            };
            Ok((BatchType::ZSet(out), PlanOp::Join { left, right, f: Arc::new(f) }))
        }

        "join_index" => {
            cx.want(3)?;
            let (left, k1, v1) = cx.indexed(plan, cx.stream_arg(0)?)?;
            let (right, k2, v2) = cx.indexed(plan, cx.stream_arg(1)?)?;
            if k1 != k2 {
                return err(
                    span,
                    format!("`join_index` needs equal key types, found `{k1}` and `{k2}`"),
                );
            }
            let (f, out) =
                {
                let f = cx.fun_arg(2)?;
                check_fun(f.params, f.body, &[k1, v1, v2], span, "the body of `join_index`", cx.funcs)?
            };
            let (kv, kt, vt) = key_value(&out, op, span)?;
            Ok((
                BatchType::IndexedZSet(kt, vt),
                PlanOp::JoinIndex { left, right, f: Arc::new(f), kv },
            ))
        }

        "antijoin" => {
            cx.want(2)?;
            let (left, k1, v1) = cx.indexed(plan, cx.stream_arg(0)?)?;
            let (right, k2, _) = cx.indexed(plan, cx.stream_arg(1)?)?;
            if k1 != k2 {
                return err(
                    span,
                    format!("`antijoin` needs equal key types, found `{k1}` and `{k2}`"),
                );
            }
            Ok((BatchType::IndexedZSet(k1, v1), PlanOp::Antijoin { left, right }))
        }

        _ => return Ok(None),
    };
    out.map(Some)
}

/// Operators that collapse each group of an indexed stream to a single value.
fn check_aggregate(cx: &Ctx<'_>, plan: &Plan) -> TResult<Option<(BatchType, PlanOp)>> {
    let (op, span) = (cx.op, cx.span);
    let out = match op {

        "aggregate" => {
            cx.want(3)?;
            let (input, k, v) = cx.indexed(plan, cx.stream_arg(0)?)?;
            let Arg::Name(agg_name) = &cx.args[1] else {
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
            let (f, out) = {
                let f = cx.fun_arg(2)?;
                check_fun(f.params, f.body, &[v], span, "the body of `aggregate`", cx.funcs)?
            };
            let optional_in = out.is_optional();

            // `min`/`max` return the projected value; the linear aggregators
            // impose their own result types.
            let result = match agg {
                // Same reason `<` is rejected: the ordering is tag-first, so a
                // `min` over documents returns the smallest by tag, which is a
                // result nobody asked for.
                Agg::Min | Agg::Max if out.non_null() == &TypeDesc::Json => {
                    return err(
                        span,
                        format!(
                            "`{agg_name}` has no meaning over documents: they sort by type \
                             tag. Project a value out with `cast` and aggregate that."
                        ),
                    );
                }
                Agg::Min | Agg::Max => out.clone(),
                Agg::Count => TypeDesc::I64,
                Agg::Sum | Agg::Avg => {
                    if !is_numeric(&out) {
                        return err(
                            span,
                            format!("`{agg_name}` needs a numeric projection, found `{out}`"),
                        );
                    }
                    // Floats are allowed, and lower to a fold rather than the
                    // linear path — fp addition is not associative, so an
                    // incrementally maintained sum would depend on the order
                    // changes arrived in. That rules out the linear path, not
                    // the operation, which is the split Feldera makes too.
                    // A mean of no contributing rows is undefined, so `avg` is
                    // always optional and always yields f64 rather than
                    // truncating. `sum` needs the same escape only when the
                    // projection itself can be `NONE`: otherwise the weighted
                    // sum is the answer even for a group whose weights cancel.
                    if agg == Agg::Avg {
                        optional(TypeDesc::F64)
                    } else if optional_in {
                        optional(out.non_null().clone())
                    } else {
                        out.non_null().clone()
                    }
                }
            };
            Ok((
                BatchType::IndexedZSet(k, result),
                PlanOp::Aggregate { input, agg, f: Arc::new(f), projection: out },
            ))
        }

        "weighted_count" => {
            cx.want(1)?;
            let (input, elem) = cx.zset(plan, cx.stream_arg(0)?)?;
            Ok((
                BatchType::IndexedZSet(elem, TypeDesc::I64),
                PlanOp::WeightedCount { input },
            ))
        }

        _ => return Ok(None),
    };
    out.map(Some)
}

/// Stream algebra: operators over whole batches, whose result type is the
/// operand type. `plan` is mutable here because `plus`, `minus` and `sum` can
/// push a node — an untyped `empty()` operand is materialized against the type
/// of its siblings.
fn check_algebraic(cx: &Ctx<'_>, plan: &mut Plan) -> TResult<Option<(BatchType, PlanOp)>> {
    let (op, span) = (cx.op, cx.span);
    let out = match op {

        "integrate" | "differentiate" | "delay" => {
            cx.want(1)?;
            let input = cx.stream(cx.stream_arg(0)?)?;
            let ty = plan.nodes[input].ty.clone();
            let plan_op = match op {
                "integrate" => PlanOp::Integrate { input },
                "differentiate" => PlanOp::Differentiate { input },
                _ => PlanOp::Delay { input },
            };
            Ok((ty, plan_op))
        }

        "distinct" => {
            cx.want(1)?;
            let i = cx.stream(cx.stream_arg(0)?)?;
            Ok((plan.nodes[i].ty.clone(), PlanOp::Distinct { input: i }))
        }

        "neg" => {
            cx.want(1)?;
            let i = cx.stream(cx.stream_arg(0)?)?;
            Ok((plan.nodes[i].ty.clone(), PlanOp::Neg { input: i }))
        }

        "plus" | "minus" => {
            cx.want(2)?;
            let (l, r) = resolve_pair(&cx.rargs, plan, span, op)?;
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
            if cx.args.len() < 2 {
                return err(span, "`sum` takes at least two streams");
            }
            // At least one operand must be typed, since `sum` requires identical
            // batch types and an `empty()` takes its type from the others.
            let Some(typed) = cx.rargs.iter().find_map(|a| match a {
                RArg::Stream(i) => Some(plan.nodes[*i].ty.clone()),
                _ => None,
            }) else {
                return err(span, "`sum` needs at least one operand with a known type");
            };
            let mut inputs = Vec::new();
            for a in &cx.rargs {
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

        _ => return Ok(None),
    };
    out.map(Some)
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