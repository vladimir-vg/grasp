//! Lowering a checked [`Plan`] onto a `dbsp` circuit, and driving it.
//!
//! Every node is one of two Rust types — `OrdZSet<DynValue>` or
//! `OrdIndexedZSet<DynValue, DynValue>` — so the whole program is expressible
//! through `dbsp`'s ordinary typed API. Operator functions become closures
//! capturing an `Arc<TypedExpr>`, which is the only thing that has to be built
//! at runtime.

use crate::diag::{Diagnostic, Pass};
use crate::expr::{eval, is_true};
use crate::typecheck::{Agg, Plan, PlanOp};
use crate::value::{Acc, BatchType, DynValue, FpAcc, FpAccSemigroup};
use dbsp::Circuit;
use dbsp::algebra::F64;
use dbsp::algebra::{AddAssignByRef, HasZero, Semigroup};
use dbsp::dynamic::{DataTrait, DynUnit, Erase, WeightTrait};
use dbsp::operator::{Aggregator, ConstantGenerator, Fold, Generator, Max};
use dbsp::typed_batch::SpineSnapshot;
use dbsp::utils::Tup2;
use dbsp::{
    DBSPHandle, IndexedZSetReader, NestedCircuit, OrdIndexedZSet, OrdZSet, OutputHandle,
    RootCircuit, Runtime, Stream, ZSetHandle, ZWeight,
};
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;

/// `min` over a projection that may be absent.
///
/// `dbsp`'s `Min` returns the smallest value outright, and `DynValue::None`
/// sorts before every other — so it reports absence for a group that merely
/// *contains* an absent row, where SQL's `MIN` skips nulls. Feldera hit the same
/// thing and hand-wrote `MinSome1` for it, described in
/// `sql-to-dbsp-compiler/.../ir/aggregate/DBSPMinMax.java` as a "Special
/// hand-crafted DBSP aggregator for Min(Option<T>). None values are ignored".
/// This mirrors its semantics at our one value type, rather than adopting its
/// `Tup1<Option<V>>` shape, which would put a second batch type in a design
/// whose leverage is that there is exactly one.
///
/// **`max` needs no equivalent, and Feldera has none.** Absence sorting first is
/// exactly what `Max` wants: it walks the cursor backward, so it reaches a real
/// value first and only reports absence when there is nothing else.
#[derive(Clone)]
struct MinSkippingNone;

#[derive(Clone)]
struct MinSkippingNoneSemigroup;

impl Semigroup<DynValue> for MinSkippingNoneSemigroup {
    fn combine(left: &DynValue, right: &DynValue) -> DynValue {
        match (left.is_none(), right.is_none()) {
            (true, true) => DynValue::None,
            (true, false) => right.clone(),
            (false, true) => left.clone(),
            (false, false) => left.min(right).clone(),
        }
    }
}

impl<T: dbsp::Timestamp> Aggregator<DynValue, T, ZWeight> for MinSkippingNone {
    type Accumulator = DynValue;
    type Output = DynValue;
    type Semigroup = MinSkippingNoneSemigroup;

    fn aggregate<VTrait, RTrait>(
        &self,
        cursor: &mut dyn dbsp::trace::Cursor<VTrait, DynUnit, T, RTrait>,
    ) -> Option<DynValue>
    where
        VTrait: DataTrait + ?Sized,
        RTrait: WeightTrait + ?Sized,
        DynValue: Erase<VTrait>,
        ZWeight: Erase<RTrait>,
    {
        // Absence sorts first, so the first present value reached is the
        // smallest. Seeing only absent values means the group exists and has no
        // value — `NONE`, not "no row". Seeing nothing at all means the group is
        // empty, and `None` here is what drops it.
        let mut seen_absent = false;
        while cursor.key_valid() {
            let mut weight: ZWeight = HasZero::zero();
            cursor.map_times(&mut |_, w| {
                weight.add_assign_by_ref(unsafe { w.downcast() });
            });
            if !weight.is_zero() {
                let key = unsafe { cursor.key().downcast::<DynValue>() };
                if key.is_none() {
                    seen_absent = true;
                } else {
                    return Some(key.clone());
                }
            }
            cursor.step_key();
        }
        seen_absent.then_some(DynValue::None)
    }

    fn finalize(&self, accumulator: DynValue) -> DynValue {
        accumulator
    }
}

/// A failure while *running* a built circuit, as distinct from a failure to
/// build one. Compilation problems are [`Diagnostic`]s; these are not, because
/// they have no source location — nothing in the program text caused them.
#[derive(Debug)]
pub struct RunError(pub String);

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RunError {}

type Flat<C> = Stream<C, OrdZSet<DynValue>>;
type Indexed<C> = Stream<C, OrdIndexedZSet<DynValue, DynValue>>;

/// A lowered node. The two stream shapes correspond exactly to the language's
/// two batch types; `Group` is a `fixpoint`, which yields one stream per
/// recursive parameter.
///
/// Generic over the circuit, because a fixpoint body is built in a
/// `NestedCircuit` — a different Rust type from `RootCircuit`.
enum Node<C: Circuit> {
    Flat(Flat<C>),
    Indexed(Indexed<C>),
    Group(Vec<Node<C>>),
}

