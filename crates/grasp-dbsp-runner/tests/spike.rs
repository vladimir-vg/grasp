//! Does `dbsp`'s typed operator API instantiate at a single universal value
//! type?
//!
//! This is the design's central bet, stated in `docs/grasp-dbsp/mapping.md`. If
//! these tests do not compile, the architecture is wrong and no amount of
//! parser work rescues it — so they exist before the parser does, and they are
//! deliberately written against the `dbsp` API directly rather than through
//! this crate's own lowering.

use dbsp::operator::{Max, Min};
use dbsp::{DBData, OutputHandle, Runtime, ZWeight};
use dbsp::{OrdIndexedZSet, OrdZSet};
use grasp_dbsp_runner::value::DynValue;

/// The bet, as a compile-time assertion. Everything else depends on this.
#[test]
fn dyn_value_is_dbdata() {
    fn assert_dbdata<T: DBData>() {}
    assert_dbdata::<DynValue>();
}

fn row(id: i64, dept: i64, salary: i64) -> DynValue {
    DynValue::record([
        DynValue::I64(id),
        DynValue::I64(dept),
        DynValue::I64(salary),
    ])
}

/// Drain an output handle into a sorted `(key, value, weight)` list.
fn drain<K: DBData, V: DBData>(
    handle: &OutputHandle<OrdIndexedZSet<K, V>>,
) -> Vec<(K, V, ZWeight)> {
    use dbsp::IndexedZSetReader;
    let batch = handle.consolidate();
    let mut out: Vec<_> = batch.iter().collect();
    out.sort();
    out
}

fn drain_flat<K: DBData>(handle: &OutputHandle<OrdZSet<K>>) -> Vec<(K, ZWeight)> {
    use dbsp::IndexedZSetReader;
    let batch = handle.consolidate();
    let mut out: Vec<_> = batch.iter().map(|(k, (), w)| (k, w)).collect();
    out.sort();
    out
}

/// The full shape the language needs: a flat input, an index step, a join
/// between two indexed streams, an aggregate, and output handles — all with
/// closures that could equally have been built at runtime from an AST.
#[test]
fn typed_api_instantiates_at_one_value_type() {
    let (mut dbsp, (emp_in, dept_in, joined_out, agg_out)) = Runtime::init_circuit(1, |circuit| {
        let (emp, emp_in) = circuit.add_input_zset::<DynValue>();
        let (dept, dept_in) = circuit.add_input_zset::<DynValue>();

        // map_index: key on field 1 (dept_id), value is the whole row.
        let emp_idx = emp
            .map_index(|r: &DynValue| (r.field(1).cloned().unwrap_or(DynValue::None), r.clone()));
        // dept rows are (id, name); key on field 0.
        let dept_idx = dept
            .map_index(|r: &DynValue| (r.field(0).cloned().unwrap_or(DynValue::None), r.clone()));

        // join: build an output record from both sides.
        let joined = emp_idx.join(&dept_idx, |_k, e: &DynValue, d: &DynValue| {
            DynValue::record([
                e.field(0).cloned().unwrap_or(DynValue::None),
                d.field(1).cloned().unwrap_or(DynValue::None),
            ])
        });

        // aggregate: max salary per department.
        let salaries = emp.map_index(|r: &DynValue| {
            (
                r.field(1).cloned().unwrap_or(DynValue::None),
                r.field(2).cloned().unwrap_or(DynValue::None),
            )
        });
        let agg = salaries.aggregate(Max);

        Ok((emp_in, dept_in, joined.output(), agg.output()))
    })
    .expect("circuit construction");

    emp_in.push(row(1, 10, 100), 1);
    emp_in.push(row(2, 10, 200), 1);
    emp_in.push(row(3, 20, 50), 1);
    dept_in.push(
        DynValue::record([DynValue::I64(10), DynValue::str("eng")]),
        1,
    );
    dept_in.push(
        DynValue::record([DynValue::I64(20), DynValue::str("ops")]),
        1,
    );
    dbsp.transaction().unwrap();

    let joined = drain_flat(&joined_out);
    assert_eq!(joined.len(), 3, "three employees match a department");
    assert!(joined.iter().all(|(_, w)| *w == 1));

    let agg = drain(&agg_out);
    assert_eq!(
        agg,
        vec![
            (DynValue::I64(10), DynValue::I64(200), 1),
            (DynValue::I64(20), DynValue::I64(50), 1),
        ],
        "max salary per department"
    );
}

/// A retraction must cancel the insertion it retracts. This is the whole reason
/// for building on DBSP, and it is what a naive re-evaluation would get wrong.
#[test]
fn retractions_cancel() {
    let (mut dbsp, (input, out)) = Runtime::init_circuit(1, |circuit| {
        let (s, handle) = circuit.add_input_zset::<DynValue>();
        let idx = s.map_index(|r: &DynValue| {
            (
                r.field(1).cloned().unwrap_or(DynValue::None),
                r.field(2).cloned().unwrap_or(DynValue::None),
            )
        });
        Ok((handle, idx.aggregate(Min).output()))
    })
    .expect("circuit construction");

    input.push(row(1, 10, 100), 1);
    input.push(row(2, 10, 50), 1);
    dbsp.transaction().unwrap();
    assert_eq!(drain(&out), vec![(DynValue::I64(10), DynValue::I64(50), 1)]);

    // Retract the minimum; the aggregate must move up to 100.
    input.push(row(2, 10, 50), -1);
    dbsp.transaction().unwrap();
    let delta = drain(&out);
    assert_eq!(
        delta,
        vec![
            (DynValue::I64(10), DynValue::I64(50), -1),
            (DynValue::I64(10), DynValue::I64(100), 1),
        ],
        "the old minimum is retracted and the new one asserted"
    );
}

/// Weights of magnitude greater than one survive as a single entry rather than
/// being expanded into repeats. `docs/grasp-dbsp/mapping.md` makes the `weighted`
/// JSON format the default output encoding for exactly this reason.
#[test]
fn weights_greater_than_one_are_preserved() {
    let (mut dbsp, (input, out)) = Runtime::init_circuit(1, |circuit| {
        let (s, handle) = circuit.add_input_zset::<DynValue>();
        Ok((handle, s.output()))
    })
    .expect("circuit construction");

    input.push(row(1, 10, 100), 3);
    dbsp.transaction().unwrap();
    assert_eq!(drain_flat(&out), vec![(row(1, 10, 100), 3)]);
}
