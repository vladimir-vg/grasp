//! Type inference and name resolution.
//!
//! Produces a [`Plan`]: nodes in dependency order, each with a [`BatchType`] and
//! an operator whose function arguments have been lowered to [`TypedExpr`].
//!
//! Resolving `row.name` to a positional index happens here. That is what allows
//! record *values* to be positional at runtime, and it is why nothing in the hot
//! path compares field-name strings.

pub mod circuits;
pub mod content;
pub mod infer;
pub mod ops;
pub mod plan;

pub use content::{body_content_ids, content_ids};
pub use plan::{AGGREGATORS, Agg, KeyValue, OPERATORS, Plan, PlanNode, PlanOp};

use circuits::{check_fixpoint, expand};
use ops::check_op;

use crate::diag::{Diagnostic, Pass, Span};
use crate::lang::{Arg, CircuitDef, Decl, Expr, ExprKind, FunctionDef, NodeRef, OpCall, Program, Rhs};
use crate::value::BatchType;
use std::collections::HashMap;

pub(super) type TResult<T> = Result<T, Diagnostic>;

/// Typespecs in scope, keyed by the node they annotate.
pub(super) type Specs<'a> = HashMap<&'a str, (&'a BatchType, Span)>;

pub(super) fn err<T>(span: Span, message: impl Into<String>) -> TResult<T> {
    Err(Diagnostic::error(Pass::Typecheck, span, message))
}

/// The declarations of one scope — the top level, or a circuit body.
pub(super) struct Group<'a> {
    nodes: Vec<(&'a String, &'a Rhs, Span)>,
    pub(super) specs: HashMap<&'a str, (&'a BatchType, Span)>,
}

pub(super) fn collect<'a>(decls: &'a [Decl]) -> TResult<Group<'a>> {
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
            // Collected separately: functions are visible from every scope, so
            // they are not part of any one group's declarations.
            Decl::Function(_) => {}
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
pub(super) type Scope = HashMap<String, usize>;

/// Everything a declaration is checked *against*, as opposed to the plan it
/// adds to. These four always travel together; passing them separately is what
/// pushed `check_decl` past a readable argument count.
#[derive(Clone, Copy)]
pub(super) struct Env<'a> {
    pub(super) specs: &'a Specs<'a>,
    pub(super) circuits: &'a HashMap<&'a str, &'a CircuitDef>,
    /// Names bound by the enclosing circuit body: its parameters, and the body
    /// nodes checked so far.
    pub(super) scope: &'a Scope,
    /// The instance a body is being expanded under, or empty at the top level.
    /// Body nodes are registered as `<prefix>.<node>`.
    pub(super) prefix: &'a str,
    /// Whether this declaration sits inside a `fixpoint` body. One level is
    /// supported and a second is rejected here rather than at lowering, which
    /// has no span to point at and no way to say what to do instead.
    pub(super) in_fixpoint: bool,
    /// Named functions. Visible from every scope, since a function is a
    /// template with no access to anything but its own parameters.
    pub(super) funcs: &'a Functions<'a>,
}

pub(super) type Functions<'a> = HashMap<&'a str, &'a FunctionDef>;

pub fn check(program: &Program) -> TResult<Plan> {
    let mut circuits: HashMap<&str, &CircuitDef> = HashMap::new();
    for decl in &program.decls {
        if let Decl::Circuit(c) = decl
            && circuits.insert(&c.name, c).is_some()
        {
            return err(c.span, format!("circuit `{}` is defined more than once", c.name));
        }
    }

    check_circuit_cycles(&circuits)?;

    let mut funcs: Functions = HashMap::new();
    for decl in &program.decls {
        if let Decl::Function(f) = decl
            && funcs.insert(&f.name, f).is_some()
        {
            return err(f.span, format!("function `{}` is defined more than once", f.name));
        }
    }
    check_function_cycles(&funcs)?;

    let top: Vec<&Decl> = program
        .decls
        .iter()
        .filter(|d| !matches!(d, Decl::Circuit(_) | Decl::Function(_)))
        .collect();
    let owned: Vec<Decl> = top.into_iter().cloned().collect();
    let group = collect(&owned)?;

    let mut plan = Plan { nodes: Vec::new(), by_name: HashMap::new() };
    let scope = Scope::new();
    for i in topo_order(&group.nodes)? {
        let (name, rhs, span) = group.nodes[i];
        let env = Env {
            specs: &group.specs,
            circuits: &circuits,
            scope: &scope,
            prefix: "",
            in_fixpoint: false,
            funcs: &funcs,
        };
        check_decl(name, rhs, span, &mut plan, env)?;
    }
    check_one_type_per_table(&plan)?;
    check_no_name_collides_with_a_function(&plan, &funcs)?;
    Ok(plan)
}