impl<C: Circuit> Clone for Node<C> {
    fn clone(&self) -> Self {
        match self {
            Node::Flat(s) => Node::Flat(s.clone()),
            Node::Indexed(s) => Node::Indexed(s.clone()),
            Node::Group(v) => Node::Group(v.iter().map(Clone::clone).collect()),
        }
    }
}

impl<C: Circuit> Node<C> {
    /// Names the operator behind this stream, so its state can be checkpointed
    /// and restored.
    ///
    /// The name is the node's *content id* rather than its name or position:
    /// `dbsp` requires an id that "identifies the same computation across
    /// restarts", derived "from the program … rather than from anything
    /// positional" (`dbsp/src/operator/recursive.rs`). A nested node's name is
    /// `filter@3:12`, which is exactly what that rules out.
    fn named(self, id: &str) -> Self {
        match self {
            Node::Flat(s) => Node::Flat(s.set_persistent_id(Some(id))),
            Node::Indexed(s) => Node::Indexed(s.set_persistent_id(Some(id))),
            // A fixpoint is not itself a stream; its members are named as they
            // are exported.
            Node::Group(v) => Node::Group(v),
        }
    }

    fn flat(&self, node: &crate::typecheck::PlanNode) -> Result<&Flat<C>, Diagnostic> {
        match self {
            Node::Flat(s) => Ok(s),
            _ => Err(shape_error(node, "a flat zset")),
        }
    }
    fn indexed(&self, node: &crate::typecheck::PlanNode) -> Result<&Indexed<C>, Diagnostic> {
        match self {
            Node::Indexed(s) => Ok(s),
            _ => Err(shape_error(node, "an indexed_zset")),
        }
    }
}

/// Splits the `record(key:, value:)` an indexing operator's function returned.
///
/// The checker has already established that this is a two-field record and
/// where each field sits, so a value of any other shape means the two passes
/// disagree — hence `NONE` rather than a panic, which would take the worker
/// thread and the circuit with it.
fn split_kv(row: DynValue, kv: crate::typecheck::KeyValue) -> (DynValue, DynValue) {
    match row.fields() {
        Some(f) if f.len() == 2 => (f[kv.key].clone(), f[kv.value].clone()),
        _ => (DynValue::None, DynValue::None),
    }
}

/// The rows a fan-out function produced. Total for the same reason as
/// `split_kv`: the checker guarantees an array, and a panic here would kill the
/// circuit rather than one row.
fn rows(v: DynValue) -> Vec<DynValue> {
    match v {
        DynValue::Array(items) => items,
        _ => Vec::new(),
    }
}

/// A shape mismatch here means the type checker and the lowering disagree,
/// which is a bug in one of them rather than a problem with the program.
fn shape_error(node: &crate::typecheck::PlanNode, wanted: &str) -> Diagnostic {
    Diagnostic::error(
        Pass::Lower,
        node.span,
        format!("`{}` needs {wanted} here, but is `{}`", node.name, node.ty),
    )
}

/// An output handle, in whichever shape its node has.
///
/// The handle is over a [`SpineSnapshot`] because these are
/// [`Stream::accumulate_output`] sinks rather than [`Stream::output`] ones —
/// see [`Runner::step`] for why that is not a detail.
enum Out {
    Flat(OutputHandle<SpineSnapshot<OrdZSet<DynValue>>>),
    Indexed(OutputHandle<SpineSnapshot<OrdIndexedZSet<DynValue, DynValue>>>),
}

/// One change: a value, and the weight by which its multiplicity changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// For an indexed stream this is the key; for a flat one, the row.
    pub key: DynValue,
    /// `None` for a flat stream.
    pub value: Option<DynValue>,
    pub weight: ZWeight,
}

/// How a circuit is **run**, as opposed to what it computes.
///
/// Nothing here reaches the plan: two runners built from one plan with
/// different configurations compute the same relations, node for node and row
/// for row. That is what makes this a separate argument rather than part of
/// the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerConfig {
    /// `dbsp` worker threads.
    ///
    /// Batches are sharded across them by `key.default_hash() % workers`
    /// (`dbsp/src/operator/dynamic/communication/shard.rs:454`), so more than
    /// one rests on invariants 2 and 3 in `mapping.md` — equal values hashing
    /// equally, and the hash being stable — and on nothing else in this
    /// module: every operator that needs its input placed shards it itself,
    /// and an embedder that shards on their behalf gets it wrong
    /// (`dbsp/src/operator/communication/shard.rs:50-57`).
    ///
    /// `NonZeroUsize` because `Layout::new_solo` asserts rather than
    /// diagnosing (`dbsp/src/circuit/dbsp_handle.rs:105-108`), and a panic on a
    /// reachable path is the one thing this crate does not do. It is also what
    /// `available_parallelism` returns, for the CLI that will eventually pass
    /// one — the count belongs there and not in a default here, since a count
    /// taken from the host would make a placement bug reproduce on a three-core
    /// machine and not on a four-core one.
    pub workers: NonZeroUsize,
}

