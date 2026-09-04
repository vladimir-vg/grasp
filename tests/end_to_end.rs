//! Source text in, JSON deltas out.
//!
//! The program under test is the worked example from `docs/design/language.md`,
//! so these tests are also a check that the documented example is real.

use dbsp_runner::json::{decode_delta, encode_delta};
use dbsp_runner::lang::parse;
use dbsp_runner::lower::Runner;
use dbsp_runner::typecheck::check;
use dbsp_runner::value::{BatchType, TypeDesc};
use serde_json::{json, Value as J};

const PROGRAM: &str = r#"
# The worked example from docs/design/language.md.
emp := input("emp")
emp :: OrdZSet(record(id: i64, name: sql.SqlString,
                      dept_id: i64, salary: i64))

dept := input("dept")
dept :: OrdZSet(record(id: i64, dname: sql.SqlString))

high_paid := filter(emp, fun((row) -> row.salary > 100000))

emp_idx  := map_index(emp,  fun((row) ->
                (row.dept_id, record(id: row.id, name: row.name, salary: row.salary))))
dept_idx := map_index(dept, fun((row) -> (row.id, record(dname: row.dname))))

joined   := join(emp_idx, dept_idx, fun((k, e, d) -> record(name: e.name, dname: d.dname)))

by_dept  := aggregate(emp_idx, max, fun((v) -> v.salary))
"#;

struct Harness {
    runner: Runner,
    plan: dbsp_runner::typecheck::Plan,
}

impl Harness {
    fn new(src: &str, outputs: &[&str]) -> Harness {
        let program = parse(src).expect("parse");
        let plan = check(&program).expect("typecheck");
        let names: Vec<String> = outputs.iter().map(|s| s.to_string()).collect();
        let runner = Runner::build(&plan, &names).expect("build circuit");
        Harness { runner, plan }
    }

    fn row_type(&self, node: &str) -> TypeDesc {
        match &self.plan.node(node).expect("node").ty {
            BatchType::ZSet(t) => t.clone(),
            BatchType::IndexedZSet(..) => panic!("{node} is indexed"),
        }
    }

    /// Push a delta in the `weighted` JSON format the runner accepts.
    fn push_json(&self, table: &str, delta: J) {
        let ty = self.row_type(table);
        let (row, weight) = decode_delta(&delta, &ty).expect("decode");
        self.runner.push(table, row, weight).expect("push");
    }

    /// Run a transaction and return each output's deltas, re-encoded as JSON.
    fn step(&mut self) -> Vec<(String, Vec<J>)> {
        let out = self.runner.step().expect("step");
        out.into_iter()
            .map(|(name, deltas)| {
                let ty = self.plan.node(&name).unwrap().ty.clone();
                let encoded = deltas
                    .iter()
                    .map(|d| encode_delta(d, &ty).expect("encode"))
                    .collect();
                (name, encoded)
            })
            .collect()
    }

    fn take(&mut self, want: &str) -> Vec<J> {
        let mut all = self.step();
        let pos = all.iter().position(|(n, _)| n == want).expect("output not selected");
        let mut deltas = all.remove(pos).1;
        deltas.sort_by_key(|v| v.to_string());
        deltas
    }
}

fn emp(id: i64, name: &str, dept: i64, salary: i64) -> J {
    json!({"weight": 1, "data": {"id": id, "name": name, "dept_id": dept, "salary": salary}})
}

#[test]
fn join_and_aggregate_end_to_end() {
    let mut h = Harness::new(PROGRAM, &["joined", "by_dept"]);

    h.push_json("emp", emp(1, "ada", 10, 150000));
    h.push_json("emp", emp(2, "bob", 10, 90000));
    h.push_json("emp", emp(3, "cy", 20, 70000));
    h.push_json("dept", json!({"weight": 1, "data": {"id": 10, "dname": "eng"}}));
    h.push_json("dept", json!({"weight": 1, "data": {"id": 20, "dname": "ops"}}));

    let out = h.step();
    let joined = {
        let mut v = out.iter().find(|(n, _)| n == "joined").unwrap().1.clone();
        v.sort_by_key(|x| x.to_string());
        v
    };
    assert_eq!(
        joined,
        vec![
            json!({"weight": 1, "data": {"name": "ada", "dname": "eng"}}),
            json!({"weight": 1, "data": {"name": "bob", "dname": "eng"}}),
            json!({"weight": 1, "data": {"name": "cy", "dname": "ops"}}),
        ]
    );

    let by_dept = {
        let mut v = out.iter().find(|(n, _)| n == "by_dept").unwrap().1.clone();
        v.sort_by_key(|x| x.to_string());
        v
    };
    assert_eq!(
        by_dept,
        vec![
            json!({"weight": 1, "key": 10, "value": 150000}),
            json!({"weight": 1, "key": 20, "value": 70000}),
        ],
        "max salary per department"
    );
}

