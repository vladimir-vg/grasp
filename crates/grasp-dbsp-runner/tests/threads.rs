//! A circuit built on one thread computes the same thing on another.
//!
//! Nothing in this crate has ever spawned a thread. Every test so far builds a
//! `Runner`, pushes, steps and reads it on the thread it was made on, so the
//! question this file asks has had no reason to come up — and a server cannot
//! avoid it. [`Runner::step`] takes `&mut self`, so one thread has to own the
//! circuit outright; the thread that compiled the program, read the
//! configuration and opened the storage backend is not that thread, because it
//! goes on to listen on a socket.
//!
//! `lower.rs` asserts that the type may cross a thread boundary. That assertion
//! is a compile-time fact about fields, and it would be satisfied by a type
//! that crossed and then misbehaved — `dbsp` hands out handles whose worker
//! threads were spawned by the *constructing* thread, and "the handles are
//! `Send`" is not the same claim as "the circuit still works once its owner has
//! moved". So this file moves one and makes it work.
//!
//! **The comparison is against the same program run in place**, not against a
//! hand-written expectation, for the reason `tests/workers.rs` gives about
//! worker counts: an expectation written by hand can be wrong in the same
//! direction as the code. Two runs that disagree are a finding whatever the
//! right answer is.
//!
//! **What is covered is what is written here.** One program with a stateful
//! operator, because state living across transactions is what a spawned worker
//! thread owns and therefore what a move could disturb; and a build that is
//! *sent* rather than made in place, because those are the two directions the
//! server actually uses.

use dbsp::ZWeight;
use grasp_dbsp_runner::json::decode_value;
use grasp_dbsp_runner::lower::{Delta, Runner, RunnerConfig};
use grasp_dbsp_runner::value::{BatchType, DynValue};
use serde_json::{Value as J, json};

/// One transaction: `(table, row, weight)`.
type Epoch = Vec<(&'static str, J, ZWeight)>;

/// A program with a join and a distinct, so the answer depends on state held
/// from one transaction to the next rather than on each transaction alone.
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

fn epochs() -> Vec<Epoch> {
    vec![
        vec![
            ("l", json!({"id": 1, "tag": "a"}), 1),
            ("l", json!({"id": 2, "tag": "b"}), 1),
            ("r", json!({"id": 1, "n": 10}), 1),
        ],
        // The second transaction joins against rows the first one left behind,
        // which is the state a moved circuit would have to have carried.
        vec![("r", json!({"id": 2, "n": 20}), 1)],
        // And a retraction, because a wrongly-carried state shows as a
        // cancellation that does not cancel.
        vec![("l", json!({"id": 1, "tag": "a"}), -1)],
    ]
}

/// Feeds every epoch and collects what each one produced.
///
/// Takes the `Runner` **by value**, which is what lets the caller decide the
/// thread it lives on — the whole point of the file. Everything else it needs
/// is owned too, so that a call can be moved into a closure whole.
fn drive(mut runner: Runner, epochs: &[Epoch], rows: &[(String, BatchType)]) -> Vec<Vec<Delta>> {
    let mut produced = Vec::new();
    for epoch in epochs {
        for (table, row, weight) in epoch {
            let ty = rows
                .iter()
                .find(|(t, _)| t == table)
                .map(|(_, ty)| ty)
                .unwrap_or_else(|| panic!("an input table `{table}`"));
            let BatchType::ZSet(ty) = ty else {
                panic!("`{table}` is indexed")
            };
            let value: DynValue = decode_value(row, ty).expect("a row of the table's type");
            runner.push(table, value, *weight).expect("pushes");
        }
        let mut step = runner.step().expect("steps");
        // One output, and this file is not the place that pins its name.
        produced.push(step.pop().map(|(_, d)| d).unwrap_or_default());
    }
    runner.kill();
    produced
}

/// Everything `drive` needs that is not the `Runner`, resolved before the move.
fn tables(plan: &grasp_dbsp_runner::typecheck::Plan) -> Vec<(String, BatchType)> {
    plan.inputs()
        .into_iter()
        .map(|(i, t)| (t.to_string(), plan.nodes[i].ty.clone()))
        .collect()
}

#[test]
fn a_circuit_computes_the_same_thing_on_a_thread_it_was_not_built_on() {
    let plan = grasp_dbsp_runner::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp_runner::diag::render(&d)));
    let outputs = vec!["out".to_string()];
    let epochs = epochs();

    let here = {
        let runner = Runner::build(&plan, &outputs, RunnerConfig::default())
            .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp_runner::diag::render(&d)));
        drive(runner, &epochs, &tables(&plan))
    };

    // Built here, driven there: the direction a server uses, since the program
    // has to compile and the configuration has to be read before there is
    // anything worth spawning a thread for.
    let there = {
        let runner = Runner::build(&plan, &outputs, RunnerConfig::default())
            .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp_runner::diag::render(&d)));
        let rows = tables(&plan);
        let epochs = epochs.clone();
        std::thread::spawn(move || drive(runner, &epochs, &rows))
            .join()
            .expect("the thread driving the circuit did not panic")
    };

    assert_eq!(
        here, there,
        "the same circuit gave different answers once it had changed threads"
    );
    assert!(
        here.iter().any(|d| !d.is_empty()),
        "neither run produced anything, so they agree about nothing"
    );
}
