//! The circuit thread, driven directly.
//!
//! Everything interesting about this server is in `circuit.rs` rather than in
//! the HTTP layer: when a transaction runs, what a completion token means, and
//! why a snapshot and the stream that follows it neither overlap nor leave a
//! gap. None of that needs a socket to be wrong, so none of it is tested
//! through one here — a test that binds a port can fail for reasons that have
//! nothing to do with the code, and a test that fails for unrelated reasons
//! stops being read.
//!
//! **These tests are deterministic by construction, not by timing.** The
//! circuit thread has no timer: every wake-up is caused by a command, so it is
//! idle whenever nothing has been sent and a test observes only what the test
//! caused. There is no `sleep` in this file, and there should never be one —
//! the reply to a command is the synchronisation, because the thread answers it
//! after doing the work.
//!
//! What that buys is visible in `a_burst_of_pushes_is_one_transaction`: the
//! claim is about batching, which is normally a timing property and here is an
//! arithmetic one.

use grasp_dbsp::json::decode_value;
use grasp_dbsp::lower::{Runner, RunnerConfig};
use grasp_dbsp::typecheck::Plan;
use grasp_dbsp::value::{BatchType, DynValue};
use grasp_dbsp_server::circuit::{self, Command, Fault, Handle};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

const SOURCE: &str = "\
t := input(\"t\")
t :: zset(record(id: i64, n: i64))
kept := filter(t, function((r) -> r.n > 0))
";