impl Default for RunnerConfig {
    /// One worker: a count nobody chose should be the one that needs no
    /// invariant to be right.
    fn default() -> Self {
        Self {
            workers: NonZeroUsize::MIN,
        }
    }
}

/// A built circuit, its input handles, and the outputs that were selected.
pub struct Runner {
    dbsp: DBSPHandle,
    inputs: HashMap<String, ZSetHandle<DynValue>>,
    outputs: Vec<(String, Out)>,
}

impl Runner {
    /// Builds the circuit for `plan`, exposing the nodes named in `outputs`.
    ///
    /// Nodes that are not named are still constructed — there is no dead-code
    /// elimination.
    pub fn build(
        plan: &Plan,
        outputs: &[String],
        config: RunnerConfig,
    ) -> Result<Runner, Vec<Diagnostic>> {
        // A node may be selected by its declared name or by its content id, so
        // a nested node — which has no name, only a `filter@3:12` label — can
        // still be observed.
        let ids = crate::typecheck::content_ids(plan);
        let index_of = |name: &String| -> Option<usize> {
            plan.by_name
                .get(name)
                .copied()
                .or_else(|| ids.iter().position(|i| i == name))
        };
        let mut wanted: Vec<(String, usize)> = Vec::with_capacity(outputs.len());
        for name in outputs {
            match index_of(name) {
                Some(i) => wanted.push((name.clone(), i)),
                None => {
                    return Err(vec![Diagnostic::error(
                        Pass::Lower,
                        None,
                        format!("no node named `{name}` to output"),
                    )]);
                }
            }
        }

        let plan = plan.clone();

        // `Runtime::init_circuit` clones this closure into every worker thread
        // and runs it there — and does *not* check what they built
        // (`dbsp/src/circuit/dbsp_handle.rs:1121-1125`, "we don't check"): it
        // keeps worker 0's answer. So determinism here is an obligation rather
        // than a checked invariant, and what it protects is the per-worker
        // `Runtime::sequence_next` counter that hands out input and exchange
        // ids (`dbsp/src/circuit/runtime.rs:1305-1313`). Workers that build
        // different circuits desync it and then deadlock or cross-wire, with no
        // error anywhere.
        //
        // Hence: nodes are visited in plan order, ids and the output selection
        // are computed above rather than here, and nothing in this closure
        // iterates a hash map. What each worker clones is one immutable `Plan`
        // whose expressions are behind `Arc`s, so the circuits differ only if
        // the walk does. `Runner::recheck_determinism` is how a test asks.
        let (dbsp, (inputs, outs)) = Runtime::init_circuit(config.workers, move |circuit| {
            let mut nodes: Vec<Node<RootCircuit>> = Vec::with_capacity(plan.nodes.len());
            let mut inputs: Vec<(String, ZSetHandle<DynValue>)> = Vec::new();

            for (i, node) in plan.nodes.iter().enumerate() {
                let built = build_root(circuit, &plan, &nodes, node, &mut inputs, &ids)?;
                nodes.push(built.named(&ids[i]));
            }

            let mut outs: Vec<(String, Out)> = Vec::new();
            for (name, idx) in &wanted {
                let idx = *idx;
                outs.push((
                    name.clone(),
                    match &nodes[idx] {
                        // `accumulate_output`, not `output`: see `step`.
                        Node::Flat(s) => Out::Flat(s.accumulate_output()),
                        Node::Indexed(s) => Out::Indexed(s.accumulate_output()),
                        // A fixpoint instance is not itself a stream; only its
                        // recursive members are, and the checker registers
                        // those under `<instance>.<label>`.
                        Node::Group(_) => {
                            return Err(anyhow::anyhow!(
                                "`{name}` names a fixpoint, not a stream; \
                                 select one of its recursive nodes"
                            ));
                        }
                    },
                ));
            }
            Ok((inputs, outs))
        })
        .map_err(|e| {
            // `Runtime::init_circuit` insists on `anyhow::Error`, and wraps it
            // in `Error::Constructor`, so a Diagnostic raised inside the
            // constructor round-trips out through two layers.
            let fallback = |e: &dyn fmt::Display| {
                vec![Diagnostic::error(
                    Pass::Lower,
                    None,
                    format!("building the circuit: {e}"),
                )]
            };
            match e {
                dbsp::Error::Constructor(any) => match any.downcast::<Diagnostic>() {
                    Ok(diag) => vec![diag],
                    Err(other) => fallback(&other),
                },
                other => fallback(&other),
            }
        })?;

        Ok(Runner {
            dbsp,
            // Worker 0's handles, and that is not a single-worker assumption:
            // `InputHandle::new` keys the runtime's shared local store by
            // `sequence_next()`, so every worker's constructor resolved to the
            // same `Arc`, whose mailbox vector is sized to all of them
            // (`dbsp/src/operator/input.rs:811-827`). `push` then round-robins
            // over those mailboxes — the caller never shards by key, and the
            // operators that need placement re-shard anyway.
            inputs: inputs.into_iter().collect(),
            outputs: outs,
        })
    }

