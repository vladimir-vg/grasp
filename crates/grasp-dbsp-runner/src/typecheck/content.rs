//! Content addressing: what a node *is*, independent of how it was written.
//!
//! `push_node` already treats `(batch type, PlanOp)` as a node's identity,
//! deliberately excluding its name and span. This module turns that identity
//! into a stable string.
//!
//! The id is a **Merkle hash**: an operand contributes its own id rather than
//! its index, so the result depends on the shape of the computation and not on
//! where a node happened to land in the list. Two programs that describe the
//! same dataflow with different whitespace, declaration order or intermediate
//! names produce the same ids.
//!
//! That property is the point. A node's id is what a `persistent_id` must be —
//! `dbsp` warns that the names "must identify the same computation across
//! restarts, so derive them from the program (a view name, a hash of the
//! subgraph) rather than from anything positional"
//! (`dbsp/src/operator/recursive.rs`). Node *names* do not qualify: a nested
//! node's name is `filter@3:12`, which is a source position, and a deduplicated
//! node keeps whichever name was written first. The content id is also the
//! stable handle for observing a node that has no name at all.

use super::plan::{Plan, PlanOp};
use crate::expr::TypedExpr;
use std::hash::{Hash, Hasher};
use xxhash_rust::xxh3::Xxh3Default;

/// The content id of every node, in plan order.
///
/// One forward pass suffices because an operand is always built before the node
/// that uses it — the property `tests/plan.rs` pins.
pub fn content_ids(plan: &Plan) -> Vec<String> {
    let mut ids: Vec<String> = Vec::with_capacity(plan.nodes.len());
    for node in &plan.nodes {
        let mut h = Xxh3Default::new();
        node.ty.hash(&mut h);
        hash_op(&mut h, &node.op, &ids);
        ids.push(format!("n{:016x}", h.finish()));
    }
    ids
}

/// Hashes one operator: a tag for which it is, its parameters, and the *ids* of
/// its operands.
fn hash_op(h: &mut Xxh3Default, op: &PlanOp, ids: &[String]) {
    // An explicit tag per variant, rather than anything derived, so that
    // reordering the enum cannot silently change every id in existence.
    let (tag, inputs): (u8, &[usize]) = match op {
        PlanOp::Input { table } => {
            table.hash(h);
            (1, &[])
        }
        PlanOp::Map { input, f } => {
            hash_expr(h, f);
            (2, std::slice::from_ref(input))
        }
        PlanOp::Filter { input, f } => {
            hash_expr(h, f);
            (3, std::slice::from_ref(input))
        }
        PlanOp::MapIndex { input, f, kv } => {
            hash_expr(h, f);
            kv.key.hash(h);
            kv.value.hash(h);
            (4, std::slice::from_ref(input))
        }
        PlanOp::FlatMap { input, f } => {
            hash_expr(h, f);
            (5, std::slice::from_ref(input))
        }
        PlanOp::FlatMapIndex { input, f, kv } => {
            hash_expr(h, f);
            kv.key.hash(h);
            kv.value.hash(h);
            (6, std::slice::from_ref(input))
        }
        PlanOp::Join { left, right, f } => {
            hash_expr(h, f);
            return finish(h, 7, &[*left, *right], ids);
        }
        PlanOp::JoinIndex { left, right, f, kv } => {
            hash_expr(h, f);
            kv.key.hash(h);
            kv.value.hash(h);
            return finish(h, 8, &[*left, *right], ids);
        }
        PlanOp::Antijoin { left, right } => return finish(h, 9, &[*left, *right], ids),
        PlanOp::Distinct { input } => (10, std::slice::from_ref(input)),
        PlanOp::Aggregate {
            input,
            agg,
            f,
            projection,
        } => {
            (*agg as u8).hash(h);
            projection.hash(h);
            hash_expr(h, f);
            (11, std::slice::from_ref(input))
        }
        PlanOp::WeightedCount { input } => (12, std::slice::from_ref(input)),
        PlanOp::Neg { input } => (13, std::slice::from_ref(input)),
        PlanOp::Plus { left, right } => return finish(h, 14, &[*left, *right], ids),
        PlanOp::Minus { left, right } => return finish(h, 15, &[*left, *right], ids),
        PlanOp::Sum { inputs } => return finish(h, 16, inputs, ids),
        PlanOp::Integrate { input } => (17, std::slice::from_ref(input)),
        PlanOp::Differentiate { input } => (18, std::slice::from_ref(input)),
        PlanOp::Delay { input } => (19, std::slice::from_ref(input)),
        PlanOp::Empty => (20, &[]),

        // A fixpoint's identity is its body's. The body is hashed with its own
        // ids, which is what lets `Import` contribute the *parent* node's id
        // rather than an index meaningful only inside this sub-plan.
        PlanOp::Fixpoint { body, outputs } => {
            let body_ids = body_content_ids(ids, body);
            23u8.hash(h);
            for &o in outputs {
                body_ids[o].hash(h);
            }
            return;
        }
        PlanOp::FixpointExport { fixpoint, slot } => {
            slot.hash(h);
            (24, std::slice::from_ref(fixpoint))
        }

        // Only ever reached inside a fixpoint body, which handles them above.
        PlanOp::Import { outer } => (25, std::slice::from_ref(outer)),
        PlanOp::RecVar { slot } => {
            slot.hash(h);
            (26, &[])
        }
    };
    finish(h, tag, inputs, ids);
}

