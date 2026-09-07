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
];

/// Every aggregator name. These appear as bare names in argument position, so a
/// node named `min` would be silently shadowed if they were not reserved.
pub const AGGREGATORS: &[&str] = &["min", "max", "sum", "avg", "count"];

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
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanOp {
    Input {
        table: String,
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
                PlanOp::Input { table } => Some((i, table.as_str())),
                _ => None,
            })
            .collect()
    }
}