/// Rejects a function that calls itself, directly or through others.
///
/// A function is fully inlined at check time, so a cycle would not terminate at
/// *compile* time, never mind at runtime. `fixpoint` is what recursion is for.
fn check_function_cycles(funcs: &Functions<'_>) -> TResult<()> {
    fn calls<'a>(e: &'a Expr, out: &mut Vec<(&'a str, Span)>) {
        match &e.kind {
            ExprKind::Call(name, args) => {
                out.push((name.as_str(), e.span));
                args.iter().for_each(|a| calls(a, out));
            }
            ExprKind::Field(b, _) => calls(b, out),
            ExprKind::Unary(_, i) => calls(i, out),
            ExprKind::Binary(_, l, r) => {
                calls(l, out);
                calls(r, out);
            }
            ExprKind::Record(fields) => fields.iter().for_each(|(_, v)| calls(v, out)),
            ExprKind::List(items) => items.iter().for_each(|i| calls(i, out)),
            _ => {}
        }
    }

    fn walk<'a>(
        name: &'a str,
        funcs: &Functions<'a>,
        path: &mut Vec<&'a str>,
        done: &mut std::collections::HashSet<&'a str>,
    ) -> TResult<()> {
        let Some(def) = funcs.get(name) else {
            return Ok(()); // a builtin, or reported when the call is checked
        };
        if let Some(at) = path.iter().position(|p| *p == name) {
            let mut cycle: Vec<&str> = path[at..].to_vec();
            cycle.push(name);
            return err(
                def.span,
                format!(
                    "function `{name}` calls itself through {}; a function is inlined, \
                     so this would not terminate. Use `fixpoint` for recursion.",
                    cycle.join(" -> ")
                ),
            );
        }
        if !done.insert(name) {
            return Ok(());
        }
        path.push(name);
        let mut called = Vec::new();
        calls(&def.body, &mut called);
        for (callee, _) in called {
            walk(callee, funcs, path, done)?;
        }
        path.pop();
        Ok(())
    }

    let mut names: Vec<&str> = funcs.keys().copied().collect();
    names.sort();
    let mut done = std::collections::HashSet::new();
    for name in names {
        walk(name, funcs, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

/// A name means one thing. A node and a function sharing one would make a bare
/// name mean a stream in one argument position and a function in another.
fn check_no_name_collides_with_a_function(plan: &Plan, funcs: &Functions<'_>) -> TResult<()> {
    for (name, idx) in &plan.by_name {
        if let Some(def) = funcs.get(name.as_str()) {
            return err(
                def.span,
                format!(
                    "`{name}` is both a function and a node (declared at {}); \
                     a name means one thing",
                    plan.nodes[*idx].span
                ),
            );
        }
    }
    Ok(())
}

/// A table has one schema. Identical `input("t")` declarations dedup into one
/// node; two that survive mean two different types were declared for it, which
/// would otherwise build two streams while `Runner` keeps only one handle —
/// a circuit that looks wired and is not.
fn check_one_type_per_table(plan: &Plan) -> TResult<()> {
    let mut seen: HashMap<&str, (&BatchType, Span)> = HashMap::new();
    for node in &plan.nodes {
        let PlanOp::Input { table } = &node.op else { continue };
        if let Some((prev, _)) = seen.get(table.as_str()) {
            return err(
                node.span,
                format!(
                    "table `{table}` is declared with two different types, \
                     `{prev}` and `{}`",
                    node.ty
                ),
            );
        }
        seen.insert(table, (&node.ty, node.span));
    }
    Ok(())
}

/// Adds a node, reusing an existing one with the same operator, inputs and
/// parameters.
///
/// Nodes are content-addressed: identity is `(ty, op)`, deliberately excluding
/// the name and span, so two names for one computation share a node and the
/// first name is the one diagnostics use. This changes no result — `plus(x, x)`
/// still adds a stream to itself and doubles the weights — it only avoids
/// building the same operator twice.
///
/// `Fixpoint` is excluded: its body carries spans, so structural equality would
/// be span-sensitive and would never match anyway.
pub(super) fn push_node(plan: &mut Plan, name: String, ty: BatchType, op: PlanOp, span: Span) -> usize {
    if !matches!(op, PlanOp::Fixpoint { .. })
        && let Some(i) = plan.nodes.iter().position(|n| n.ty == ty && n.op == op)
    {
        return i;
    }
    plan.nodes.push(PlanNode { name, ty, op, span });
    plan.nodes.len() - 1
}

/// Rejects a circuit that instantiates itself, directly or through others.
///
/// Expansion is inlining, so a definition-level cycle would expand forever —
/// which without this check means a stack overflow rather than a diagnostic.
fn check_circuit_cycles(circuits: &HashMap<&str, &CircuitDef>) -> TResult<()> {
    fn walk<'a>(
        name: &'a str,
        circuits: &HashMap<&str, &'a CircuitDef>,
        path: &mut Vec<&'a str>,
        done: &mut std::collections::HashSet<&'a str>,
    ) -> TResult<()> {
        let Some(def) = circuits.get(name) else {
            return Ok(()); // reported when the instantiation is checked
        };
        if let Some(at) = path.iter().position(|p| *p == name) {
            let mut cycle: Vec<&str> = path[at..].to_vec();
            cycle.push(name);
            return err(
                def.span,
                format!(
                    "circuit `{name}` instantiates itself through {}; expansion \
                     would not terminate. Use `fixpoint` for recursion.",
                    cycle.join(" -> ")
                ),
            );
        }
        if done.contains(name) {
            return Ok(());
        }
        path.push(name);
        for decl in &def.body {
            if let Decl::Node { rhs: Rhs::Instantiate(i) | Rhs::Fixpoint(i), .. } = decl {
                walk(&i.circuit, circuits, path, done)?;
            }
        }
        path.pop();
        done.insert(name);
        Ok(())
    }

    let mut names: Vec<&str> = circuits.keys().copied().collect();
    names.sort();
    let mut done = std::collections::HashSet::new();
    for name in names {
        walk(name, circuits, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

/// Checks one declaration, registering whatever names it introduces.
///
/// Most declarations add exactly one node. An instantiation adds one per body
/// node, under `<instance>.<node>`, and an alias adds no node at all.
pub(super) fn check_decl(
    name: &str,
    rhs: &Rhs,
    span: Span,
    plan: &mut Plan,
    env: Env<'_>,
) -> TResult<()> {
    match rhs {
        Rhs::Op(call) => {
            let (ty, plan_op) = check_op(name, call, span, plan, env)?;

            // An explicit typespec on a non-input node is checked, not used to
            // drive inference.
            if let Some((expected, spec_span)) = env.specs.get(name)
                && !matches!(plan_op, PlanOp::Input { .. })
                && **expected != ty
            {
                return err(
                    *spec_span,
                    format!("`{name}` is declared as `{expected}` but is inferred as `{ty}`"),
                );
            }
            let idx = push_node(plan, name.to_string(), ty, plan_op, span);
            plan.by_name.insert(name.to_string(), idx);
            Ok(())
        }

        Rhs::Ref(r) => {
            let idx = lookup(r, plan, env.scope, env.prefix)?;
            // An alias adds no node; the name simply points at an existing one.
            plan.by_name.insert(name.to_string(), idx);
            Ok(())
        }

        Rhs::Instantiate(inst) => expand(name, inst, plan, env),

        Rhs::Fixpoint(inst) => {
            // `dbsp` allows circuits at arbitrary depth, but each level is a
            // distinct Rust circuit type needing its own instantiation of the
            // lowering. The lowering therefore cannot build this, and its
            // `Import` indices would refer to the wrong node list if it tried.
            if env.in_fixpoint {
                return err(
                    span,
                    format!(
                        "`fixpoint` cannot be nested: `{name}` is already inside one. \
                         Lift it out, or make both recursions parameters of a single \
                         `fixpoint` — one fixpoint may have several recursive streams."
                    ),
                );
            }
            check_fixpoint(name, inst, plan, env)
        }
    }
}

/// Resolves a dotted reference: the local scope first, then the enclosing
/// instance's namespace, then the global one.
///
/// The middle step is what lets a circuit body refer to a nested
/// instantiation's nodes by their short path — inside `c`, `inner.node` finds
/// `c.inner.node`.
pub(super) fn lookup(r: &NodeRef, plan: &Plan, scope: &Scope, prefix: &str) -> TResult<usize> {
    let key = r.key();
    if let Some(i) = scope.get(&key) {
        return Ok(*i);
    }
    if !prefix.is_empty()
        && let Some(i) = plan.by_name.get(&format!("{prefix}.{key}"))
    {
        return Ok(*i);
    }
    if let Some(i) = plan.by_name.get(&key) {
        return Ok(*i);
    }
    if r.path.len() == 1 {
        return err(r.span, format!("unknown stream `{key}`"));
    }
    err(r.span, format!("`{key}` is not a node of `{}`", r.base()))
}

/// Every node name an operator call refers to, including inside nested calls.
/// A nested call is an inline tree, so it cannot itself participate in a cycle
/// — but a name buried in one is still a dependency.
pub(super) fn collect_names<'a>(call: &'a OpCall, out: &mut Vec<&'a String>) {
    for a in &call.args {
        match a {
            Arg::Name(n) => out.push(n),
            // `inst.node` depends on `inst`, which is what introduces the name.
            Arg::Field(r) => out.push(r.base()),
            Arg::Op(inner) => collect_names(inner, out),
            _ => {}
        }
    }
}

pub(super) fn collect_deps<'a>(rhs: &'a Rhs, out: &mut Vec<&'a String>) {
    match rhs {
        Rhs::Op(call) => collect_names(call, out),
        Rhs::Ref(r) => out.push(r.base()),
        Rhs::Instantiate(inst) | Rhs::Fixpoint(inst) => {
            for (_, a) in &inst.args {
                match a {
                    Arg::Name(n) => out.push(n),
                    Arg::Field(r) => out.push(r.base()),
                    Arg::Op(inner) => collect_names(inner, out),
                    _ => {}
                }
            }
        }
    }
}

/// Dependency order, rejecting cycles. Recursion needs `delay`, which this cut
/// does not implement, so any cycle is an error rather than a fixpoint.
pub(super) fn topo_order(decls: &[(&String, &Rhs, Span)]) -> TResult<Vec<usize>> {
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
