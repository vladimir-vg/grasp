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
use crate::lang::{
    Arg, BinOp, CircuitDef, Decl, Expr, ExprKind, FunLit, Instantiation, NodeRef, OpCall, Program,
    Rhs, UnOp,
};
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

/// Every operator name. The reserved-word list is built from this, and a test
/// asserts `check_op` accepts each one, so the two cannot drift apart.
pub const OPERATORS: &[&str] = &[
    "input",
    "map",
    "filter",
    "flat_map",
    "map_index",
    "flat_map_index",
    "join",
    "join_index",
    "antijoin",
    "distinct",
    "aggregate",
    "weighted_count",
    "neg",
    "plus",
    "minus",
    "sum",
    "integrate",
    "differentiate",
    "delay",
    "empty",
];

/// Every aggregator name. These appear as bare names in argument position, so a
/// node named `min` would be silently shadowed if they were not reserved.
pub const AGGREGATORS: &[&str] = &["min", "max", "sum", "avg", "count"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Min,
    Max,
    Sum,
    Avg,
    Count,
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
    Aggregate {
        input: usize,
        agg: Agg,
        f: Arc<TypedExpr>,
    },
    WeightedCount { input: usize },
    Neg { input: usize },
    Plus { left: usize, right: usize },
    Minus { left: usize, right: usize },
    Sum { inputs: Vec<usize> },
    /// One expression per output row: fan-out is fixed by the source, not by
    /// the data.
    FlatMap { input: usize, outputs: Vec<Arc<TypedExpr>> },
    FlatMapIndex { input: usize, pairs: Vec<(Arc<TypedExpr>, Arc<TypedExpr>)> },
    JoinIndex { left: usize, right: usize, key: Arc<TypedExpr>, value: Arc<TypedExpr> },
    Integrate { input: usize },
    Differentiate { input: usize },
    Delay { input: usize },
    /// An empty stream. Its type is fixed by where it is used.
    Empty,
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

/// The type of an expression. `None` is the type of a bare `null` literal: it
/// has no type of its own and takes one from context.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ty {
    Known(TypeDesc),
    None,
}

impl Ty {
    fn into_known(self, span: Span, what: impl fmt::Display) -> TResult<TypeDesc> {
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

fn optional(t: TypeDesc) -> TypeDesc {
    if t.is_optional() { t } else { TypeDesc::Optional(Box::new(t)) }
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

/// The declarations of one scope — the top level, or a circuit body.
struct Group<'a> {
    nodes: Vec<(&'a String, &'a Rhs, Span)>,
    specs: HashMap<&'a str, (&'a BatchType, Span)>,
}

fn collect<'a>(decls: &'a [Decl]) -> TResult<Group<'a>> {
    let mut g = Group { nodes: Vec::new(), specs: HashMap::new() };
    for decl in decls {
        match decl {
            Decl::Node { name, rhs, span } => {
                if g.nodes.iter().any(|(n, _, _)| *n == name) {
                    return err(*span, format!("`{name}` is declared more than once"));
                }
                g.nodes.push((name, rhs, *span));
            }
            Decl::TypeSpec { name, ty, span } => {
                if g.specs.insert(name, (ty, *span)).is_some() {
                    return err(*span, format!("`{name}` has more than one typespec"));
                }
            }
            Decl::Circuit(c) => {
                return err(c.span, "a circuit definition belongs at the top level");
            }
        }
    }
    for (name, (_, span)) in &g.specs {
        if !g.nodes.iter().any(|(n, _, _)| n.as_str() == *name) {
            return err(*span, format!("typespec for `{name}`, which is not declared"));
        }
    }
    Ok(g)
}

/// Names bound in the enclosing scope: a circuit body's parameters and the body
/// nodes checked so far. Empty at the top level.
type Scope = HashMap<String, usize>;

pub fn check(program: &Program) -> TResult<Plan> {
    let mut circuits: HashMap<&str, &CircuitDef> = HashMap::new();
    for decl in &program.decls {
        if let Decl::Circuit(c) = decl
            && circuits.insert(&c.name, c).is_some()
        {
            return err(c.span, format!("circuit `{}` is defined more than once", c.name));
        }
    }

    let top: Vec<&Decl> = program
        .decls
        .iter()
        .filter(|d| !matches!(d, Decl::Circuit(_)))
        .collect();
    let owned: Vec<Decl> = top.into_iter().cloned().collect();
    let group = collect(&owned)?;

    let mut plan = Plan { nodes: Vec::new(), by_name: HashMap::new() };
    let scope = Scope::new();
    for i in topo_order(&group.nodes)? {
        let (name, rhs, span) = group.nodes[i];
        check_decl(name, rhs, span, &mut plan, &group.specs, &circuits, &scope)?;
    }
    Ok(plan)
}