/// Retracting a row must cancel it and move the aggregate — the reason for
/// building on DBSP rather than recomputing.
#[test]
fn retraction_updates_the_aggregate() {
    let mut h = Harness::new(PROGRAM, &["by_dept"]);
    h.push_json("emp", emp(1, "ada", 10, 150000));
    h.push_json("emp", emp(2, "bob", 10, 90000));
    assert_eq!(h.take("by_dept"), vec![json!({"weight": 1, "key": 10, "value": 150000})]);

    // Retract the top earner; the max must fall back to 90000.
    let mut retract = emp(1, "ada", 10, 150000);
    retract["weight"] = json!(-1);
    h.push_json("emp", retract);

    assert_eq!(
        h.take("by_dept"),
        vec![
            json!({"weight": -1, "key": 10, "value": 150000}),
            json!({"weight": 1, "key": 10, "value": 90000}),
        ]
    );
}

#[test]
fn filter_uses_a_real_expression() {
    let mut h = Harness::new(PROGRAM, &["high_paid"]);
    h.push_json("emp", emp(1, "ada", 10, 150000));
    h.push_json("emp", emp(2, "bob", 10, 90000));

    let out = h.take("high_paid");
    assert_eq!(
        out,
        vec![json!({
            "weight": 1,
            "data": {"id": 1, "name": "ada", "dept_id": 10, "salary": 150000}
        })],
        "only the row above the threshold survives"
    );
}

/// A weight of 3 must stay one delta rather than becoming three records.
#[test]
fn weights_survive_the_round_trip() {
    let mut h = Harness::new(PROGRAM, &["high_paid"]);
    let mut r = emp(1, "ada", 10, 150000);
    r["weight"] = json!(3);
    h.push_json("emp", r);
    let out = h.take("high_paid");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["weight"], json!(3));
}

#[test]
fn weighted_count_counts_rows() {
    let src = r#"
        emp := input("emp")
        emp :: OrdZSet(record(id: i64, dept_id: i64))
        depts := map(emp, fun((row) -> row.dept_id))
        per_dept := weighted_count(depts)
    "#;
    let mut h = Harness::new(src, &["per_dept"]);
    for (id, dept) in [(1, 10), (2, 10), (3, 20)] {
        h.push_json("emp", json!({"weight": 1, "data": {"id": id, "dept_id": dept}}));
    }
    assert_eq!(
        h.take("per_dept"),
        vec![
            json!({"weight": 1, "key": 10, "value": 2}),
            json!({"weight": 1, "key": 20, "value": 1}),
        ]
    );
}

// -- diagnostics ------------------------------------------------------------

#[test]
fn join_key_mismatch_is_rejected() {
    let src = r#"
        a := input("a")
        a :: OrdZSet(record(id: i64))
        b := input("b")
        b :: OrdZSet(record(name: sql.SqlString))
        ai := map_index(a, fun((r) -> (r.id, r)))
        bi := map_index(b, fun((r) -> (r.name, r)))
        j  := join(ai, bi, fun((k, x, y) -> k))
    "#;
    let program = parse(src).expect("parse");
    let err = check(&program).expect_err("mismatched key types must be rejected");
    assert!(err.message.contains("equal key types"), "unexpected error: {err}");
}

#[test]
fn unknown_field_is_rejected() {
    let src = r#"
        a := input("a")
        a :: OrdZSet(record(id: i64))
        b := filter(a, fun((r) -> r.nope > 1))
    "#;
    let program = parse(src).expect("parse");
    let err = check(&program).expect_err("unknown field must be rejected");
    assert!(err.message.contains("no field `nope`"), "unexpected error: {err}");
}

#[test]
fn cycles_are_rejected() {
    let src = r#"
        a := input("a")
        a :: OrdZSet(record(id: i64))
        b := map(c, fun((r) -> r))
        c := map(b, fun((r) -> r))
    "#;
    let program = parse(src).expect("parse");
    let err = check(&program).expect_err("a cycle must be rejected");
    assert!(err.message.contains("cycle"), "unexpected error: {err}");
}

#[test]
fn input_without_a_typespec_is_rejected() {
    let program = parse(r#"a := input("a")"#).expect("parse");
    let err = check(&program).expect_err("an input needs a typespec");
    assert!(err.message.contains("typespec"), "unexpected error: {err}");
}
