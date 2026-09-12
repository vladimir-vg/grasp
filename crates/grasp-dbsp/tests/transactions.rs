//! A transaction may be held open across several calls, and it changes nothing.
//!
//! [`Runner::step`] is one whole transaction and is what every fixture uses.
//! `begin_transaction` / `advance` / `commit_transaction` are the same unit
//! taken apart, for the caller that cannot use the short form: a server, where
//! one transaction's rows arrive in several requests and the client decides
//! when it is finished.
//!
//! **The claim is that taking it apart changes nothing.** The same rows, pushed
//! in one `step` or spread across an open transaction, must produce the same
//! deltas — so every test here compares the two rather than checking the long
//! form against a written expectation, for the reason `tests/workers.rs` gives:
//! an expectation written by hand can be wrong in the same direction as the
//! code.
//!
//! **And that nothing leaks out early.** A transaction is atomic on the wire or
//! it is not a transaction. That guarantee is `dbsp`'s — `DBSPHandle::step`
//! returns `false` throughout the in-progress phase
//! (`dbsp/src/circuit/dbsp_handle.rs:1706-1711`) and the accumulating sinks
//! publish once per transaction (`dbsp/src/operator/accumulator.rs:21-31`) —
//! but a wrong call order here would break it, so it is checked rather than
//! cited.
//!
//! **Misuse is a diagnostic, not a panic.** `dbsp` does not track the lifecycle
//! it documents: a second `start_transaction`, or a commit with nothing open,
//! broadcasts a command and lets the workers interpret it. This crate's rule is
//! that no reachable path produces an internal error, so the state machine
//! lives in `Runner` and every illegal call is a `RunError` before a command is
//! sent. Those four calls are the last four tests.

use dbsp::ZWeight;
use grasp_dbsp::json::decode_value;
use grasp_dbsp::lower::{Delta, Runner, RunnerConfig};
use grasp_dbsp::typecheck::Plan;
use grasp_dbsp::value::{BatchType, DynValue};
use serde_json::{Value as J, json};

/// A join and a distinct, so the answer depends on state carried across
/// transactions rather than on each transaction alone.
const SOURCE: &str = "\
l := input(\"l\")
l :: zset(record(id: i64, tag: string))
r := input(\"r\")
r :: zset(record(id: i64, n: i64))

li := map_index(l, function((x) -> record(key: x.id, value: x)))
ri := map_index(r, function((x) -> record(key: x.id, value: x)))
j  := join(li, ri, function((k, a, b) -> record(tag: a.tag, n: b.n)))
out := distinct(j)
";

/// The rows of one transaction: `(table, row, weight)`.
type Rows = Vec<(&'static str, J, ZWeight)>;

fn program() -> (Plan, Vec<String>) {
    let plan = grasp_dbsp::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
    (plan, vec!["out".to_string()])
}

fn built(plan: &Plan, outputs: &[String]) -> Runner {
    Runner::build(plan, outputs, RunnerConfig::default())
        .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)))
}

fn push_all(runner: &Runner, plan: &Plan, rows: &Rows) {
    for (table, row, weight) in rows {
        let idx = plan
            .inputs()
            .into_iter()
            .find(|(_, t)| t == table)
            .map(|(i, _)| i)
            .unwrap_or_else(|| panic!("an input table `{table}`"));
        let BatchType::ZSet(ty) = &plan.nodes[idx].ty else {
            panic!("`{table}` is indexed")
        };
        let value: DynValue = decode_value(row, ty).expect("a row of the table's type");
        runner.push(table, value, *weight).expect("pushes");
    }
}

/// Only the deltas, since one output makes the name noise.
fn deltas(step: Vec<(String, Vec<Delta>)>) -> Vec<Delta> {
    step.into_iter().next().map(|(_, d)| d).unwrap_or_default()
}

/// The transactions this file runs, chosen so the second depends on what the
/// first left behind and the third retracts part of it.
fn transactions() -> Vec<Rows> {
    vec![
        vec![
            ("l", json!({"id": 1, "tag": "a"}), 1),
            ("l", json!({"id": 2, "tag": "b"}), 1),
            ("r", json!({"id": 1, "n": 10}), 1),
        ],
        vec![("r", json!({"id": 2, "n": 20}), 1)],
        vec![("l", json!({"id": 1, "tag": "a"}), -1)],
    ]
}