/// Checks one declaration, registering whatever names it introduces.
///
/// Most declarations add exactly one node. An instantiation adds one per body
/// node, under `<instance>.<node>`, and an alias adds no node at all.
fn check_decl(
    name: &str,
    rhs: &Rhs,
    span: Span,
    plan: &mut Plan,
    specs: &Specs<'_>,
    circuits: &HashMap<&str, &CircuitDef>,
    scope: &Scope,
) -> TResult<()> {
    match rhs {
        Rhs::Op(call) => {
            let (ty, plan_op) = check_op(name, call, span, plan, specs, scope)?;

            // An explicit typespec on a non-input node is checked, not used to
            // drive inference.
            if let Some((expected, spec_span)) = specs.get(name)
                && !matches!(plan_op, PlanOp::Input { .. })
                && **expected != ty
            {
                return err(
                    *spec_span,
                    format!("`{name}` is declared as `{expected}` but is inferred as `{ty}`"),
                );
            }
            plan.by_name.insert(name.to_string(), plan.nodes.len());
            plan.nodes.push(PlanNode { name: name.to_string(), ty, op: plan_op, span });
            Ok(())
        }

        Rhs::Ref(r) => {
            let idx = lookup(r, plan, scope)?;
            // An alias adds no node; the name simply points at an existing one.
            plan.by_name.insert(name.to_string(), idx);
            Ok(())
        }

        Rhs::Instantiate(inst) => expand(name, inst, plan, circuits, scope),

        Rhs::Fixpoint(inst) => err(
            inst.span,
            format!(
                "`fixpoint` is not implemented yet; `{}` can only be expanded",
                inst.circuit
            ),
        ),
    }
}

/// Expands a circuit body at the call site, binding parameters to the caller's
/// nodes and registering each body node as `<instance>.<node>`.
fn expand(
    instance: &str,
    inst: &Instantiation,
    plan: &mut Plan,
    circuits: &HashMap<&str, &CircuitDef>,
    outer: &Scope,
) -> TResult<()> {
    let Some(def) = circuits.get(inst.circuit.as_str()) else {
        return err(inst.span, format!("unknown circuit `{}`", inst.circuit));
    };

    for (label, _) in &inst.args {
        if !def.params.iter().any(|(l, _)| l == label) {
            return err(inst.span, format!("`{}` has no parameter `{label}`", inst.circuit));
        }
    }

    // Body typespecs are needed while binding parameters, since an `empty()`
    // argument takes its type from the node the parameter feeds.
    let group = collect(&def.body)?;
    let body_specs = &group.specs;

    let mut scope = Scope::new();
    for (label, internal) in &def.params {
        let Some((_, arg)) = inst.args.iter().find(|(l, _)| l == label) else {
            return err(inst.span, format!("missing argument `{label}` for `{}`", inst.circuit));
        };
        let resolved = resolve_arg(arg, plan, &Specs::new(), outer)?;
        let idx = match resolved {
            RArg::Stream(i) => i,
            // `empty()` takes the type of the body node this parameter feeds,
            // which is why a self-referential node needs a typespec.
            RArg::Empty(espan) => {
                let Some((ty, _)) = body_specs.get(label.as_str()) else {
                    return err(
                        espan,
                        format!(
                            "`empty()` has no type here: `{}` has no typespec for `{label}`, \
                             so nothing determines it. Add `{label} :: ...` to the circuit body.",
                            inst.circuit
                        ),
                    );
                };
                plan.nodes.push(PlanNode {
                    name: format!("empty@{espan}"),
                    ty: (*ty).clone(),
                    op: PlanOp::Empty,
                    span: espan,
                });
                plan.nodes.len() - 1
            }
            RArg::Name(n) => return err(inst.span, format!("unknown stream `{n}`")),
            _ => return err(inst.span, format!("argument `{label}` must be a stream")),
        };
        scope.insert(internal.clone(), idx);
    }

    for i in topo_order(&group.nodes)? {
        let (bname, rhs, bspan) = group.nodes[i];
        let mangled = format!("{instance}.{bname}");
        check_decl(&mangled, rhs, bspan, plan, &group.specs, circuits, &scope)?;
        // Rebind under the body-local name, and under the caller's typespec key,
        // so later body nodes see it by its short name.
        let idx = plan.by_name[&mangled];
        scope.insert(bname.clone(), idx);
    }
    Ok(())
}