    /// Queues a change to an input table. Applied at the next [`Self::step`].
    pub fn push(&self, table: &str, row: DynValue, weight: ZWeight) -> Result<(), RunError> {
        let handle = self
            .inputs
            .get(table)
            .ok_or_else(|| RunError(format!("no input table `{table}`")))?;
        handle.push(row, weight);
        Ok(())
    }

    /// Runs one transaction and drains the output deltas it produced.
    ///
    /// A transaction is the semantic unit: the logical clock advances between
    /// transactions, not within them.
    pub fn step(&mut self) -> Result<Vec<(String, Vec<Delta>)>, RunError> {
        self.dbsp
            .transaction()
            .map_err(|e| RunError(format!("running a transaction: {e}")))?;

        let mut out = Vec::new();
        for (name, handle) in &self.outputs {
            let deltas = match handle {
                Out::Flat(h) => h
                    .concat()
                    .consolidate()
                    .iter()
                    .map(|(k, (), w)| Delta {
                        key: k,
                        value: None,
                        weight: w,
                    })
                    .collect(),
                Out::Indexed(h) => h
                    .concat()
                    .consolidate()
                    .iter()
                    .map(|(k, v, w)| Delta {
                        key: k,
                        value: Some(v),
                        weight: w,
                    })
                    .collect(),
            };
            out.push((name.clone(), deltas));
        }
        Ok(out)
    }

    /// Builds this plan's circuit a second time in every worker and requires
    /// each worker's two copies to agree.
    ///
    /// The obligation named at [`Runner::build`] has one place where `dbsp`
    /// checks anything: `create_bootstrap_circuit` re-runs the constructor on
    /// every worker, at the real worker count, and refuses a fingerprint
    /// mismatch with "the circuit constructor is nondeterministic"
    /// (`dbsp/src/circuit/dbsp_handle.rs:977-991`). Given no checkpoint it
    /// restores nothing, so borrowing it as a check costs one rebuild and the
    /// teardown below.
    ///
    /// **Be exact about what that is.** Each worker compares its own two
    /// builds, so what this catches is a constructor that is not a pure
    /// function of what it captured — a hash map reaching node order, a
    /// counter, a clock. It does *not* compare the workers with each other,
    /// and the fingerprint is FNV over each node's **type name**
    /// (`dbsp/src/circuit/fingerprinter.rs`,
    /// `circuit_builder.rs:8788-8795`), so a constructor that read
    /// `Runtime::worker_index()` into a node's *name* would pass this and
    /// still be wrong. Against that there is an argument rather than a test:
    /// the closure captures one immutable `Plan` and a `Vec` of ids computed
    /// before it, reads nothing else, and every expression under it is an
    /// `Arc`.
    ///
    /// Call it between transactions.
    pub fn recheck_determinism(&mut self) -> Result<(), RunError> {
        self.dbsp
            .create_bootstrap_circuit()
            .map_err(|e| RunError(format!("rebuilding the circuit: {e}")))?;
        self.dbsp
            .destroy_bootstrap_circuit()
            .map_err(|e| RunError(format!("discarding the rebuilt circuit: {e}")))
    }

    pub fn kill(self) {
        let _ = self.dbsp.kill();
    }
}

