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
use crate::value::{Acc, BatchType, DynValue};
use dbsp::algebra::F64;
use dbsp::operator::{Generator, Max, Min};
use dbsp::Circuit;
use dbsp::{
    DBSPHandle, IndexedZSetReader, OrdIndexedZSet, OrdZSet, OutputHandle, RootCircuit, Runtime,
    Stream, ZSetHandle, ZWeight,
};
use std::collections::HashMap;
use std::fmt;

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

type Flat = Stream<RootCircuit, OrdZSet<DynValue>>;
type Indexed = Stream<RootCircuit, OrdIndexedZSet<DynValue, DynValue>>;

/// A lowered node. The two shapes correspond exactly to the language's two
/// batch types.
#[derive(Clone)]
enum Node {
    Flat(Flat),
    Indexed(Indexed),
}

impl Node {
    fn flat(&self, node: &crate::typecheck::PlanNode) -> Result<&Flat, Diagnostic> {
        match self {
            Node::Flat(s) => Ok(s),
            Node::Indexed(_) => Err(shape_error(node, "a flat zset")),
        }
    }
    fn indexed(&self, node: &crate::typecheck::PlanNode) -> Result<&Indexed, Diagnostic> {
        match self {
            Node::Indexed(s) => Ok(s),
            Node::Flat(_) => Err(shape_error(node, "an indexed_zset")),
        }
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
enum Out {
    Flat(OutputHandle<OrdZSet<DynValue>>),
    Indexed(OutputHandle<OrdIndexedZSet<DynValue, DynValue>>),
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
    pub fn build(plan: &Plan, outputs: &[String]) -> Result<Runner, Vec<Diagnostic>> {
        for name in outputs {
            if !plan.by_name.contains_key(name) {
                return Err(vec![Diagnostic::error(
                    Pass::Lower,
                    None,
                    format!("no node named `{name}` to output"),
                )]);
            }
        }

        let plan = plan.clone();
        let wanted = outputs.to_vec();

        // `Runtime::init_circuit` runs this closure once per worker and asserts
        // the circuits match, so it must be deterministic: nodes are visited in
        // plan order and nothing here iterates a hash map.
        let (dbsp, (inputs, outs)) = Runtime::init_circuit(1, move |circuit| {
            let mut nodes: Vec<Node> = Vec::with_capacity(plan.nodes.len());
            let mut inputs: Vec<(String, ZSetHandle<DynValue>)> = Vec::new();

            for node in &plan.nodes {
                let built = build_node(circuit, &plan, &nodes, node, &mut inputs)?;
                nodes.push(built);
            }

            let mut outs: Vec<(String, Out)> = Vec::new();
            for name in &wanted {
                let idx = plan.by_name[name];
                outs.push((
                    name.clone(),
                    match &nodes[idx] {
                        Node::Flat(s) => Out::Flat(s.output()),
                        Node::Indexed(s) => Out::Indexed(s.output()),
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
                vec![Diagnostic::error(Pass::Lower, None, format!("building the circuit: {e}"))]
            };
            match e {
                dbsp::Error::Constructor(any) => match any.downcast::<Diagnostic>() {
                    Ok(diag) => vec![diag],
                    Err(other) => fallback(&other),
                },
                other => fallback(&other),
            }
        })?;

        Ok(Runner { dbsp, inputs: inputs.into_iter().collect(), outputs: outs })
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
                    .consolidate()
                    .iter()
                    .map(|(k, (), w)| Delta { key: k, value: None, weight: w })
                    .collect(),
                Out::Indexed(h) => h
                    .consolidate()
                    .iter()
                    .map(|(k, v, w)| Delta { key: k, value: Some(v), weight: w })
                    .collect(),
            };
            out.push((name.clone(), deltas));
        }
        Ok(out)
    }

    pub fn kill(self) {
        let _ = self.dbsp.kill();
    }
}

fn build_node(
    circuit: &mut RootCircuit,
    plan: &Plan,
    built: &[Node],
    node: &crate::typecheck::PlanNode,
    inputs: &mut Vec<(String, ZSetHandle<DynValue>)>,
) -> Result<Node, anyhow::Error> {
    let _ = plan;
    let dep = |i: usize| -> &Node { &built[i] };

    Ok(match &node.op {
        // A source that always yields the zero batch. `HasZero` on `TypedBatch`
        // is what makes this expressible; the erased batch types have no zero
        // without factories.
        PlanOp::Empty => match &node.ty {
            BatchType::ZSet(_) => Node::Flat(
                circuit.add_source(Generator::new(dbsp::algebra::HasZero::zero)),
            ),
            BatchType::IndexedZSet(..) => Node::Indexed(
                circuit.add_source(Generator::new(dbsp::algebra::HasZero::zero)),
            ),
        },

        PlanOp::Input { table } => {
            let (stream, handle) = circuit.add_input_zset::<DynValue>();
            inputs.push((table.clone(), handle));
            Node::Flat(stream)
        }

        PlanOp::Map { input, f } => {
            let f = f.clone();
            Node::Flat(dep(*input).flat(node)?.map(move |r: &DynValue| eval(&f, &[r])))
        }

        PlanOp::Filter { input, f } => {
            let f = f.clone();
            match dep(*input) {
                Node::Flat(s) => {
                    Node::Flat(s.filter(move |r: &DynValue| is_true(&eval(&f, &[r]))))
                }
                // An indexed stream's element is the (key, value) pair.
                Node::Indexed(s) => Node::Indexed(
                    s.filter(move |(k, v): (&DynValue, &DynValue)| is_true(&eval(&f, &[k, v]))),
                ),
            }
        }

        PlanOp::MapIndex { input, key, value } => {
            let (key, value) = (key.clone(), value.clone());
            Node::Indexed(
                dep(*input)
                    .flat(node)?
                    .map_index(move |r: &DynValue| (eval(&key, &[r]), eval(&value, &[r]))),
            )
        }

        PlanOp::Join { left, right, f } => {
            let f = f.clone();
            Node::Flat(dep(*left).indexed(node)?.join(
                dep(*right).indexed(node)?,
                move |k: &DynValue, a: &DynValue, b: &DynValue| eval(&f, &[k, a, b]),
            ))
        }

        PlanOp::Antijoin { left, right } => Node::Indexed(
            dep(*left)
                .indexed(node)?
                .antijoin(dep(*right).indexed(node)?),
        ),

        PlanOp::Distinct { input } => match dep(*input) {
            Node::Flat(s) => Node::Flat(s.distinct()),
            Node::Indexed(s) => Node::Indexed(s.distinct()),
        },

        PlanOp::Aggregate { input, agg, f } => match agg {
            // `Stream::aggregate` has no projection argument: it aggregates over
            // the value directly. So re-project the value first, then aggregate.
            Agg::Min | Agg::Max => {
                let f = f.clone();
                let projected = dep(*input)
                    .indexed(node)?
                    .map_index(move |(k, v): (&DynValue, &DynValue)| (k.clone(), eval(&f, &[v])));
                Node::Indexed(match agg {
                    Agg::Min => projected.aggregate(Min),
                    _ => projected.aggregate(Max),
                })
            }

            // `sum`, `avg` and `count` are linear: each row contributes
            // independently, scaled by its weight, which is what lets `dbsp`
            // maintain them without replaying the group.
            Agg::Sum | Agg::Avg | Agg::Count => {
                let proj = f.clone();
                let agg = *agg;
                Node::Indexed(dep(*input).indexed(node)?.aggregate_linear_postprocess(
                    move |v: &DynValue| match eval(&proj, &[v]) {
                        DynValue::I64(n) => Acc::value(n),
                        // A `NONE` projection contributes to no sum and to no
                        // count, which is what separates `count` from
                        // `weighted_count`. The type checker rejects a
                        // floating-point projection, so nothing else arrives.
                        _ => Acc::none(),
                    },
                    move |acc: Acc| match agg {
                        Agg::Count => DynValue::I64(acc.rows),
                        _ if acc.rows == 0 => DynValue::None,
                        Agg::Sum => DynValue::I64(acc.sum),
                        // The division happens here, not in the accumulator, so
                        // integer inputs still give a fractional mean.
                        Agg::Avg => DynValue::F64(F64::new(acc.sum as f64 / acc.rows as f64)),
                        Agg::Min | Agg::Max => unreachable!("handled above"),
                    },
                ))
            }
        },

        PlanOp::WeightedCount { input } => {
            // `weighted_count` yields `OrdIndexedZSet<K, ZWeight>` — the value is
            // a raw i64, off the uniform shape — so box it back into a DynValue.
            let counted = dep(*input).flat(node)?.weighted_count();
            Node::Indexed(
                counted.map_index(|(k, w): (&DynValue, &ZWeight)| (k.clone(), DynValue::I64(*w))),
            )
        }

        PlanOp::Neg { input } => match dep(*input) {
            Node::Flat(s) => Node::Flat(s.neg()),
            Node::Indexed(s) => Node::Indexed(s.neg()),
        },

        PlanOp::Plus { left, right } => match (dep(*left), dep(*right)) {
            (Node::Flat(a), Node::Flat(b)) => Node::Flat(a.plus(b)),
            (Node::Indexed(a), Node::Indexed(b)) => Node::Indexed(a.plus(b)),
            _ => return Err(shape_error(node, "matching shapes in `plus`").into()),
        },

        PlanOp::Minus { left, right } => match (dep(*left), dep(*right)) {
            (Node::Flat(a), Node::Flat(b)) => Node::Flat(a.minus(b)),
            (Node::Indexed(a), Node::Indexed(b)) => Node::Indexed(a.minus(b)),
            _ => return Err(shape_error(node, "matching shapes in `minus`").into()),
        },

        PlanOp::FlatMap { input, outputs } => {
            let outputs = outputs.clone();
            Node::Flat(dep(*input).flat(node)?.flat_map(move |r: &DynValue| {
                outputs.iter().map(|e| eval(e, &[r])).collect::<Vec<_>>()
            }))
        }

        PlanOp::FlatMapIndex { input, pairs } => {
            let pairs = pairs.clone();
            Node::Indexed(dep(*input).flat(node)?.flat_map_index(move |r: &DynValue| {
                pairs
                    .iter()
                    .map(|(k, v)| (eval(k, &[r]), eval(v, &[r])))
                    .collect::<Vec<_>>()
            }))
        }

        PlanOp::JoinIndex { left, right, key, value } => {
            let (key, value) = (key.clone(), value.clone());
            Node::Indexed(dep(*left).indexed(node)?.join_index(
                dep(*right).indexed(node)?,
                move |k: &DynValue, a: &DynValue, b: &DynValue| {
                    // `join_index` takes an iterator of pairs; ours is always
                    // exactly one, since the body is a single `(key, value)`.
                    std::iter::once((eval(&key, &[k, a, b]), eval(&value, &[k, a, b])))
                },
            ))
        }

        PlanOp::Integrate { input } => match dep(*input) {
            Node::Flat(s) => Node::Flat(s.integrate()),
            Node::Indexed(s) => Node::Indexed(s.integrate()),
        },

        PlanOp::Differentiate { input } => match dep(*input) {
            Node::Flat(s) => Node::Flat(s.differentiate()),
            Node::Indexed(s) => Node::Indexed(s.differentiate()),
        },

        PlanOp::Delay { input } => match dep(*input) {
            Node::Flat(s) => Node::Flat(s.delay()),
            Node::Indexed(s) => Node::Indexed(s.delay()),
        },

        PlanOp::Sum { inputs: ins } => {
            let head = dep(ins[0]);
            match head {
                Node::Flat(first) => {
                    let rest: Vec<&Flat> = ins[1..]
                        .iter()
                        .map(|i| dep(*i).flat(node))
                        .collect::<Result<_, _>>()?;
                    Node::Flat(first.sum(rest))
                }
                Node::Indexed(first) => {
                    let rest: Vec<&Indexed> = ins[1..]
                        .iter()
                        .map(|i| dep(*i).indexed(node))
                        .collect::<Result<_, _>>()?;
                    Node::Indexed(first.sum(rest))
                }
            }
        }
    })
}

/// The batch shape of a node, for the JSON codec to know whether a delta has a
/// value half.
pub fn shape<'a>(plan: &'a Plan, name: &str) -> Option<&'a BatchType> {
    plan.node(name).map(|n| &n.ty)
}
