//! Instantiating a circuit: expanding its body at the call site, and iterating
//! it to convergence.

use super::ops::{RArg, resolve_arg};
use super::plan::{Plan, PlanNode, PlanOp};
use super::{Env, Scope, Specs, TResult, check_decl, collect, err, push_node, topo_order};
use crate::diag::Span;
use crate::lang::{Arg, Instantiation, OpCall, Rhs};
use crate::value::BatchType;
use std::collections::HashMap;

/// Expands a circuit body at the call site, binding parameters to the caller's
/// nodes and registering each body node as `<instance>.<node>`.
pub(super) fn expand(
    instance: &str,
    inst: &Instantiation,
    plan: &mut Plan,
    env: Env<'_>,
) -> TResult<()> {
    let Some(def) = env.circuits.get(inst.circuit.as_str()) else {
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
        let resolved = resolve_arg(arg, plan, Env { specs: &Specs::new(), scope: env.scope, ..env })?;
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
                push_node(plan, format!("empty@{espan}"), (*ty).clone(), PlanOp::Empty, espan)
            }
            RArg::Name(n) => return err(inst.span, format!("unknown stream `{n}`")),
            _ => return err(inst.span, format!("argument `{label}` must be a stream")),
        };
        scope.insert(internal.clone(), idx);
    }

    for i in topo_order(&group.nodes)? {
        let (bname, rhs, bspan) = group.nodes[i];
        let mangled = format!("{instance}.{bname}");
        check_decl(&mangled, rhs, bspan, plan, Env { specs: &group.specs, scope: &scope, prefix: instance, ..env })?;
        // Only bind a body-local shorthand when a node was actually registered
        // under that name. A nested instantiation registers a *namespace*, so
        // there is nothing to bind and its members are reached by their path.
        if let Some(idx) = plan.by_name.get(&mangled).copied() {
            scope.insert(bname.clone(), idx);
        }
    }
    Ok(())
}

/// Works out the batch type of as many body nodes as possible, without
/// building anything.
///
/// This exists so a recursive stream need not be annotated. The realistic
/// recursive shape is `path := plus(base, step)`, and `plus` *equates* its
/// operands' types — so `path` has `base`'s type whatever `step` turns out to
/// be, even though `step` consumes `path`. No solver is needed, only the
/// operators whose result type is one of their operands.
///
/// What it cannot reach: `map`, `join`, `aggregate` and the like, whose result
/// type is computed by a function. Their type cannot be known without first
/// knowing the value type being solved for, so those still need a typespec. A
/// recursion defined only that way has no base case and computes nothing, so
/// the gap is theoretical.
///
/// Inference never weakens checking. It supplies a starting type; the body is
/// then checked exactly as before, and `plus` still rejects operands that
/// disagree.
fn infer_shapes(nodes: &[(&String, &Rhs, Span)], known: &mut HashMap<String, BatchType>) {
    // A node can become knowable once another does, so iterate to a fixed
    // point. Each round learns at least one type or stops.
    loop {
        let mut learned = false;
        for (name, rhs, _) in nodes {
            if known.contains_key(name.as_str()) {
                continue;
            }
            if let Some(ty) = shape_of_rhs(rhs, known) {
                known.insert((*name).clone(), ty);
                learned = true;
            }
        }
        if !learned {
            return;
        }
    }
}

fn shape_of_rhs(rhs: &Rhs, known: &HashMap<String, BatchType>) -> Option<BatchType> {
    match rhs {
        Rhs::Op(call) => shape_of_call(call, known),
        Rhs::Ref(r) => known.get(&r.key()).cloned(),
        // An instantiation defines a namespace rather than a stream, and a
        // fixpoint's own type is not needed here.
        Rhs::Instantiate(_) | Rhs::Fixpoint(_) => None,
    }
}

fn shape_of_call(call: &OpCall, known: &HashMap<String, BatchType>) -> Option<BatchType> {
    match call.op.as_str() {
        // Identical batch types are required, so any known operand settles it.
        "plus" | "minus" | "sum" => call.args.iter().find_map(|a| shape_of_arg(a, known)),
        // Shape-preserving: the result is the input's type.
        "distinct" | "neg" | "filter" | "integrate" | "differentiate" | "delay" => {
            shape_of_arg(call.args.first()?, known)
        }
        _ => None,
    }
}

fn shape_of_arg(arg: &Arg, known: &HashMap<String, BatchType>) -> Option<BatchType> {
    match arg {
        Arg::Name(n) => known.get(n.as_str()).cloned(),
        Arg::Field(r) => known.get(&r.key()).cloned(),
        Arg::Op(call) => shape_of_call(call, known),
        _ => None,
    }
}