/// The operator arms shared by every circuit type.
///
/// `dbsp` exposes its operators as inherent methods on concrete circuit types
/// rather than through a trait, so a function generic over `C: Circuit` cannot
/// call them. The lowering is therefore instantiated once per circuit type, and
/// this macro is what keeps the two copies from drifting.
macro_rules! operator_arms {
    ($node:expr, $dep:expr, $op:expr) => {
        match $op {
            // Row-at-a-time over either shape. `dbsp` hands an indexed stream's
            // element as one `(&K, &V)` tuple, which is the same closure shape
            // `filter` already uses.
            PlanOp::Map { input, f } => {
                let f = f.clone();
                match $dep(*input) {
                    Node::Flat(s) => Node::Flat(s.map(move |r: &DynValue| eval(&f, &[r]))),
                    // Mapping an indexed stream flattens it — the way out of an
                    // indexed shape that is not a join.
                    Node::Indexed(s) => Node::Flat(
                        s.map(move |(k, v): (&DynValue, &DynValue)| eval(&f, &[k, v])),
                    ),
                    Node::Group(_) => return Err(shape_error($node, "a stream").into()),
                }
            }

            PlanOp::Filter { input, f } => {
                let f = f.clone();
                match $dep(*input) {
                    Node::Flat(s) => {
                        Node::Flat(s.filter(move |r: &DynValue| is_true(&eval(&f, &[r]))))
                    }
                    // An indexed stream's element is the (key, value) pair.
                    Node::Indexed(s) => Node::Indexed(
                        s.filter(move |(k, v): (&DynValue, &DynValue)| is_true(&eval(&f, &[k, v]))),
                    ),
                    Node::Group(_) => return Err(shape_error($node, "a stream").into()),
                }
            }

            // The function returns one `record(key:, value:)`, which is split
            // here. `kv` carries where the two fields sit, so writing them in
            // either order means the same thing.
            PlanOp::MapIndex { input, f, kv } => {
                let (f, kv) = (f.clone(), *kv);
                match $dep(*input) {
                    Node::Flat(s) => Node::Indexed(
                        s.map_index(move |r: &DynValue| split_kv(eval(&f, &[r]), kv)),
                    ),
                    Node::Indexed(s) => Node::Indexed(s.map_index(
                        move |(k, v): (&DynValue, &DynValue)| split_kv(eval(&f, &[k, v]), kv),
                    )),
                    Node::Group(_) => return Err(shape_error($node, "a stream").into()),
                }
            }

            PlanOp::Join { left, right, f } => {
                let f = f.clone();
                Node::Flat($dep(*left).indexed($node)?.join(
                    $dep(*right).indexed($node)?,
                    move |k: &DynValue, a: &DynValue, b: &DynValue| eval(&f, &[k, a, b]),
                ))
            }

            PlanOp::Antijoin { left, right } => Node::Indexed(
                $dep(*left)
                    .indexed($node)?
                    .antijoin($dep(*right).indexed($node)?),
            ),

            PlanOp::Distinct { input } => match $dep(*input) {
                Node::Flat(s) => Node::Flat(s.distinct()),
                Node::Indexed(s) => Node::Indexed(s.distinct()),
                Node::Group(_) => return Err(shape_error($node, "a stream").into()),
            },

            PlanOp::Aggregate { input, agg, f, projection } => match agg {
                // `Stream::aggregate` has no projection argument: it aggregates over
                // the value directly. So re-project the value first, then aggregate.
                Agg::Min | Agg::Max => {
                    let f = f.clone();
                    let projected = $dep(*input)
                        .indexed($node)?
                        .map_index(move |(k, v): (&DynValue, &DynValue)| (k.clone(), eval(&f, &[v])));
                    Node::Indexed(match agg {
                        Agg::Min => projected.aggregate(MinSkippingNone),
                        _ => projected.aggregate(Max),
                    })
                }

                // Floating-point `sum` and `avg` take the *non-linear* path: fp
                // addition is not associative, so an incrementally maintained
                // sum would depend on the order changes arrived in. A fold
                // replays the group in cursor order instead, which is
                // deterministic. Feldera splits them the same way, choosing the
                // linear path only `if (this.linearAllowed && !this.fp())`.
                //
                // The two paths must agree, because which one runs is invisible
                // from the source. `sum` and `avg` are absent under one rule: the
                // projection is optional and no row contributed. `count` is never
                // floating point — it counts.
                Agg::Sum | Agg::Avg if projection.non_null() == &crate::value::TypeDesc::F64 => {
                    let proj = f.clone();
                    let agg = *agg;
                    let may_be_none =
                        matches!(&$node.ty, BatchType::IndexedZSet(_, v) if v.is_optional());
                    Node::Indexed($dep(*input).indexed($node)?.aggregate(Fold::<
                        DynValue,
                        FpAcc,
                        FpAccSemigroup,
                        _,
                        _,
                    >::with_output(
                        FpAcc::default(),
                        move |acc: &mut FpAcc, v: &DynValue, w: ZWeight| {
                            if let DynValue::F64(x) = eval(&proj, &[v]) {
                                acc.sum = acc.sum + F64::new(x.into_inner() * w as f64);
                                acc.rows += w;
                            }
                        },
                        move |acc: FpAcc| match agg {
                            Agg::Avg if acc.rows == 0 => DynValue::None,
                            Agg::Avg => {
                                DynValue::F64(F64::new(acc.sum.into_inner() / acc.rows as f64))
                            }
                            _ if acc.rows == 0 && may_be_none => DynValue::None,
                            _ => DynValue::F64(acc.sum),
                        },
                    )))
                }

                // `sum`, `avg` and `count` over anything else are linear: each
                // row contributes independently, scaled by its weight, which is
                // what lets `dbsp` maintain them without replaying the group.
                Agg::Sum | Agg::Avg | Agg::Count => {
                    let proj = f.clone();
                    let agg = *agg;
                    // A fold's result type is optional exactly when its
                    // projection is, so the node's own type says whether absence
                    // is legal.
                    let may_be_none =
                        matches!(&$node.ty, BatchType::IndexedZSet(_, v) if v.is_optional());
                    Node::Indexed($dep(*input).indexed($node)?.aggregate_linear_postprocess(
                        move |v: &DynValue| match eval(&proj, &[v]) {
                            DynValue::I64(n) => Acc::value(n),
                            // A `NONE` projection contributes to no sum and to no
                            // count, which is what separates `count` from
                            // `weighted_count`.
                            DynValue::None => Acc::none(),
                            // Present, and not a number. `sum` and `avg` cannot
                            // reach here — the checker requires a numeric
                            // projection of them — but `count` can, and counts
                            // it: what it counts is what is there, of whatever
                            // type. Folding this into the absent case is what
                            // made `count` over a `string` return zero.
                            _ => Acc::counted(),
                        },
                        move |acc: Acc| match agg {
                            Agg::Count => DynValue::I64(acc.rows),
                            // Only an all-`NONE` group has nothing to report, and
                            // that needs an optional projection to arise at all.
                            // With a definite projection the weighted sum is the
                            // answer even when the group's weights cancel — so
                            // this never returns absence in a column typed `i64`.
                            //
                            // Unconditional for `avg` all the same, since it is
                            // also what keeps the division below total.
                            Agg::Avg if acc.rows == 0 => DynValue::None,
                            Agg::Sum if acc.rows == 0 && may_be_none => DynValue::None,
                            // Integer division, so the mean of `i64`s truncates
                            // toward zero. The division happens here rather than
                            // in the accumulator, which is what keeps the fold
                            // linear.
                            Agg::Avg => DynValue::I64(acc.sum / acc.rows),
                            Agg::Sum => DynValue::I64(acc.sum),
                            Agg::Min | Agg::Max => unreachable!("handled above"),
                        },
                    ))
                }
            },

            PlanOp::WeightedCount { input } => {
                // `weighted_count` yields `OrdIndexedZSet<K, ZWeight>` — the value is
                // a raw i64, off the uniform shape — so box it back into a DynValue.
                let counted = $dep(*input).flat($node)?.weighted_count();
                Node::Indexed(
                    counted.map_index(|(k, w): (&DynValue, &ZWeight)| (k.clone(), DynValue::I64(*w))),
                )
            }

            PlanOp::Neg { input } => match $dep(*input) {
                Node::Flat(s) => Node::Flat(s.neg()),
                Node::Indexed(s) => Node::Indexed(s.neg()),
                Node::Group(_) => return Err(shape_error($node, "a stream").into()),
            },

            PlanOp::Plus { left, right } => match ($dep(*left), $dep(*right)) {
                (Node::Flat(a), Node::Flat(b)) => Node::Flat(a.plus(b)),
                (Node::Indexed(a), Node::Indexed(b)) => Node::Indexed(a.plus(b)),
                _ => return Err(shape_error($node, "matching shapes in `plus`").into()),
            },

            PlanOp::Minus { left, right } => match ($dep(*left), $dep(*right)) {
                (Node::Flat(a), Node::Flat(b)) => Node::Flat(a.minus(b)),
                (Node::Indexed(a), Node::Indexed(b)) => Node::Indexed(a.minus(b)),
                _ => return Err(shape_error($node, "matching shapes in `minus`").into()),
            },

            // One row per element of the array the function returns, so the
            // fan-out is whatever the data says.
            PlanOp::FlatMap { input, f } => {
                let f = f.clone();
                match $dep(*input) {
                    Node::Flat(s) => {
                        Node::Flat(s.flat_map(move |r: &DynValue| rows(eval(&f, &[r]))))
                    }
                    Node::Indexed(s) => Node::Flat(
                        s.flat_map(move |(k, v): (&DynValue, &DynValue)| rows(eval(&f, &[k, v]))),
                    ),
                    Node::Group(_) => return Err(shape_error($node, "a stream").into()),
                }
            }

            PlanOp::FlatMapIndex { input, f, kv } => {
                let (f, kv) = (f.clone(), *kv);
                let split = move |v: DynValue| {
                    rows(v).into_iter().map(move |row| split_kv(row, kv)).collect::<Vec<_>>()
                };
                match $dep(*input) {
                    Node::Flat(s) => Node::Indexed(
                        s.flat_map_index(move |r: &DynValue| split(eval(&f, &[r]))),
                    ),
                    Node::Indexed(s) => Node::Indexed(s.flat_map_index(
                        move |(k, v): (&DynValue, &DynValue)| split(eval(&f, &[k, v])),
                    )),
                    Node::Group(_) => return Err(shape_error($node, "a stream").into()),
                }
            }

            PlanOp::JoinIndex { left, right, f, kv } => {
                let (f, kv) = (f.clone(), *kv);
                Node::Indexed($dep(*left).indexed($node)?.join_index(
                    $dep(*right).indexed($node)?,
                    move |k: &DynValue, a: &DynValue, b: &DynValue| {
                        // `join_index` takes an iterator of pairs; ours is always
                        // exactly one, since the body returns a single record.
                        std::iter::once(split_kv(eval(&f, &[k, a, b]), kv))
                    },
                ))
            }

            PlanOp::Integrate { input } => match $dep(*input) {
                Node::Flat(s) => Node::Flat(s.integrate()),
                Node::Indexed(s) => Node::Indexed(s.integrate()),
                Node::Group(_) => return Err(shape_error($node, "a stream").into()),
            },

            PlanOp::Differentiate { input } => match $dep(*input) {
                Node::Flat(s) => Node::Flat(s.differentiate()),
                Node::Indexed(s) => Node::Indexed(s.differentiate()),
                Node::Group(_) => return Err(shape_error($node, "a stream").into()),
            },

            PlanOp::Delay { input } => match $dep(*input) {
                Node::Flat(s) => Node::Flat(s.delay()),
                Node::Indexed(s) => Node::Indexed(s.delay()),
                Node::Group(_) => return Err(shape_error($node, "a stream").into()),
            },

            PlanOp::Sum { inputs: ins } => {
                let head = $dep(ins[0]);
                match head {
                    Node::Group(_) => return Err(shape_error($node, "a stream").into()),
                    Node::Flat(first) => {
                        let rest: Vec<_> = ins[1..]
                            .iter()
                            .map(|i| $dep(*i).flat($node))
                            .collect::<Result<_, _>>()?;
                        Node::Flat(first.sum(rest))
                    }
                    Node::Indexed(first) => {
                        let rest: Vec<_> = ins[1..]
                            .iter()
                            .map(|i| $dep(*i).indexed($node))
                            .collect::<Result<_, _>>()?;
                        Node::Indexed(first.sum(rest))
                    }
                }
            }

            other => return Err(unsupported($node, other).into()),
        }
    };
}

