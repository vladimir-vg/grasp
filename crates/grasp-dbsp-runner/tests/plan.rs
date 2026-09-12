//! Structural properties of a checked `Plan` that the fixtures cannot express.

use grasp_dbsp_runner::compile;
use grasp_dbsp_runner::lower::{Runner, RunnerConfig};
use grasp_dbsp_runner::typecheck::PlanOp;

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
    assert_eq!(
        plan.nodes.len(),
        3,
        "the nested filter should be its own node"
    );

    let mut names: Vec<&str> = plan.by_name.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, ["a", "out"], "only declared names are addressable");

    // Its synthesized name carries the operator and where it came from, so a
    // diagnostic about it is locatable.
    let nested = plan
        .nodes
        .iter()
        .find(|n| n.name.starts_with("filter@"))
        .expect("nested node");
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
    let nested = plan
        .nodes
        .iter()
        .find(|n| n.name.starts_with("filter@"))
        .unwrap();

    // `Runner` is not `Debug`, so match rather than `expect_err`.
    match Runner::build(
        &plan,
        std::slice::from_ref(&nested.name),
        RunnerConfig::default(),
    ) {
        Ok(_) => panic!("an anonymous node should not be addressable as an output"),
        Err(diags) => assert!(
            diags.iter().any(|d| d.message.contains("no node named")),
            "unexpected: {diags:?}"
        ),
    }

    // The declared name works, so the rejection is about anonymity.
    Runner::build(&plan, &["out".to_string()], RunnerConfig::default())
        .expect("a declared node is addressable");
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

    let filters = plan
        .nodes
        .iter()
        .filter(|n| n.name.starts_with("filter@"))
        .count();
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

    assert_ne!(
        plan.by_name["p"], plan.by_name["q"],
        "different predicates, different nodes"
    );
    assert_eq!(plan.nodes.len(), 3);
}

