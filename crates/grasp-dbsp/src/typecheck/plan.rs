//! The checked program: what `typecheck` produces and `lower` consumes.

use crate::diag::Span;
use crate::expr::TypedExpr;
use crate::value::BatchType;
use std::collections::HashMap;
use std::sync::Arc;

/// Every operator name. The reserved-word list is built from this, and a test
/// asserts `check_op` accepts each one, so the two cannot drift apart.
pub const OPERATORS: &[&str] = &[
    "input",
    "map",
    "filter",
    "flat_map",
    "map_index",
    "flat_map_index",
    "join",
    "join_index",
    "antijoin",
    "distinct",
    "aggregate",
    "weighted_count",
    "neg",
    "plus",
    "minus",
    "sum",
    "integrate",
    "differentiate",
    "delay",
    "empty",
    "constant",
];

/// Every aggregator name. These appear as bare names in argument position, so a
/// node named `min` would be silently shadowed if they were not reserved.
pub const AGGREGATORS: &[&str] = &["min", "max", "sum", "avg", "count", "argmin", "argmax"];

/// Where the `key` and `value` fields sit in the record an indexing operator's
/// function returns.
///
/// They are resolved by *name*, so writing `record(value: ..., key: ...)` means
/// the same thing — unlike every other record, where the literal's order defines
/// the type. Carrying the two indices is what lets the lowering split the record
/// without caring which order it was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyValue {
    pub key: usize,
    pub value: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Min,
    Max,
    Sum,
    Avg,
    Count,
    // Appended, and any new aggregator must be too: a node's content id hashes
    // the discriminant (`content.rs`), so inserting one earlier would renumber
    // the rest and change the identity of every program that uses them — and
    // with it every checkpoint's program digest.
    /// The `value` from the row whose `by` is smallest, ties to the smallest
    /// `value`.
    ArgMin,
    /// The `value` from the row whose `by` is largest, ties to the smallest
    /// `value`.
    ArgMax,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanOp {
    Input {
        table: String,
        /// The field `partition_as` names, as an index into the input's record
        /// type. The runtime fills it; a pushed row never carries it.
        partition: Option<usize>,
        /// The field `offset_as` names. Only ever set alongside `partition`.
        offset: Option<usize>,
    },
    Map {
        input: usize,
        f: Arc<TypedExpr>,
    },
    Filter {
        input: usize,
        f: Arc<TypedExpr>,
    },
    MapIndex {
        input: usize,
        f: Arc<TypedExpr>,
        kv: KeyValue,
    },
    Join {
        left: usize,
        right: usize,
        f: Arc<TypedExpr>,
    },
    Antijoin {
        left: usize,
        right: usize,
    },
    Distinct {
        input: usize,
    },
    Aggregate {
        input: usize,
        agg: Agg,
        f: Arc<TypedExpr>,
        /// What `f` projects. The lowering needs it to choose between the
        /// linear path and the fold, and `avg`'s *result* cannot say — it is
        /// `f64` whatever it was given.
        projection: crate::value::TypeDesc,
    },
    WeightedCount {
        input: usize,
    },
    Neg {
        input: usize,
    },
    Plus {
        left: usize,
        right: usize,
    },
    Minus {
        left: usize,
        right: usize,
    },
    Sum {
        inputs: Vec<usize>,
    },
    /// One expression per output row: fan-out is fixed by the source, not by
    /// the data.
    /// `f` returns an `array`, so the number of rows emitted per input row
    /// follows the data rather than being fixed by the source.
    FlatMap {
        input: usize,
        f: Arc<TypedExpr>,
    },
    FlatMapIndex {
        input: usize,
        f: Arc<TypedExpr>,
        kv: KeyValue,
    },
    JoinIndex {
        left: usize,
        right: usize,
        f: Arc<TypedExpr>,
        kv: KeyValue,
    },
    Integrate {
        input: usize,
    },
    Differentiate {
        input: usize,
    },
    Delay {
        input: usize,
    },
    /// An empty stream. Its type is fixed by where it is used.
    Empty,

    /// A stream holding exactly these rows.
    ///
    /// The rows are delivered in the first transaction and the stream is zero
    /// thereafter, so `integrate(constant(X))` is `X` at every transaction —
    /// which is the sense in which the *relation* is constant. The non-zero
    /// sibling of [`PlanOp::Empty`].
    ///
    /// Already evaluated: the argument is a closed expression, checked and
    /// folded during typechecking, so the plan carries values rather than an
    /// expression nothing could supply a row to.
    Constant {
        rows: Arc<Vec<crate::value::DynValue>>,
    },

    /// Iterate a circuit body to convergence.
    ///
    /// The body is a sub-plan built inside a nested circuit, so this is the one
    /// place the node list stops being flat. It yields one stream per recursive
    /// parameter; `FixpointExport` picks them out.
    Fixpoint {
        body: Vec<PlanNode>,
        outputs: Vec<usize>,
    },
    /// One convergent stream of a `Fixpoint` node.
    FixpointExport {
        fixpoint: usize,
        slot: usize,
    },

    /// Body-only: a parent stream imported into the nested circuit (`delta0`).
    Import {
        outer: usize,
    },
    /// Body-only: the previous round's value of recursive slot `slot`.
    RecVar {
        slot: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub name: String,
    pub ty: BatchType,
    pub op: PlanOp,
    /// Where the node was declared, so lowering failures have a location.
    pub span: Span,
}

/// A checked program: nodes in dependency order.
#[derive(Debug, Clone)]
pub struct Plan {
    pub nodes: Vec<PlanNode>,
    pub by_name: HashMap<String, usize>,
}

impl Plan {
    pub fn node(&self, name: &str) -> Option<&PlanNode> {
        self.by_name.get(name).map(|i| &self.nodes[*i])
    }

    /// Input nodes, as `(node index, table name)`.
    pub fn inputs(&self) -> Vec<(usize, &str)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| match &n.op {
                PlanOp::Input { table, .. } => Some((i, table.as_str())),
                _ => None,
            })
            .collect()
    }

    /// The fields the runtime fills in a partitioned input, as indices into its
    /// record type: `(partition, offset)`. `None` for a plain input, or for a
    /// table this program does not have.
    pub fn runtime_fields(&self, table: &str) -> Option<(usize, Option<usize>)> {
        self.nodes.iter().find_map(|n| match &n.op {
            PlanOp::Input {
                table: t,
                partition: Some(p),
                offset,
            } if t == table => Some((*p, *offset)),
            _ => None,
        })
    }

    /// What a row pushed into `table` carries: its declared record, less the
    /// fields the runtime fills. For a plain input that is the declared record.
    pub fn ingress_type(&self, table: &str) -> Option<crate::value::TypeDesc> {
        let node = self
            .nodes
            .iter()
            .find(|n| matches!(&n.op, PlanOp::Input { table: t, .. } if t == table))?;
        let crate::value::BatchType::ZSet(crate::value::TypeDesc::Record(fields)) = &node.ty else {
            return None;
        };
        let runtime = self.runtime_fields(table);
        Some(crate::value::TypeDesc::Record(
            fields
                .iter()
                .enumerate()
                .filter(|(i, _)| !runtime.is_some_and(|(p, o)| *i == p || Some(*i) == o))
                .map(|(_, f)| f.clone())
                .collect(),
        ))
    }

    /// Every declared name that may be selected as an output, sorted.
    ///
    /// A server that exposes "every view this program has" needs the list. The
    /// body is `by_name`'s keys and nothing else, which is worth a method
    /// rather than a field access for one reason: **that it needs no filter is
    /// a fact about the checker, not an obvious truth**, and this is where the
    /// fact is pinned.
    ///
    /// The filter it looks like it needs is for `fixpoint`. A fixpoint instance
    /// is not a stream but a group of them, and `Runner::build` refuses one
    /// with "select one of its recursive nodes" — so if `fp` were a key here,
    /// exposing every key would offer a name that cannot build. It is not a
    /// key: `Fixpoint` nodes are pushed without registering a name, and only
    /// the members are registered, under the dotted `<instance>.<label>` that
    /// `mapping.md` describes. For `fp := fixpoint(tc(…))` the keys are
    /// `fp.path` and whatever else the body labelled, never `fp`.
    /// `tests/plan.rs::a_fixpoint_instance_is_not_a_view` is what keeps that
    /// true, and `::every_view_a_plan_offers_can_be_an_output` is what would
    /// catch any other unbuildable key.
    ///
    /// Sorted so the order is the program's alphabet rather than a `HashMap`'s
    /// iteration, which would make a server's view list — and the output
    /// selection built from it — differ run to run. That is the determinism
    /// obligation `Runner::build` carries, reaching one step further out.
    ///
    /// Nodes with no declared name are **not** here. They are addressable only
    /// by content id (`typecheck::content_ids`), a deliberate asymmetry: a name
    /// is something the program chose to expose, and an id is something a
    /// caller had to go looking for.
    pub fn views(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}