/// An operator that cannot appear where it did — `input` inside a fixpoint, or
/// a fixpoint inside one. The checker should have caught it, so this is a
/// disagreement between the two rather than a problem with the program.
fn unsupported(node: &crate::typecheck::PlanNode, op: &PlanOp) -> Diagnostic {
    Diagnostic::error(
        Pass::Lower,
        node.span,
        format!("`{}` cannot be built here ({op:?})", node.name),
    )
}

fn build_root(
    circuit: &mut RootCircuit,
    plan: &Plan,
    built: &[Node<RootCircuit>],
    node: &crate::typecheck::PlanNode,
    inputs: &mut Vec<(String, ZSetHandle<DynValue>)>,
    ids: &[String],
) -> Result<Node<RootCircuit>, anyhow::Error> {
    let _ = plan;
    let dep = |i: usize| -> &Node<RootCircuit> { &built[i] };

    Ok(match &node.op {
        // A source that always yields the zero batch. `HasZero` on `TypedBatch`
        // is what makes this expressible; the erased batch types have no zero
        // without factories.
        PlanOp::Empty => match &node.ty {
            BatchType::ZSet(_) => {
                Node::Flat(circuit.add_source(Generator::new(dbsp::algebra::HasZero::zero)))
            }
            BatchType::IndexedZSet(..) => {
                Node::Indexed(circuit.add_source(Generator::new(dbsp::algebra::HasZero::zero)))
            }
        },

        // A source holding exactly these rows, delivered in the first
        // transaction and zero thereafter.
        //
        // `ConstantGenerator` yields the batch on every step and is stateless —
        // `is_deterministic_source` is true, so a restore reproduces it by
        // re-running rather than by replaying it. `differentiate` is what turns
        // "always" into "once", and it does so in a real operator carrying a
        // persistent id, so the fire-once state is checkpointed. A `Generator`
        // with a captured `bool` would put that state in a closure `dbsp` does
        // not know about, and a restore would fire the rows a second time.
        PlanOp::Constant { rows } => {
            let batch = OrdZSet::<DynValue>::from_keys(
                (),
                rows.iter().map(|v| Tup2(v.clone(), 1)).collect(),
            );
            // The source is named *before* `differentiate` is built: that
            // expands to `minus(x, delay(x))`, and `delay` derives its own
            // persistent id from its input's at construction time. The caller
            // names what this returns, which is the `minus` — too late for the
            // stream underneath. This is the same ordering the fixpoint
            // lowering follows for a recursive stream.
            let source = circuit
                .add_source(ConstantGenerator::new(batch))
                .set_persistent_id(Some(&format!("{}.const", ids[built.len()])));
            Node::Flat(source.differentiate())
        }

        PlanOp::Input { table } => {
            let (stream, handle) = circuit.add_input_zset::<DynValue>();
            inputs.push((table.clone(), handle));
            Node::Flat(stream)
        }

        PlanOp::Fixpoint { body, outputs } => {
            build_fixpoint(circuit, built, node, body, outputs, ids)?
        }

        // One convergent stream of a fixpoint. Only recursive members are
        // exported; other body nodes exist only inside the nested circuit.
        PlanOp::FixpointExport { fixpoint, slot } => match dep(*fixpoint) {
            Node::Group(streams) => streams[*slot].clone(),
            _ => return Err(shape_error(node, "a fixpoint").into()),
        },

        other => operator_arms!(node, dep, other),
    })
}