/// Builds a `fixpoint` instantiation: the body becomes a sub-plan whose
/// parameters are either imported from the parent or bound to a recursive slot.
pub(super) fn check_fixpoint(
    instance: &str,
    inst: &Instantiation,
    plan: &mut Plan,
    env: Env<'_>,
) -> TResult<()> {
    let Some(def) = env.circuits.get(inst.circuit.as_str()) else {
        return err(inst.span, format!("unknown circuit `{}`", inst.circuit));
    };
    for (label, _) in &inst.args {
        if !def.params.iter().any(|(l, _)| l == label) {
            return err(inst.span, format!("`{}` has no parameter `{label}`", inst.circuit));
        }
    }
    let group = collect(&def.body)?;

    let mut sub = Plan { nodes: Vec::new(), by_name: HashMap::new() };
    let mut scope = Scope::new();
    let mut recs: Vec<(String, BatchType)> = Vec::new();

    // A parameter is self-referential when a body node shares its label.
    let recursive = |label: &str| group.nodes.iter().any(|(n, _, _)| n.as_str() == label);

    // Pass 1: resolve the base parameters, which is where every known type
    // enters the body.
    let mut base: Vec<(&String, usize)> = Vec::new();
    let mut known: HashMap<String, BatchType> = HashMap::new();
    for (label, internal) in &def.params {
        let Some((_, arg)) = inst.args.iter().find(|(l, _)| l == label) else {
            return err(inst.span, format!("missing argument `{label}` for `{}`", inst.circuit));
        };
        if recursive(label) {
            if !matches!(arg, Arg::Op(c) if c.op == "empty") {
                return err(
                    inst.span,
                    format!(
                        "`{label}` is recursive, so it starts empty: pass `empty()`. \
                         A base case belongs in the circuit body."
                    ),
                );
            }
            continue;
        }
        let idx = match resolve_arg(arg, plan, Env { specs: &Specs::new(), scope: env.scope, ..env })? {
            RArg::Stream(i) => i,
            RArg::Name(n) => return err(inst.span, format!("unknown stream `{n}`")),
            _ => return err(inst.span, format!("argument `{label}` must be a stream")),
        };
        known.insert(internal.clone(), plan.nodes[idx].ty.clone());
        base.push((internal, idx));
    }

    // Pass 2: work out what type each recursive stream will have. See
    // `infer_shapes` for what this can and cannot reach.
    infer_shapes(&group.nodes, &mut known);

    // Pass 3: build the body's parameter nodes, now that the types are settled.
    for (internal, idx) in base {
        sub.nodes.push(PlanNode {
            name: format!("<import {internal}>"),
            ty: plan.nodes[idx].ty.clone(),
            op: PlanOp::Import { outer: idx },
            span: inst.span,
        });
        scope.insert(internal.clone(), sub.nodes.len() - 1);
    }
    for (label, internal) in &def.params {
        if !recursive(label) {
            continue;
        }
        let declared = group.specs.get(label.as_str()).map(|(t, _)| (*t).clone());
        let inferred = known.get(label.as_str()).cloned();
        let ty = match (declared, inferred) {
            // A typespec is checked against inference, not an override, which
            // is the rule every other node follows.
            (Some(d), Some(i)) if d != i => {
                return err(
                    group.specs[label.as_str()].1,
                    format!("`{label}` is declared as `{d}` but is inferred as `{i}`"),
                );
            }
            (Some(d), _) => d,
            (None, Some(i)) => i,
            (None, None) => {
                return err(
                    inst.span,
                    format!(
                        "`{label}` is recursive and its type cannot be inferred: it is \
                         computed by an operator whose result type comes from a function, \
                         which cannot be known from a body that consumes `{label}`. \
                         Add `{label} :: ...` to the body of `{}`.",
                        inst.circuit
                    ),
                );
            }
        };
        let slot = recs.len();
        sub.nodes.push(PlanNode {
            name: format!("<{label}>"),
            ty: ty.clone(),
            op: PlanOp::RecVar { slot },
            span: inst.span,
        });
        scope.insert(internal.clone(), sub.nodes.len() - 1);
        recs.push((label.clone(), ty));
    }

    if recs.is_empty() {
        return err(
            inst.span,
            format!(
                "`{}` has no recursive parameter, so there is nothing to iterate; \
                 a parameter is recursive when a body node shares its label",
                inst.circuit
            ),
        );
    }
    // One nested batch type serves every recursive stream, so they must agree
    // on shape.
    let flat = matches!(recs[0].1, BatchType::ZSet(_));
    for (label, ty) in &recs {
        if matches!(ty, BatchType::ZSet(_)) != flat {
            return err(
                inst.span,
                format!("`{label}` has a different shape from the other recursive streams"),
            );
        }
    }

    for i in topo_order(&group.nodes)? {
        let (bname, rhs, bspan) = group.nodes[i];
        check_decl(bname, rhs, bspan, &mut sub, Env { specs: &group.specs, scope: &scope, prefix: "", ..env })?;
        if let Some(idx) = sub.by_name.get(bname.as_str()).copied() {
            scope.insert(bname.clone(), idx);
        }
    }

    let outputs: Vec<usize> = recs.iter().map(|(l, _)| sub.by_name[l.as_str()]).collect();
    let first = recs[0].1.clone();
    plan.nodes.push(PlanNode {
        name: instance.to_string(),
        ty: first,
        op: PlanOp::Fixpoint { body: sub.nodes, outputs },
        span: inst.span,
    });
    let fixpoint = plan.nodes.len() - 1;

    // Only the recursive streams leave the nested circuit; other body nodes
    // exist only inside it.
    for (slot, (label, ty)) in recs.into_iter().enumerate() {
        let name = format!("{instance}.{label}");
        plan.by_name.insert(name.clone(), plan.nodes.len());
        plan.nodes.push(PlanNode {
            name,
            ty,
            op: PlanOp::FixpointExport { fixpoint, slot },
            span: inst.span,
        });
    }
    Ok(())
}
