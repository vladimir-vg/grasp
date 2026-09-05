//! Structural properties of a checked `Plan` that the fixtures cannot express.

use dbsp_runner::compile;
use dbsp_runner::lower::Runner;
use dbsp_runner::typecheck::PlanOp;

const NESTED: &str = "\
a := input(\"a\")
a :: zset(record(v: i64))
out := distinct(filter(a, fun((r) -> r.v > 0)))
";

/// Nested calls become real nodes, but anonymous ones.
#[test]
fn nested_nodes_are_not_addressable_by_name() {
    let plan = compile(NESTED).expect("compiles");

    // input, the nested filter, and the named distinct.
    assert_eq!(plan.nodes.len(), 3, "the nested filter should be its own node");

    let mut names: Vec<&str> = plan.by_name.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, ["a", "out"], "only declared names are addressable");

    // Its synthesized name carries the operator and where it came from, so a
    // diagnostic about it is locatable.
    let nested = plan.nodes.iter().find(|n| n.name.starts_with("filter@")).expect("nested node");
    assert_eq!(nested.span.line, 3);
}

/// Lowering walks `plan.nodes` in order and indexes into already-built nodes,
/// so every operand must come before the node that uses it. Depth-first
/// resolution gives this, and breaking it would produce a panic far from the
/// cause.
#[test]
fn operands_always_precede_their_consumer() {
    let plan = compile(
        "a := input(\"a\")\n\
         a :: zset(record(v: i64))\n\
         out := plus(filter(a, fun((r) -> r.v > 0)), map(a, fun((r) -> r)))\n",
    )
    .expect("compiles");

    for (i, node) in plan.nodes.iter().enumerate() {
        for operand in operands(&node.op) {
            assert!(
                operand < i,
                "`{}` at index {i} uses operand {operand}, which is not yet built",
                node.name
            );
        }
    }
}

/// A nested node cannot be selected as an output, because it has no name.
#[test]
fn a_nested_node_cannot_be_an_output() {
    let plan = compile(NESTED).expect("compiles");
    let nested = plan.nodes.iter().find(|n| n.name.starts_with("filter@")).unwrap();

    // `Runner` is not `Debug`, so match rather than `expect_err`.
    match Runner::build(&plan, std::slice::from_ref(&nested.name)) {
        Ok(_) => panic!("an anonymous node should not be addressable as an output"),
        Err(diags) => assert!(
            diags.iter().any(|d| d.message.contains("no node named")),
            "unexpected: {diags:?}"
        ),
    }

    // The declared name works, so the rejection is about anonymity.
    Runner::build(&plan, &["out".to_string()]).expect("a declared node is addressable");
}

/// Nodes are content-addressed, so identical work is built once. This is
/// invisible in a program's output — `plus(x, x)` doubles weights whether `x`
/// is one node or two — so it can only be asserted on the plan itself.
#[test]
fn identical_work_becomes_one_node() {
    let plan = compile(
        "a := input(\"a\")\n\
         a :: zset(record(v: i64))\n\
         out := plus(filter(a, fun((r) -> r.v > 0)), filter(a, fun((r) -> r.v > 0)))\n",
    )
    .expect("compiles");

    let filters = plan.nodes.iter().filter(|n| n.name.starts_with("filter@")).count();
    assert_eq!(filters, 1, "the two identical filters should be one node");
    // input, filter, plus.
    assert_eq!(plan.nodes.len(), 3);
}

/// Two instantiations with the same arguments collapse entirely.
#[test]
fn identical_instantiations_share_their_nodes() {
    let plan = compile(
        "a := input(\"a\")\n\
         a :: zset(record(v: i64))\n\
         circuit c(src: s) { out := distinct(s) }\n\
         x := c(src: a)\n\
         y := c(src: a)\n",
    )
    .expect("compiles");

    assert_eq!(plan.nodes.len(), 2, "input and one distinct");
    assert_eq!(plan.by_name["x.out"], plan.by_name["y.out"]);
}

/// Two declarations of one table are one node, so the single input handle
/// `Runner` keeps is the one both names refer to.
#[test]
fn one_table_is_one_input_node() {
    let plan = compile(
        "a := input(\"t\")\n\
         a :: zset(record(v: i64))\n\
         b := input(\"t\")\n\
         b :: zset(record(v: i64))\n",
    )
    .expect("compiles");

    assert_eq!(plan.nodes.len(), 1);
    assert_eq!(plan.by_name["a"], plan.by_name["b"]);
}

/// Different work still gets different nodes — dedup must not over-merge.
#[test]
fn different_work_stays_separate() {
    let plan = compile(
        "a := input(\"a\")\n\
         a :: zset(record(v: i64))\n\
         p := filter(a, fun((r) -> r.v > 0))\n\
         q := filter(a, fun((r) -> r.v > 1))\n",
    )
    .expect("compiles");

    assert_ne!(plan.by_name["p"], plan.by_name["q"], "different predicates, different nodes");
    assert_eq!(plan.nodes.len(), 3);
}

fn operands(op: &PlanOp) -> Vec<usize> {
    match op {
        PlanOp::Input { .. } | PlanOp::Empty => vec![],
        PlanOp::Map { input, .. }
        | PlanOp::Filter { input, .. }
        | PlanOp::MapIndex { input, .. }
        | PlanOp::FlatMap { input, .. }
        | PlanOp::FlatMapIndex { input, .. }
        | PlanOp::Distinct { input }
        | PlanOp::Aggregate { input, .. }
        | PlanOp::WeightedCount { input }
        | PlanOp::Neg { input }
        | PlanOp::Integrate { input }
        | PlanOp::Differentiate { input }
        | PlanOp::Delay { input } => vec![*input],
        PlanOp::Join { left, right, .. }
        | PlanOp::JoinIndex { left, right, .. }
        | PlanOp::Antijoin { left, right }
        | PlanOp::Plus { left, right }
        | PlanOp::Minus { left, right } => vec![*left, *right],
        PlanOp::Sum { inputs } => inputs.clone(),
        // A fixpoint's body has its own index space, so only the export's
        // reference to the fixpoint node itself is an operand here.
        PlanOp::Fixpoint { .. } => vec![],
        PlanOp::FixpointExport { fixpoint, .. } => vec![*fixpoint],
        // Body-only, and never reached at the top level.
        PlanOp::Import { .. } | PlanOp::RecVar { .. } => vec![],
    }
}