/// Builds a fixpoint body inside the nested circuit.
///
/// `Import` and `RecVar` are resolved here rather than in `build_nested`,
/// because only this scope holds both the parent's streams and the child
/// circuit needed to import them.
fn build_body(
    child: &NestedCircuit,
    vars: &[Node<NestedCircuit>],
    body: &[crate::typecheck::PlanNode],
    outer: &[Node<RootCircuit>],
    ids: &[String],
) -> Result<Vec<Node<NestedCircuit>>, Diagnostic> {
    let mut nodes: Vec<Node<NestedCircuit>> = Vec::with_capacity(body.len());
    for (i, bn) in body.iter().enumerate() {
        let built = match &bn.op {
            // `delta0` carries a parent stream into the child circuit. It needs
            // `HasZero`, which `TypedBatch` has and the erased batches do not.
            PlanOp::Import { outer: i } => match &outer[*i] {
                Node::Flat(s) => Node::Flat(s.delta0(child)),
                Node::Indexed(s) => Node::Indexed(s.delta0(child)),
                Node::Group(_) => return Err(shape_error(bn, "a stream")),
            },
            PlanOp::RecVar { slot } => vars[*slot].clone(),
            _ => build_nested(&nodes, bn).map_err(|e| {
                e.downcast::<Diagnostic>()
                    .unwrap_or_else(|o| Diagnostic::error(Pass::Lower, bn.span, o.to_string()))
            })?,
        };
        // Every stream created inside a recursive scope needs a persistent id,
        // or taking a checkpoint fails with `NoPersistentId`. A `RecVar` was
        // already named by the caller, before anything was built from it.
        nodes.push(if matches!(bn.op, PlanOp::RecVar { .. }) {
            built
        } else {
            built.named(&ids[i])
        });
    }
    Ok(nodes)
}

