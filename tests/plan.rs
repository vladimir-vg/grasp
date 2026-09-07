//! Structural properties of a checked `Plan` that the fixtures cannot express.

use dbsp_runner::compile;
use dbsp_runner::lower::Runner;
use dbsp_runner::typecheck::PlanOp;

const NESTED: &str = "\
a := input(\"a\")
a :: zset(record(v: i64))
out := distinct(filter(a, function((r) -> r.v > 0)))
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
         out := plus(filter(a, function((r) -> r.v > 0)), map(a, function((r) -> r)))\n",
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
         out := plus(filter(a, function((r) -> r.v > 0)), filter(a, function((r) -> r.v > 0)))\n",
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
         p := filter(a, function((r) -> r.v > 0))\n\
         q := filter(a, function((r) -> r.v > 1))\n",
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

/// A numeric literal takes its type from the operand beside it, so one function
/// body serves every numeric type. The literal must be *pinned* into the plan,
/// not merely accepted by the checker: the evaluator promotes mixed operands, so
/// a body left holding an `i64` two would still compute the right answer at
/// `f64` and only diverge once the vocabulary grows past these two types.
#[test]
fn a_numeric_literal_is_pinned_to_its_context() {
    use dbsp_runner::expr::TypedExpr;
    use dbsp_runner::value::DynValue;

    let consts = |src: &str| -> Vec<DynValue> {
        let plan = compile(src).expect("compiles");
        let mut found = Vec::new();
        fn walk(e: &TypedExpr, out: &mut Vec<DynValue>) {
            match e {
                TypedExpr::Const(v) => out.push(v.clone()),
                TypedExpr::IntLit(_) | TypedExpr::FloatLit(_) => {
                    panic!("a literal reached the plan without taking a type")
                }
                TypedExpr::Unary(_, i) => walk(i, out),
                TypedExpr::Binary(_, l, r) => {
                    walk(l, out);
                    walk(r, out);
                }
                TypedExpr::Call(_, args)
                | TypedExpr::Record(args)
                | TypedExpr::Array(args) => args.iter().for_each(|a| walk(a, out)),
                TypedExpr::Field(b, _) | TypedExpr::Cast(b, _) => walk(b, out),
                TypedExpr::Var(_) => {}
            }
        }
        for node in &plan.nodes {
            if let PlanOp::Map { f, .. } = &node.op {
                walk(f, &mut found);
            }
        }
        found
    };

    let body = |ty: &str| {
        format!(
            "a := input(\"a\")\n\
             a :: zset(record(v: {ty}))\n\
             out := map(a, function((r) -> r.v * 2))\n"
        )
    };

    assert_eq!(consts(&body("i64")), vec![DynValue::I64(2)], "`2` beside an i64");
    assert_eq!(
        consts(&body("f64")),
        vec![DynValue::F64(dbsp::algebra::F64::new(2.0))],
        "the same `2` beside an f64"
    );
}

/// Two programs describing the same dataflow must produce the same content ids,
/// however they were written.
///
/// This is what makes the id usable as a `persistent_id`. An emitting agent
/// regenerates whole programs rather than editing them, so whitespace,
/// declaration order and the names of intermediates are all unstable across
/// emissions while the computation is not. `dbsp` says as much about the ids it
/// wants: derive them "from the program … rather than from anything positional".
#[test]
fn content_ids_survive_regeneration() {
    use dbsp_runner::typecheck::content_ids;
    use std::collections::BTreeSet;

    let a = "\
a := input(\"emp\")
a :: zset(record(id: i64, dept: i64))
big := filter(a, function((r) -> r.dept > 3))
idx := map_index(big, function((r) -> record(key: r.dept, value: r)))
out := aggregate(idx, count, function((v) -> v.id))
";

    // Same dataflow: declarations reordered, intermediates renamed, the filter
    // written inline instead of bound, and the whitespace changed throughout.
    let b = "\
out   := aggregate(grouped, count, function((v) -> v.id))\n\
\n\
grouped := map_index(\n\
    filter(src, function((r) -> r.dept > 3)),\n\
    function((r) -> record(key: r.dept, value: r)))\n\
\n\
src :: zset(record(id: i64, dept: i64))\n\
src := input(\"emp\")\n\
";

    let ids = |src: &str| -> BTreeSet<String> {
        content_ids(&compile(src).expect("compiles")).into_iter().collect()
    };
    assert_eq!(ids(a), ids(b), "the same dataflow, written two ways");

    // And a genuinely different computation must not collide.
    let c = a.replace("r.dept > 3", "r.dept > 4");
    assert_ne!(ids(a), ids(&c), "a different filter is a different node");
}

/// A node with no name can still be observed, because its content id is a name.
///
/// Outputs are chosen when the runner starts and anything may be chosen, so a
/// nested node being unaddressable was a hole in that. Its only label is
/// `filter@3:12` — a source position, which is exactly what must not be used.
#[test]
fn a_nested_node_is_observable_by_content_id() {
    use dbsp_runner::typecheck::content_ids;

    let plan = compile(NESTED).expect("compiles");
    let ids = content_ids(&plan);
    let nested = plan.nodes.iter().position(|n| n.name.starts_with("filter@")).unwrap();

    let runner = Runner::build(&plan, &[ids[nested].clone()]).expect("builds");
    runner.kill();
}
