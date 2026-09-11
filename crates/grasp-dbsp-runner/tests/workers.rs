//! The worker count does not change the answer.
//!
//! `Runner` takes a [`RunnerConfig`] and `dbsp` shards batches across the
//! workers it names, by `key.default_hash() % workers`. Everything that follows
//! from that is invisible at one worker, so this file runs the same program at
//! one and at three and requires the two to agree — delta for delta, in order.
//!
//! **Three, not two or four.** `dbsp`'s own reason, from the test beside its
//! upsert handle (`dbsp/src/operator/dynamic/input.rs:2175-2181`): placement
//! functions that differ only in the high bits of a hash still agree modulo a
//! power of two. Three also leaves a worker empty on the smaller programs,
//! which is the path through `consolidate` that merges an empty mailbox.
//!
//! **Every program retracts**, and that is not decoration. `OutputHandle::
//! consolidate` merges every worker's batch whatever the placement, so a
//! program that only inserts gives the right answer even when a key's rows are
//! scattered. The failure mode the invariants name is *retractions stop
//! cancelling*, so only a retraction can show it.
//!
//! **What is covered is what is written here.** The fixture corpus runs at one
//! worker; these programs are the whole of the multi-worker evidence, so they
//! are chosen by operator family — join, antijoin, distinct, aggregate,
//! weighted_count, fixpoint — and by the key types whose `Eq`/`Hash` agreement
//! `mapping.md` asserts rather than constructs. An operator family added later
//! and not added here has none.

use dbsp::ZWeight;
use grasp_dbsp_runner::json::decode_value;
use grasp_dbsp_runner::lower::{Delta, Runner, RunnerConfig};
use grasp_dbsp_runner::value::BatchType;
use serde_json::{Value as J, json};
use std::num::NonZeroUsize;

/// One transaction: `(table, row, weight)`.
type Epoch = Vec<(&'static str, J, ZWeight)>;

/// What every epoch produced, in order, at `workers` workers.
///
/// The result is compared as a `Vec` rather than a set, which also pins that
/// `step` is *ordered* at any count: `consolidate` merges the workers into one
/// key-ordered batch, and the outputs come back in build order. The fixture
/// harness sorts both sides, so this is the only place that says so.
fn run(
    source: &str,
    outputs: &[&str],
    epochs: &[Epoch],
    workers: usize,
) -> Vec<Vec<(String, Vec<Delta>)>> {
    let plan = grasp_dbsp_runner::compile(source)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp_runner::diag::render(&d)));
    let outs: Vec<String> = outputs.iter().map(|s| (*s).to_string()).collect();
    let config = RunnerConfig {
        workers: NonZeroUsize::new(workers).expect("a worker count"),
        ..RunnerConfig::default()
    };
    let mut runner = Runner::build(&plan, &outs, config)
        .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp_runner::diag::render(&d)));

    // `dbsp` does not check that the workers built the same circuit; this is
    // where we ask it to. Only worth asking where there is more than one.
    if workers > 1 {
        runner
            .recheck_determinism()
            .expect("the circuit constructor is deterministic");
    }

    let mut produced = Vec::new();
    for epoch in epochs {
        for (table, row, weight) in epoch {
            let idx = plan
                .inputs()
                .into_iter()
                .find(|(_, t)| t == table)
                .map(|(i, _)| i)
                .unwrap_or_else(|| panic!("an input table `{table}`"));
            let BatchType::ZSet(ty) = &plan.nodes[idx].ty else {
                panic!("`{table}` is indexed");
            };
            let value = decode_value(row, ty).expect("a row of the table's type");
            runner.push(table, value, *weight).expect("pushes");
        }
        produced.push(runner.step().expect("steps"));
    }
    runner.kill();
    produced
}

/// The claim, for one program.
fn agrees(what: &str, source: &str, outputs: &[&str], epochs: &[Epoch]) {
    let one = run(source, outputs, epochs, 1);
    let three = run(source, outputs, epochs, 3);
    assert_eq!(one, three, "{what}: one worker and three disagree");
    assert!(
        one.iter()
            .any(|epoch| epoch.iter().any(|(_, d)| !d.is_empty())),
        "{what}: neither count produced anything, so they agree about nothing"
    );
}

/// How many keys a program spreads over. Enough that three workers all get
/// some: the chance one is empty is under a millionth.
const KEYS: i64 = 40;

