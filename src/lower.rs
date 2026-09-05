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
    DBSPHandle, NestedCircuit, IndexedZSetReader, OrdIndexedZSet, OrdZSet, OutputHandle, RootCircuit, Runtime,
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
            let mut nodes: Vec<Node<RootCircuit>> = Vec::with_capacity(plan.nodes.len());
            let mut inputs: Vec<(String, ZSetHandle<DynValue>)> = Vec::new();

            for node in &plan.nodes {
                let built = build_root(circuit, &plan, &nodes, node, &mut inputs)?;
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

/// The operator arms shared by every circuit type.
///
/// `dbsp` exposes its operators as inherent methods on concrete circuit types
/// rather than through a trait, so a function generic over `C: Circuit` cannot
/// call them. The lowering is therefore instantiated once per circuit type, and
/// this macro is what keeps the two copies from drifting.
macro_rules! operator_arms {
    ($node:expr, $dep:expr, $op:expr) => {
        match $op {
            PlanOp::Map { input, f } => {
                let f = f.clone();
                Node::Flat($dep(*input).flat($node)?.map(move |r: &DynValue| eval(&f, &[r])))
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

            PlanOp::MapIndex { input, key, value } => {
                let (key, value) = (key.clone(), value.clone());
                Node::Indexed(
                    $dep(*input)
                        .flat($node)?
                        .map_index(move |r: &DynValue| (eval(&key, &[r]), eval(&value, &[r]))),
                )
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

            PlanOp::Aggregate { input, agg, f } => match agg {
                // `Stream::aggregate` has no projection argument: it aggregates over
                // the value directly. So re-project the value first, then aggregate.
                Agg::Min | Agg::Max => {
                    let f = f.clone();
                    let projected = $dep(*input)
                        .indexed($node)?
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
                    Node::Indexed($dep(*input).indexed($node)?.aggregate_linear_postprocess(
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

            PlanOp::FlatMap { input, outputs } => {
                let outputs = outputs.clone();
                Node::Flat($dep(*input).flat($node)?.flat_map(move |r: &DynValue| {
                    outputs.iter().map(|e| eval(e, &[r])).collect::<Vec<_>>()
                }))
            }

            PlanOp::FlatMapIndex { input, pairs } => {
                let pairs = pairs.clone();
                Node::Indexed($dep(*input).flat($node)?.flat_map_index(move |r: &DynValue| {
                    pairs
                        .iter()
                        .map(|(k, v)| (eval(k, &[r]), eval(v, &[r])))
                        .collect::<Vec<_>>()
                }))
            }

            PlanOp::JoinIndex { left, right, key, value } => {
                let (key, value) = (key.clone(), value.clone());
                Node::Indexed($dep(*left).indexed($node)?.join_index(
                    $dep(*right).indexed($node)?,
                    move |k: &DynValue, a: &DynValue, b: &DynValue| {
                        // `join_index` takes an iterator of pairs; ours is always
                        // exactly one, since the body is a single `(key, value)`.
                        std::iter::once((eval(&key, &[k, a, b]), eval(&value, &[k, a, b])))
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
) -> Result<Node<RootCircuit>, anyhow::Error> {
    let _ = plan;
    let dep = |i: usize| -> &Node<RootCircuit> { &built[i] };

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

        PlanOp::Fixpoint { body, outputs } => {
            build_fixpoint(circuit, built, node, body, outputs)?
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
) -> Result<Vec<Node<NestedCircuit>>, Diagnostic> {
    let mut nodes: Vec<Node<NestedCircuit>> = Vec::with_capacity(body.len());
    for bn in body {
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
        nodes.push(built);
    }
    Ok(nodes)
}

fn build_fixpoint(
    circuit: &mut RootCircuit,
    built: &[Node<RootCircuit>],
    node: &crate::typecheck::PlanNode,
    body: &[crate::typecheck::PlanNode],
    outputs: &[usize],
) -> Result<Node<RootCircuit>, anyhow::Error> {
    // The checker guarantees every recursive stream shares one shape, which is
    // what lets a single nested batch type serve them all.
    let flat = matches!(body[outputs[0]].ty, BatchType::ZSet(_));

    // A failure inside the closure means the checker and the lowering disagree.
    // `recursive_dynamic` cannot carry a Diagnostic out, so it is parked here
    // and the closure returns its inputs unchanged to keep the arity right.
    let mut failure: Option<Diagnostic> = None;

    let group = if flat {
        let exports = circuit.recursive_dynamic(
            outputs.len(),
            |child: &NestedCircuit, vars: Vec<Flat<NestedCircuit>>| {
                let wrapped: Vec<_> = vars.iter().cloned().map(Node::Flat).collect();
                match build_body(child, &wrapped, body, built) {
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
                let wrapped: Vec<_> = vars.iter().cloned().map(Node::Indexed).collect();
                match build_body(child, &wrapped, body, built) {
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