#[test]
fn an_open_transaction_gives_what_one_step_would_have_given() {
    let (plan, outputs) = program();
    let txns = transactions();

    let short = {
        let mut runner = built(&plan, &outputs);
        let out: Vec<Vec<Delta>> = txns
            .iter()
            .map(|rows| {
                push_all(&runner, &plan, rows);
                deltas(runner.step().expect("steps"))
            })
            .collect();
        runner.kill();
        out
    };

    let long = {
        let mut runner = built(&plan, &outputs);
        let out: Vec<Vec<Delta>> = txns
            .iter()
            .map(|rows| {
                runner.begin_transaction().expect("opens a transaction");
                // Pushed one row at a time with a step between, which is the
                // shape a server produces: several requests, one transaction.
                for row in rows {
                    push_all(&runner, &plan, &vec![row.clone()]);
                    assert!(
                        !runner.advance().expect("advances"),
                        "a transaction reported itself committed before it was asked to commit"
                    );
                }
                deltas(runner.commit_transaction().expect("commits"))
            })
            .collect();
        runner.kill();
        out
    };

    assert_eq!(
        short, long,
        "the same rows gave different answers when their transaction was held open"
    );
    assert!(
        short.iter().any(|d| !d.is_empty()),
        "neither form produced anything, so they agree about nothing"
    );
}

#[test]
fn an_open_transaction_produces_no_output_until_it_commits() {
    let (plan, outputs) = program();
    let mut runner = built(&plan, &outputs);

    runner.begin_transaction().expect("opens a transaction");
    push_all(&runner, &plan, &transactions()[0]);

    // Ten advances, each of which would publish if a transaction were not
    // atomic. `advance` answering `false` is the whole guarantee.
    for _ in 0..10 {
        assert!(
            !runner.advance().expect("advances"),
            "an in-progress transaction reported a completed commit"
        );
    }

    let out = deltas(runner.commit_transaction().expect("commits"));
    assert!(
        !out.is_empty(),
        "the commit produced nothing, so the advances had nothing to hold back"
    );
    runner.kill();
}

#[test]
fn a_transaction_does_not_nest() {
    let (plan, outputs) = program();
    let mut runner = built(&plan, &outputs);
    runner.begin_transaction().expect("opens a transaction");

    let Err(e) = runner.begin_transaction() else {
        panic!("a second `begin_transaction` was allowed");
    };
    assert!(
        e.to_string().contains("do not nest"),
        "the message should say transactions do not nest, and said: {e}"
    );
    runner.kill();
}

#[test]
fn a_step_refuses_to_run_inside_an_open_transaction() {
    let (plan, outputs) = program();
    let mut runner = built(&plan, &outputs);
    runner.begin_transaction().expect("opens a transaction");

    let Err(e) = runner.step() else {
        panic!("`step` ran a transaction of its own inside an open one");
    };
    assert!(
        e.to_string().contains("commit_transaction"),
        "the message should name the way out, and said: {e}"
    );
    runner.kill();
}

#[test]
fn a_commit_with_nothing_open_is_a_diagnostic() {
    let (plan, outputs) = program();
    let mut runner = built(&plan, &outputs);

    let Err(e) = runner.commit_transaction() else {
        panic!("a commit with no transaction open was allowed");
    };
    assert!(
        e.to_string().contains("begin_transaction"),
        "the message should name what opens one, and said: {e}"
    );
    runner.kill();
}

#[test]
fn an_advance_with_nothing_open_is_a_diagnostic() {
    let (plan, outputs) = program();
    let mut runner = built(&plan, &outputs);

    let Err(e) = runner.advance() else {
        panic!("an advance with no transaction open was allowed");
    };
    assert!(
        e.to_string().contains("begin_transaction"),
        "the message should name what opens one, and said: {e}"
    );
    runner.kill();
}

#[test]
fn a_committed_transaction_leaves_the_runner_ready_for_the_next() {
    let (plan, outputs) = program();
    let mut runner = built(&plan, &outputs);

    runner.begin_transaction().expect("opens a transaction");
    push_all(&runner, &plan, &transactions()[0]);
    runner.commit_transaction().expect("commits");
    assert!(
        !runner.in_transaction(),
        "the runner still thought a transaction was open after committing it"
    );

    // The short form has to work again afterwards, or the two are not
    // interchangeable and a server could not mix them.
    push_all(&runner, &plan, &transactions()[1]);
    runner.step().expect("steps after a committed transaction");
    runner.kill();
}