/// The content ids of a fixpoint body's nodes.
///
/// Separate from [`content_ids`] because the lowering needs them too: `dbsp`
/// requires a persistent id on every stream inside a recursive scope, assigned
/// before anything is built from it.
pub fn body_content_ids(parent_ids: &[String], body: &[crate::typecheck::PlanNode]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::with_capacity(body.len());
    for bn in body {
        let mut h = Xxh3Default::new();
        bn.ty.hash(&mut h);
        match &bn.op {
            // An imported stream is identified by the parent node it imports.
            PlanOp::Import { outer } => {
                21u8.hash(&mut h);
                parent_ids[*outer].hash(&mut h);
            }
            // A recursive slot is meaningful only within its own fixpoint, so
            // the slot number is the whole of its identity.
            PlanOp::RecVar { slot } => {
                22u8.hash(&mut h);
                slot.hash(&mut h);
            }
            other => hash_op(&mut h, other, &ids),
        }
        ids.push(format!("n{:016x}", h.finish()));
    }
    ids
}

fn finish(h: &mut Xxh3Default, tag: u8, inputs: &[usize], ids: &[String]) {
    tag.hash(h);
    for &i in inputs {
        // An operand contributes what it *is*, not where it sits.
        ids[i].hash(h);
    }
}

/// Hashes a conversion. Two of them carry a type, which is part of what the
/// conversion *is* — extracting a document into one record type is not the same
/// computation as extracting it into another.
fn hash_conv(h: &mut Xxh3Default, conv: &crate::expr::Conv) {
    use crate::expr::Conv::*;
    let tag: u8 = match conv {
        Identity => 0,
        IntToFloat => 1,
        FloatToInt => 2,
        BoolToString => 3,
        IntToString => 4,
        FloatToString => 5,
        StringToBool => 6,
        StringToInt => 7,
        StringToFloat => 8,
        FromJson(ty) => {
            9u8.hash(h);
            ty.hash(h);
            return;
        }
        ToJson(ty) => {
            10u8.hash(h);
            ty.hash(h);
            return;
        }
    };
    tag.hash(h);
}

/// Hashes a compiled expression structurally, matching the `PartialEq` that
/// `push_node` compares them with.
fn hash_expr(h: &mut Xxh3Default, e: &TypedExpr) {
    match e {
        TypedExpr::Const(v) => {
            0u8.hash(h);
            v.hash(h);
        }
        // Unreachable in a checked plan — `commit` pins every literal — but
        // hashed distinctly rather than merged with `Const`.
        TypedExpr::IntLit(v) => {
            1u8.hash(h);
            v.hash(h);
        }
        TypedExpr::FloatLit(v) => {
            2u8.hash(h);
            v.to_bits().hash(h);
        }
        TypedExpr::Var(i) => {
            3u8.hash(h);
            i.hash(h);
        }
        TypedExpr::Field(base, index) => {
            4u8.hash(h);
            index.hash(h);
            hash_expr(h, base);
        }
        TypedExpr::Record(fields) => {
            5u8.hash(h);
            fields.len().hash(h);
            fields.iter().for_each(|f| hash_expr(h, f));
        }
        TypedExpr::Array(items) => {
            6u8.hash(h);
            items.len().hash(h);
            items.iter().for_each(|i| hash_expr(h, i));
        }
        TypedExpr::Dict(entries) => {
            11u8.hash(h);
            entries.len().hash(h);
            entries.iter().for_each(|(k, v)| {
                hash_expr(h, k);
                hash_expr(h, v);
            });
        }
        TypedExpr::DictFrom(inner) => {
            12u8.hash(h);
            hash_expr(h, inner);
        }
        TypedExpr::Unary(op, inner) => {
            7u8.hash(h);
            (*op as u8).hash(h);
            hash_expr(h, inner);
        }
        TypedExpr::Binary(op, l, r) => {
            8u8.hash(h);
            (*op as u8).hash(h);
            hash_expr(h, l);
            hash_expr(h, r);
        }
        TypedExpr::Cast(inner, conv) => {
            10u8.hash(h);
            hash_conv(h, conv);
            hash_expr(h, inner);
        }
        TypedExpr::Call(f, args) => {
            9u8.hash(h);
            (*f as u8).hash(h);
            args.len().hash(h);
            args.iter().for_each(|a| hash_expr(h, a));
        }
    }
}