fn build_fixpoint(
    circuit: &mut RootCircuit,
    built: &[Node<RootCircuit>],
    node: &crate::typecheck::PlanNode,
    body: &[crate::typecheck::PlanNode],
    outputs: &[usize],
    parent_ids: &[String],
) -> Result<Node<RootCircuit>, anyhow::Error> {
    // The checker guarantees every recursive stream shares one shape, which is
    // what lets a single nested batch type serve them all.
    let flat = matches!(body[outputs[0]].ty, BatchType::ZSet(_));

    // Ids for the body, and for the recursive slots within it. A slot's id is
    // its own `RecVar` node's, so it identifies the same recursive stream
    // across restarts however the program was written.
    let body_ids = crate::typecheck::body_content_ids(parent_ids, body);
    let mut rec_ids: Vec<String> = vec![String::new(); outputs.len()];
    for (i, bn) in body.iter().enumerate() {
        if let PlanOp::RecVar { slot } = &bn.op {
            rec_ids[*slot] = body_ids[i].clone();
        }
    }

    // A failure inside the closure means the checker and the lowering disagree.
    // `recursive_dynamic` cannot carry a Diagnostic out, so it is parked here
    // and the closure returns its inputs unchanged to keep the arity right.
    let mut failure: Option<Diagnostic> = None;

    let group = if flat {
        let exports = circuit.recursive_dynamic(
            outputs.len(),
            |child: &NestedCircuit, vars: Vec<Flat<NestedCircuit>>| {
                // `dbsp` derives the ids of the implicit `distinct` and the
                // exporting integral from the recursive stream's own id, and
                // does so as they are constructed — so these must be named
                // before `build_body` builds anything from them.
                let wrapped: Vec<_> = vars
                    .iter()
                    .enumerate()
                    .map(|(slot, v)| Node::Flat(v.set_persistent_id(Some(&rec_ids[slot]))))
                    .collect();
                match build_body(child, &wrapped, body, built, &body_ids) {
                    Ok(nodes) => Ok(outputs
                        .iter()
                        .map(|i| match &nodes[*i] {
                            Node::Flat(s) => s.clone(),
                            _ => unreachable!("checked shape"),
                        })
                        .collect()),
                    Err(e) => {
                        failure = Some(e);
                        Ok(vars)
                    }
                }
            },
        )?;
        exports.into_iter().map(Node::Flat).collect::<Vec<_>>()
    } else {
        let exports = circuit.recursive_dynamic(
            outputs.len(),
            |child: &NestedCircuit, vars: Vec<Indexed<NestedCircuit>>| {
                // `dbsp` derives the ids of the implicit `distinct` and the
                // exporting integral from the recursive stream's own id, and
                // does so as they are constructed — so these must be named
                // before `build_body` builds anything from them.
                let wrapped: Vec<_> = vars
                    .iter()
                    .enumerate()
                    .map(|(slot, v)| Node::Indexed(v.set_persistent_id(Some(&rec_ids[slot]))))
                    .collect();
                match build_body(child, &wrapped, body, built, &body_ids) {
                    Ok(nodes) => Ok(outputs
                        .iter()
                        .map(|i| match &nodes[*i] {
                            Node::Indexed(s) => s.clone(),
                            _ => unreachable!("checked shape"),
                        })
                        .collect()),
                    Err(e) => {
                        failure = Some(e);
                        Ok(vars)
                    }
                }
            },
        )?;
        exports.into_iter().map(Node::Indexed).collect::<Vec<_>>()
    };

    if let Some(e) = failure {
        return Err(e.into());
    }
    let _ = node;
    Ok(Node::Group(group))
}

/// The same lowering, inside a fixpoint's nested circuit.
fn build_nested(
    built: &[Node<NestedCircuit>],
    node: &crate::typecheck::PlanNode,
) -> Result<Node<NestedCircuit>, anyhow::Error> {
    let dep = |i: usize| -> &Node<NestedCircuit> { &built[i] };
    Ok(operator_arms!(node, dep, &node.op))
}

/// The batch shape of a node, for the JSON codec to know whether a delta has a
/// value half.
pub fn shape<'a>(plan: &'a Plan, name: &str) -> Option<&'a BatchType> {
    plan.node(name).map(|n| &n.ty)
}