#[test]
fn join_antijoin_and_distinct_agree() {
    let source = "\
l := input(\"l\")
l :: zset(record(k: i64, a: i64))
r := input(\"r\")
r :: zset(record(k: i64, b: i64))
x := input(\"x\")
x :: zset(record(k: i64))

li := map_index(l, function((v) -> record(key: v.k, value: record(a: v.a))))
ri := map_index(r, function((v) -> record(key: v.k, value: record(b: v.b))))
xi := map_index(x, function((v) -> record(key: v.k, value: record(k: v.k))))

joined := join(li, ri, function((k, p, q) -> record(k: k, total: (p.a + q.b))))
kept   := antijoin(map_index(joined, function((v) -> record(key: v.k, value: v))), xi)
uniq   := distinct(map(kept, function((k, v) -> record(bucket: (k % 5)))))
";
    let mut first: Epoch = Vec::new();
    for k in 0..KEYS {
        first.push(("l", json!({"k": k, "a": k * 10}), 1));
        first.push(("r", json!({"k": k, "b": k + 1}), 1));
    }
    for k in (0..KEYS).step_by(7) {
        first.push(("x", json!({"k": k}), 1));
    }
    // Retract every row, and leave two new keys behind.
    let mut second: Epoch = first
        .iter()
        .map(|(t, row, _)| (*t, row.clone(), -1))
        .collect();
    second.push(("l", json!({"k": 100, "a": 1}), 1));
    second.push(("r", json!({"k": 100, "b": 2}), 1));

    agrees(
        "join, antijoin and distinct",
        source,
        &["joined", "kept", "uniq"],
        &[first, second],
    );
}

#[test]
fn keys_that_are_not_scalars_agree() {
    // A record, an `optional(f64)` carrying both `0.0` and `-0.0`, and a dict:
    // the three places `mapping.md` says `Eq`/`Hash` agreement is asserted
    // rather than constructed. Each is a shard key here.
    let source = "\
t := input(\"t\")
t :: zset(record(id: i64, f: optional(f64), tag: string))

byrec   := map_index(t, function((v) -> record(key: record(tag: v.tag, bucket: (v.id % 7)), value: record(id: v.id))))
highest := aggregate(byrec, max, function((v) -> v.id))
byfloat := weighted_count(map(t, function((v) -> v.f)))
bydict  := distinct(map(t, function((v) -> {\"tag\" => v.tag})))
";
    let float = |id: i64| -> J {
        match id % 4 {
            0 => json!(0.0),
            1 => json!(-0.0),
            2 => json!(null),
            _ => json!(id as f64),
        }
    };
    let first: Epoch = (0..KEYS)
        .map(|id| {
            (
                "t",
                json!({"id": id, "f": float(id), "tag": format!("t{}", id % 6)}),
                1,
            )
        })
        .collect();
    let second: Epoch = first
        .iter()
        .map(|(t, row, _)| (*t, row.clone(), -1))
        .collect();

    agrees(
        "record, optional(f64) and dict keys",
        source,
        &["highest", "byfloat", "bydict"],
        &[first, second],
    );
}

#[test]
fn aggregates_agree() {
    let source = "\
t := input(\"t\")
t :: zset(record(g: i64, id: i64, v: i64))
idx     := map_index(t, function((r) -> record(key: r.g, value: r)))
total   := aggregate(idx, sum, function((v) -> v.v))
lowest  := aggregate(idx, min, function((v) -> v.v))
highest := aggregate(idx, max, function((v) -> v.v))
rows    := weighted_count(map(t, function((r) -> r.g)))
";
    let mut first: Epoch = Vec::new();
    for g in 0..KEYS {
        for i in 0..3 {
            first.push(("t", json!({"g": g, "id": i, "v": g * 10 + i}), 1));
        }
    }
    // Retract one row of every group, so each aggregate is recomputed rather
    // than merely rebuilt from nothing.
    let second: Epoch = first
        .iter()
        .filter(|(_, row, _)| row["id"] == json!(1))
        .map(|(t, row, _)| (*t, row.clone(), -1))
        .collect();

    agrees(
        "sum, min, max and weighted_count",
        source,
        &["total", "lowest", "highest", "rows"],
        &[first, second],
    );
}

#[test]
fn a_fixpoint_agrees() {
    let source = "\
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
    // A chain 0 -> 1 -> … -> 39, so the closure is quadratic in the chain and
    // the implicit `distinct` inside the nested circuit runs at many keys.
    let first: Epoch = (0..KEYS - 1)
        .map(|n| ("edges", json!({"src": n, "dst": n + 1}), 1))
        .collect();
    // Cutting the middle edge shrinks the closure, so the retraction has to
    // propagate through the recursion.
    let second: Epoch = vec![("edges", json!({"src": KEYS / 2, "dst": KEYS / 2 + 1}), -1)];

    agrees("a fixpoint", source, &["closure"], &[first, second]);
}

/// Floating-point `sum` folds a group in cursor order, and sharding does not
/// change which rows are in the group.
///
/// `aggregate` shards by key, so every row of one group lands on one worker and
/// the deterministic `Fold` replays that one cursor in `Ord` order of the value
/// — not in arrival order, and not in worker order. This is the one program
/// here that does not retract: what it is about is the order of the addition.
///
/// The rows are chosen so the order is visible. `1e-16` is below half the ulp
/// of `1.0`, so `1.0 + 1e-16 + 1e-16` is `1.0`, while `1e-16 + 1e-16 + 1.0` is
/// `1.0000000000000002`. The value sorts the tiny pair first, so the answer is
/// the second — from either feed order, at either worker count.
#[test]
fn a_float_sum_agrees_whatever_the_order() {
    let source = "\
t := input(\"t\")
t :: zset(record(g: i64, id: i64, x: f64))
idx   := map_index(t, function((r) -> record(key: r.g, value: r)))
total := aggregate(idx, sum, function((v) -> v.x))
";
    let mut forward: Epoch = Vec::new();
    for g in 0..30 {
        forward.push(("t", json!({"g": g, "id": 0, "x": 1e-16}), 1));
        forward.push(("t", json!({"g": g, "id": 1, "x": 1e-16}), 1));
        forward.push(("t", json!({"g": g, "id": 2, "x": 1.0}), 1));
    }
    let backward: Epoch = forward.iter().rev().cloned().collect();

    agrees(
        "a float sum",
        source,
        &["total"],
        std::slice::from_ref(&forward),
    );
    agrees(
        "a float sum, fed backwards",
        source,
        &["total"],
        std::slice::from_ref(&backward),
    );

    // And the answer is the one cursor order gives, not the one arrival order
    // would have.
    let want = grasp_dbsp_runner::value::DynValue::F64(dbsp::algebra::F64::new(1.0000000000000002));
    for epoch in [forward, backward] {
        for workers in [1, 3] {
            for (_, deltas) in run(source, &["total"], std::slice::from_ref(&epoch), workers)
                .into_iter()
                .flatten()
            {
                for delta in deltas {
                    assert_eq!(delta.value, Some(want.clone()), "at {workers} worker(s)");
                }
            }
        }
    }
}