/// Turns one argument into a node index, giving an `empty()` the type `want`.
fn materialize(
    a: &RArg<'_>,
    want: &BatchType,
    plan: &mut Plan,
    span: Span,
    op: &str,
) -> TResult<usize> {
    match a {
        RArg::Stream(i) => Ok(*i),
        RArg::Empty(espan) => {
            plan.nodes.push(PlanNode {
                name: format!("empty@{espan}"),
                ty: want.clone(),
                op: PlanOp::Empty,
                span: *espan,
            });
            Ok(plan.nodes.len() - 1)
        }
        RArg::Name(n) => err(span, format!("unknown stream `{n}`")),
        _ => err(span, format!("an argument of `{op}` must name a stream")),
    }
}

/// Resolves two operands where at most one may be an untyped `empty()`.
fn resolve_pair(
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

/// Resolves a possibly-dotted reference against the local scope, then the plan.
fn lookup(r: &NodeRef, plan: &Plan, scope: &Scope) -> TResult<usize> {
    let key = r.key();
    if let Some(i) = scope.get(&key) {
        return Ok(*i);
    }
    if let Some(i) = plan.by_name.get(&key) {
        return Ok(*i);
    }
    if r.field.is_none() {
        return err(r.span, format!("unknown stream `{key}`"));
    }
    err(r.span, format!("`{key}` is not a node of `{}`", r.base))
}

/// Every node name an operator call refers to, including inside nested calls.
/// A nested call is an inline tree, so it cannot itself participate in a cycle
/// — but a name buried in one is still a dependency.
fn collect_names<'a>(call: &'a OpCall, out: &mut Vec<&'a String>) {
    for a in &call.args {
        match a {
            Arg::Name(n) => out.push(n),
            // `inst.node` depends on `inst`, which is what introduces the name.
            Arg::Field(r) => out.push(&r.base),
            Arg::Op(inner) => collect_names(inner, out),
            _ => {}
        }
    }
}

fn collect_deps<'a>(rhs: &'a Rhs, out: &mut Vec<&'a String>) {
    match rhs {
        Rhs::Op(call) => collect_names(call, out),
        Rhs::Ref(r) => out.push(&r.base),
        Rhs::Instantiate(inst) | Rhs::Fixpoint(inst) => {
            for (_, a) in &inst.args {
                match a {
                    Arg::Name(n) => out.push(n),
                    Arg::Field(r) => out.push(&r.base),
                    Arg::Op(inner) => collect_names(inner, out),
                    _ => {}
                }
            }
        }
    }
}

/// Dependency order, rejecting cycles. Recursion needs `delay`, which this cut
/// does not implement, so any cycle is an error rather than a fixpoint.
fn topo_order(decls: &[(&String, &Rhs, Span)]) -> TResult<Vec<usize>> {
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
            let (name, rhs, span) = decls[node];
            if child == 0 {
                if marks[node] == Mark::Done {
                    continue;
                }
                marks[node] = Mark::Active;
            }
            let mut deps: Vec<&String> = Vec::new();
            collect_deps(rhs, &mut deps);
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

/// An argument after nested calls have been resolved into nodes.
enum RArg<'a> {
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

type Specs<'a> = HashMap<&'a str, (&'a BatchType, Span)>;

/// Resolves one argument, checking and pushing a node for a nested call.
///
/// Recursion is depth-first and pushes before returning, so a nested node
/// always lands at a lower index than the node using it — which is what
/// `lower.rs` needs, since it builds `plan.nodes` in order.
fn resolve_arg<'a>(
    arg: &'a Arg,
    plan: &mut Plan,
    specs: &Specs<'_>,
    scope: &Scope,
) -> TResult<RArg<'a>> {
    Ok(match arg {
        Arg::Str(_) => RArg::Str,
        Arg::Fun(f) => RArg::Fun(f),
        Arg::Field(r) => RArg::Stream(lookup(r, plan, scope)?),
        Arg::Name(n) => match scope.get(n.as_str()).or_else(|| plan.by_name.get(n.as_str())) {
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
            let (ty, op) = check_op(&name, call, call.span, plan, specs, scope)?;
            plan.nodes.push(PlanNode { name, ty, op, span: call.span });
            RArg::Stream(plan.nodes.len() - 1)
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

fn check_op(
    name: &str,
    call: &OpCall,
    span: Span,
    plan: &mut Plan,
    specs: &Specs<'_>,
    scope: &Scope,
) -> TResult<(BatchType, PlanOp)> {
    let op = call.op.as_str();
    let args = &call.args;

    // Resolve nested calls first, so the mutable borrow of `plan` ends before
    // anything below reads it.
    let mut rargs = Vec::with_capacity(args.len());
    for a in args {
        rargs.push(resolve_arg(a, plan, specs, scope)?);
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
            let Some((spec, _)) = specs.get(name) else {
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
            let Some((ty, _)) = specs.get(name) else {
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