fn operands(op: &PlanOp) -> Vec<usize> {
    match op {
        PlanOp::Input { .. } | PlanOp::Empty | PlanOp::Constant { .. } => vec![],
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
    use grasp_dbsp_runner::expr::TypedExpr;
    use grasp_dbsp_runner::value::DynValue;

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
                TypedExpr::Call(_, args) | TypedExpr::Record(args) | TypedExpr::Array(args) => {
                    args.iter().for_each(|a| walk(a, out))
                }
                TypedExpr::Dict(entries) => entries.iter().for_each(|(k, v)| {
                    walk(k, out);
                    walk(v, out);
                }),
                TypedExpr::Field(b, _) | TypedExpr::Cast(b, _) | TypedExpr::DictFrom(b) => {
                    walk(b, out)
                }
                TypedExpr::MapArray { array, body } | TypedExpr::FilterArray { array, body } => {
                    walk(array, out);
                    walk(body, out);
                }
                TypedExpr::Var(_) | TypedExpr::Elem(_) => {}
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

    assert_eq!(
        consts(&body("i64")),
        vec![DynValue::I64(2)],
        "`2` beside an i64"
    );
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
    use grasp_dbsp_runner::typecheck::content_ids;
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
        content_ids(&compile(src).expect("compiles"))
            .into_iter()
            .collect()
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
    use grasp_dbsp_runner::typecheck::content_ids;

    let plan = compile(NESTED).expect("compiles");
    let ids = content_ids(&plan);
    let nested = plan
        .nodes
        .iter()
        .position(|n| n.name.starts_with("filter@"))
        .unwrap();

    let runner =
        Runner::build(&plan, &[ids[nested].clone()], RunnerConfig::default()).expect("builds");
    runner.kill();
}

/// A constant relation's identity is its rows, and a relation is a set: two
/// constants holding the same rows are one node however they were written.
///
/// None of this is visible from a program's output — both spellings compute the
/// same thing either way. It matters because the content id is the
/// `persistent_id`, so two nodes that ought to be one would checkpoint as two.
#[test]
fn constants_are_identified_by_their_rows() {
    let one = |src: &str| {
        compile(src)
            .expect("compiles")
            .nodes
            .iter()
            .filter(|n| matches!(n.op, PlanOp::Constant { .. }))
            .count()
    };

    // Written in a different order: one relation, so one node.
    assert_eq!(
        one("a :: zset(record(v: i64))\n\
             a := constant([record(v: 1), record(v: 2)])\n\
             b :: zset(record(v: i64))\n\
             b := constant([record(v: 2), record(v: 1)])\n\
             out := plus(a, b)\n"),
        1,
        "row order is not part of a relation, so it must not be part of the node"
    );

    // Folded to the same values: also one node, which is why the rows are
    // evaluated before they are hashed.
    assert_eq!(
        one("a :: zset(record(v: i64))\n\
             a := constant([record(v: 1 + 1)])\n\
             b :: zset(record(v: i64))\n\
             b := constant([record(v: 2)])\n\
             out := plus(a, b)\n"),
        1,
        "`1 + 1` and `2` are the same row, so they are the same node"
    );

    // Different rows are different relations.
    assert_eq!(
        one("a :: zset(record(v: i64))\n\
             a := constant([record(v: 1)])\n\
             b :: zset(record(v: i64))\n\
             b := constant([record(v: 2)])\n\
             out := plus(a, b)\n"),
        2,
        "distinct rows must not collapse"
    );

    // Same row values, different types: the node's type is hashed too, which
    // matters because a record value is positional and carries no field names.
    assert_eq!(
        one("a :: zset(record(v: i64))\n\
             a := constant([record(v: 1)])\n\
             b :: zset(record(w: i64))\n\
             b := constant([record(w: 1)])\n\
             out := plus(a, map(b, function((r) -> record(v: r.w))))\n"),
        2,
        "the row values are identical; only the declared type tells them apart"
    );
}

const RECURSIVE: &str = "\
edges := input(\"edges\")
edges :: zset(record(src: i64, dst: i64))
base  := map_index(edges, function((r) -> record(key: r.dst, value: record(src: r.src))))
fwd   := map_index(edges, function((r) -> record(key: r.src, value: record(dst: r.dst))))

circuit tc(base: b, fwd: f, path: p) {
    step := join_index(p, f, function((k, a, e) -> record(key: e.dst, value: record(src: a.src))))
    path := plus(b, step)
}

fp      := fixpoint(tc(base: base, fwd: fwd, path: empty()))
closure := fp.path
";

/// `Plan::views` is the list a server exposes, so what it leaves out matters as
/// much as what it has: every name it offers must actually build.
#[test]
fn every_view_a_plan_offers_can_be_an_output() {
    for source in [NESTED, RECURSIVE] {
        let plan = compile(source).expect("compiles");
        let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
        assert!(!views.is_empty(), "a program with names offered none");

        // All of them at once, which is what a server does at startup.
        match Runner::build(&plan, &views, RunnerConfig::default()) {
            Ok(runner) => runner.kill(),
            Err(diags) => panic!("a name `views` offered was refused: {diags:?}"),
        }
    }
}

/// A `fixpoint` instance is the name that would break `views` if it were a key
/// in `by_name`: it is not a stream, and `Runner::build` refuses it. It is not
/// a key — the checker registers only the members, under `<instance>.<label>` —
/// and this is what keeps that true, since `views` has no filter for it.
#[test]
fn a_fixpoint_instance_is_not_a_view() {
    let plan = compile(RECURSIVE).expect("compiles");

    assert!(
        !plan.views().contains(&"fp"),
        "the fixpoint instance `fp` was offered as a view, which `views` does \
         not filter out — it relies on the checker never registering one"
    );
    // Both spellings of the stream taken out of it are, and both build: the
    // dotted member path, and the name the program bound it to.
    for name in ["fp.path", "closure"] {
        assert!(
            plan.views().contains(&name),
            "`{name}` should be a view of a recursive program"
        );
    }

    // The refusal it would have hit is reachable by content id, which is the
    // only way to name a fixpoint node at all.
    let ids = grasp_dbsp_runner::typecheck::content_ids(&plan);
    let fixpoint = plan
        .nodes
        .iter()
        .position(|n| matches!(n.op, PlanOp::Fixpoint { .. }))
        .expect("a fixpoint node");
    match Runner::build(
        &plan,
        std::slice::from_ref(&ids[fixpoint]),
        RunnerConfig::default(),
    ) {
        Ok(_) => panic!("a fixpoint should not build as an output"),
        Err(diags) => assert!(
            diags.iter().any(|d| d.message.contains("not a stream")),
            "unexpected: {diags:?}"
        ),
    }
}

/// Sorted, because a server's view list and the output selection built from it
/// would otherwise differ run to run with `HashMap` iteration order.
#[test]
fn the_views_are_in_the_programs_alphabet() {
    let plan = compile(RECURSIVE).expect("compiles");
    let views = plan.views();
    let mut sorted = views.clone();
    sorted.sort_unstable();
    assert_eq!(views, sorted, "`views` is not sorted");
}