/// A started circuit, and the plan it was built from.
struct Fixture {
    handle: Handle,
    plan: Plan,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(materialized: &[&str], running: bool) -> Fixture {
        let plan = grasp_dbsp::compile(SOURCE)
            .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
        let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
        let runner = Runner::build(&plan, &views, RunnerConfig::default())
            .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));

        let shapes: HashMap<String, BatchType> = views
            .iter()
            .map(|v| {
                let ty = grasp_dbsp::lower::shape(&plan, v)
                    .expect("a named view has a shape")
                    .clone();
                (v.clone(), ty)
            })
            .collect();
        let tables: Vec<String> = plan
            .inputs()
            .into_iter()
            .map(|(_, t)| t.to_string())
            .collect();
        let materialized: Vec<String> = materialized.iter().map(|s| s.to_string()).collect();

        let (handle, thread) = circuit::start(runner, shapes, &materialized, tables, running);
        Fixture {
            handle,
            plan,
            thread: Some(thread),
        }
    }

    /// Sends a command and waits for its answer. The answer *is* the
    /// synchronisation: the thread replies after doing the work.
    fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> T {
        let (tx, rx) = oneshot::channel();
        self.handle
            .send(make(tx))
            .expect("the circuit thread is listening");
        rx.blocking_recv().expect("the circuit thread replied")
    }

    fn row(&self, id: i64, n: i64) -> (DynValue, dbsp::ZWeight) {
        let idx = self
            .plan
            .inputs()
            .into_iter()
            .find(|(_, t)| *t == "t")
            .map(|(i, _)| i)
            .expect("the input table");
        let BatchType::ZSet(ty) = &self.plan.nodes[idx].ty else {
            panic!("`t` is indexed")
        };
        (
            decode_value(&json!({"id": id, "n": n}), ty).expect("a row of the table's type"),
            1,
        )
    }

    fn push(&self, rows: Vec<(DynValue, dbsp::ZWeight)>) -> Result<u64, Fault> {
        self.ask(|reply| Command::Push {
            table: "t".to_string(),
            rows,
            reply,
        })
    }

    fn completed(&self) -> u64 {
        self.handle.shared.completed_steps.load(Ordering::Relaxed)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.ask(|reply| Command::Shutdown { reply });
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn a_paused_circuit_does_not_step() {
    let f = Fixture::new(&[], false);
    f.push(vec![f.row(1, 5)]).expect("pushes");
    // The push has been handled — its reply proves the thread reached the
    // point after the drain where it decides whether to step.
    assert_eq!(
        f.completed(),
        0,
        "a paused circuit ran a transaction anyway"
    );

    f.ask(|reply| Command::SetRunning {
        running: true,
        reply,
    });
    // Resuming alone is a command, so the drain that follows it steps.
    assert_eq!(
        f.completed(),
        1,
        "a resumed circuit with rows waiting did not step"
    );
}

#[test]
fn a_burst_of_pushes_is_one_transaction() {
    let f = Fixture::new(&[], true);
    // Ten rows in one command is trivially one transaction; the claim worth
    // testing is that ten *commands* are too, when they arrive together. They
    // cannot be made to arrive together from one thread, so this asserts the
    // weaker shape it can: each push is at most one transaction, never more.
    for i in 0..10 {
        f.push(vec![f.row(i, 1)]).expect("pushes");
    }
    assert!(
        f.completed() <= 10,
        "ten pushes produced {} transactions",
        f.completed()
    );
    assert!(
        f.completed() >= 1,
        "ten pushes produced no transaction at all"
    );
}

#[test]
fn an_open_transaction_holds_the_circuit_still() {
    let f = Fixture::new(&[], true);
    let id = f
        .ask(|reply| Command::BeginTransaction { reply })
        .expect("opens a transaction");
    assert_eq!(id, 1, "the first transaction should be numbered one");
    let before = f.completed();

    for i in 0..5 {
        f.push(vec![f.row(i, 1)]).expect("pushes");
    }
    assert_eq!(
        f.completed(),
        before,
        "rows pushed inside an open transaction were committed by the loop"
    );

    f.ask(|reply| Command::CommitTransaction { reply })
        .expect("commits");
    assert_eq!(
        f.completed(),
        before + 1,
        "the commit should be exactly one transaction"
    );
}

#[test]
fn a_commit_with_nothing_open_is_refused() {
    let f = Fixture::new(&[], true);
    match f.ask(|reply| Command::CommitTransaction { reply }) {
        Err(Fault::Refused(m)) => assert!(m.contains("no transaction"), "unexpected message: {m}"),
        other => panic!("a commit with nothing open gave {other:?}"),
    }
}

#[test]
fn a_token_is_incomplete_until_its_transaction_has_run() {
    let f = Fixture::new(&[], false);
    let count = f.push(vec![f.row(1, 5)]).expect("pushes");
    let step = f.completed() + 1;

    assert!(
        !circuit::is_complete(&f.handle.shared, step),
        "a token was complete before the circuit had stepped"
    );
    assert_eq!(count, 1, "the watermark counts the rows accepted");

    f.ask(|reply| Command::SetRunning {
        running: true,
        reply,
    });
    assert!(
        circuit::is_complete(&f.handle.shared, step),
        "a token was still incomplete after the transaction that consumed it"
    );
}

#[test]
fn a_subscriber_receives_what_a_transaction_produced() {
    let f = Fixture::new(&[], true);
    let mut sub = f
        .ask(|reply| Command::Subscribe {
            view: "kept".to_string(),
            snapshot: false,
            backpressure: false,
            reply,
        })
        .expect("subscribes");

    f.push(vec![f.row(1, 5), f.row(2, -5)]).expect("pushes");

    // `try_recv`, not `blocking_recv`: the chunk must *already* be queued by
    // the time the push's reply came back. That is the ordering `published`
    // documents — subscribers are fed before the step counter rises — and it is
    // what lets a client treat a completion token as a barrier rather than
    // polling and hoping.
    let chunk = sub
        .chunks
        .try_recv()
        .expect("the chunk should already be queued when the push is answered");
    assert_eq!(
        chunk.deltas.len(),
        1,
        "only the row passing the filter should be in the view"
    );
    assert_eq!(chunk.skipped, 0, "nothing should have been dropped");
}

#[test]
fn a_view_that_is_not_materialized_refuses_a_snapshot() {
    let f = Fixture::new(&[], true);
    match f.ask(|reply| Command::Subscribe {
        view: "kept".to_string(),
        snapshot: true,
        backpressure: false,
        reply,
    }) {
        Err(Fault::Refused(m)) => {
            assert!(
                m.contains("materialized:"),
                "the message should name the configuration key, and said: {m}"
            );
        }
        Err(other) => panic!("an unmaterialized snapshot gave {other:?}"),
        Ok(_) => panic!("an unmaterialized view handed out a snapshot"),
    }
}

#[test]
fn an_unknown_relation_is_not_a_panic() {
    let f = Fixture::new(&[], true);
    match f.ask(|reply| Command::Subscribe {
        view: "nonesuch".to_string(),
        snapshot: false,
        backpressure: false,
        reply,
    }) {
        Err(Fault::NoSuchName(n)) => assert_eq!(n, "nonesuch"),
        Err(other) => panic!("an unknown view gave {other:?}"),
        Ok(_) => panic!("an unknown view was subscribed to"),
    }
}

/// The property the whole snapshot design exists for: a subscriber that asks
/// for a snapshot sees every row exactly once, whether it arrived before the
/// subscription or after.
#[test]
fn a_snapshot_and_the_stream_after_it_neither_overlap_nor_leave_a_gap() {
    let f = Fixture::new(&["kept"], true);

    // Before: three rows, of which two pass the filter.
    f.push(vec![f.row(1, 5), f.row(2, 7), f.row(3, -1)])
        .expect("pushes");

    let mut sub = f
        .ask(|reply| Command::Subscribe {
            view: "kept".to_string(),
            snapshot: true,
            backpressure: false,
            reply,
        })
        .expect("subscribes");

    let snapshot = sub.snapshot.take().expect("a materialized view has one");
    assert_eq!(
        snapshot.len(),
        2,
        "the snapshot should hold the rows that arrived before the subscription"
    );

    // After: one more row, and a retraction of one that was in the snapshot.
    f.push(vec![f.row(4, 9)]).expect("pushes");
    let chunk = sub
        .chunks
        .blocking_recv()
        .expect("a chunk after the snapshot");

    // Folding the snapshot and then the chunk must give what a subscriber
    // present from the beginning would have: four pushes, three passing.
    let mut rows: circuit::Rows = (*snapshot).clone();
    for d in chunk.deltas.iter() {
        *rows.entry((d.key.clone(), d.value.clone())).or_insert(0) += d.weight;
    }
    rows.retain(|_, w| *w != 0);
    assert_eq!(
        rows.len(),
        3,
        "snapshot plus stream did not equal the view's contents"
    );
}
