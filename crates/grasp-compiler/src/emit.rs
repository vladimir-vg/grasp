//! Emit — `docs/grasp/mapping.md`.
//!
//! Turns a [`crate::plan::Plan`] into grasp-dbsp text. This is the only place
//! the two languages meet, and the only place that knows grasp-dbsp's spelling
//! of anything.
//!
//! Three things about this pass are load-bearing.
//!
//! **It reads a node's content and its position, and nothing else.** That is
//! what makes `compilation.md`'s structural key sufficient: two plans with
//! equal keys emit identical text, so a tie the key settled stays settled here.
//! Nothing in this file looks at a span.
//!
//! **It is not [`crate::key`].** That module renders grasp for comparison; this
//! one renders grasp-dbsp for a file, resolves variables against the row
//! binder, and undoes desugaring — grasp-dbsp has no `record:get`, no
//! `dict:get` and no `boolean:not`, those being names in grasp's reserved
//! namespaces rather than callables anything implements. The two share
//! [`crate::key::float`], because `1.0` must not render as `1` in either and a
//! disagreement between them would surface as a normalization failure nobody
//! could read.
//!
//! **Every expression is fully parenthesised.** grasp and grasp-dbsp specify
//! precedence independently, and nothing holds them together; parenthesising
//! costs nothing and removes the question. It is a function of the plan, so
//! two equivalent programs still agree byte for byte.

use crate::ast::{BinOp, Lit, Type, UnOp};
use crate::core;
use crate::key;
use crate::plan::{Field, Group, Node, Op, Plan, Relation, Rule, Source};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// The row binder every emitted function takes.
///
/// One name for all of them: functions never nest, so it can never shadow.
const ROW: &str = "row";

/// Emit a planned program.
pub fn emit(plan: &Plan) -> String {
    let mut names = Names::new(plan);
    let mut out = String::new();
    for (i, group) in plan.groups.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if group.recursive {
            emit_fixpoint(&mut out, group, &mut names);
        } else {
            for relation in &group.relations {
                emit_relation(&mut out, relation, &mut names);
            }
        }
    }
    out
}

/// One strongly connected component: a `circuit` holding its relations, and the
/// `fixpoint` that iterates it to convergence.
///
/// Three rules from `mapping.md`, all easy to get wrong and none of them
/// visible in the answer if they are:
///
/// 1. **The recursive stream's typespec is always written.** grasp-dbsp infers
///    a recursive type only through operators whose result type is one of their
///    operands, and a Datalog rule ends in a `map`, which is not one of those.
/// 2. **No `distinct` inside.** grasp-dbsp applies one to every recursive
///    stream on every round — that is what makes the iteration terminate — so
///    the union-of-rules rule has an exception here.
/// 3. **Recursive streams start empty.** The call site passes `empty()` and the
///    base case is in the body, as one of the summed rules.
///
/// And a fourth thing, which is why facts are emitted before the circuit rather
/// than inside it: grasp-dbsp rejects a `constant` in a `fixpoint` body, where
/// a source would fire once per *iteration* rather than once per transaction.
/// It goes outside and comes in as an ordinary parameter — which is what an
/// input relation already does, so it needs no special case.
fn emit_fixpoint(out: &mut String, group: &Group, names: &mut Names) {
    let first = names.node_of(&group.relations[0].name);
    let circuit = names.reserve(&format!("{first}_scc"));
    let instance = names.reserve(&format!("{first}_fp"));

    // `label: internal`. A parameter is self-referential when a body node
    // shares its *label*, so a recursive member's label is its node name and
    // its internal name — the previous round's value — has to be another.
    let mut params: Vec<(String, String, String)> = Vec::new();
    for read in &group.reads {
        let node = names.node_of(read);
        let internal = names.reserve(&format!("{node}_in"));
        params.push((node.clone(), internal, node));
    }
    let mut fact_param: BTreeMap<String, String> = BTreeMap::new();
    for relation in &group.relations {
        let Source::Derived { facts, .. } = &relation.source else {
            continue;
        };
        if facts.is_empty() {
            continue;
        }
        let node = names.node_of(&relation.name);
        let outside = names.intermediate(&node);
        let _ = writeln!(out, "{outside} :: {}", zset(&relation.columns));
        let rows: Vec<String> = facts
            .iter()
            .map(|row| fact_row(row, &relation.columns))
            .collect();
        let _ = writeln!(out, "{outside} := constant([{}])", rows.join(", "));
        let label = names.reserve(&format!("{node}_facts"));
        let internal = names.reserve(&format!("{label}_in"));
        fact_param.insert(relation.name.clone(), internal.clone());
        params.push((label, internal, outside));
    }
    let mut recursive: Vec<(String, String)> = Vec::new();
    for relation in &group.relations {
        let node = names.node_of(&relation.name);
        let internal = names.reserve(&format!("{node}_prev"));
        recursive.push((relation.name.clone(), node.clone()));
        params.push((node, internal, "empty()".to_string()));
    }

    // Inside the body a relation name means the parameter that carries it —
    // the previous round for a member, the outer stream for anything else.
    for (label, internal, _) in &params {
        names.scope.insert(label.clone(), internal.clone());
    }
    let mut body = String::new();
    for relation in &group.relations {
        let node = names.node_of_unscoped(&relation.name);
        let _ = writeln!(&mut body, "{node} :: {}", zset(&relation.columns));
        let Source::Derived { rules, .. } = &relation.source else {
            unreachable!("a recursive relation is derived by rules")
        };
        let mut operands: Vec<String> = Vec::new();
        if let Some(p) = fact_param.get(&relation.name) {
            operands.push(p.clone());
        }
        for rule in rules {
            operands.push(emit_rule(&mut body, relation, rule, names));
        }
        let union = match operands.len() {
            1 => operands.pop().expect("one operand"),
            2 => format!("plus({}, {})", operands[0], operands[1]),
            _ => format!("sum({})", operands.join(", ")),
        };
        // No `distinct`: grasp-dbsp applies one every round already.
        let _ = writeln!(&mut body, "{node} := {union}");
    }
    names.scope.clear();

    let decl: Vec<String> = params
        .iter()
        .map(|(label, internal, _)| format!("{label}: {internal}"))
        .collect();
    let _ = writeln!(out, "circuit {circuit}({}) {{", decl.join(", "));
    out.push_str(&indent(&body));
    let _ = writeln!(out, "}}");

    let args: Vec<String> = params
        .iter()
        .map(|(label, _, arg)| format!("{label}: {arg}"))
        .collect();
    let _ = writeln!(
        out,
        "{instance} := fixpoint({circuit}({}))",
        args.join(", ")
    );
    // "Only recursive members leave the fixpoint", read off as `fp.name`.
    for (_, node) in &recursive {
        let _ = writeln!(out, "{node} := {instance}.{node}");
    }
}

fn indent(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let _ = writeln!(&mut out, "    {line}");
    }
    out
}

fn emit_relation(out: &mut String, relation: &Relation, names: &mut Names) {
    let node = names.node_of(&relation.name);
    let node = &node;
    let ty = zset(&relation.columns);
    match &relation.source {
        // "The typespec must be written, not inferred: grasp-dbsp takes an
        //  `input` node's schema from its `::` annotation."
        Source::Input => {
            let _ = writeln!(out, "{node} :: {ty}");
            let _ = writeln!(out, "{node} := input({:?})", relation.name);
        }
        // "A relation with a typespec and no producer is empty for the life of
        //  the program, and emits as a standalone `empty()`." Its typespec is
        //  required for the same reason: standing alone, the `::` is the only
        //  thing that can give `empty()` a type.
        Source::Empty => {
            let _ = writeln!(out, "{node} :: {ty}");
            let _ = writeln!(out, "{node} := empty()");
        }
        Source::Derived { facts, rules } => {
            let mut operands: Vec<String> = Vec::new();
            if !facts.is_empty() {
                // "The facts of one relation collect into a single `constant`."
                // It is the relation's own node when nothing else defines the
                // relation, and an operand of the union otherwise — either way
                // it needs a name and a typespec, because `constant` does not
                // nest and takes its type from a `::`.
                let name = if rules.is_empty() {
                    node.clone()
                } else {
                    names.intermediate(node)
                };
                let rows: Vec<String> = facts
                    .iter()
                    .map(|row| fact_row(row, &relation.columns))
                    .collect();
                let _ = writeln!(out, "{name} :: {ty}");
                let _ = writeln!(out, "{name} := constant([{}])", rows.join(", "));
                if rules.is_empty() {
                    // No `distinct`: a fact's rows are written out once and
                    // cannot repeat, so there is nothing to deduplicate.
                    return;
                }
                operands.push(name);
            }
            for rule in rules {
                operands.push(emit_rule(out, relation, rule, names));
            }
            // "Several rules for one relation are summed and then
            //  deduplicated", `plus` for two and n-ary `sum` beyond.
            let union = match operands.len() {
                1 => operands.pop().expect("one operand"),
                2 => format!("plus({}, {})", operands[0], operands[1]),
                _ => format!("sum({})", operands.join(", ")),
            };
            // `distinct` wraps a rule-derived definition and nothing else: a
            // relation is a set, and `plus` adds weights.
            let _ = writeln!(out, "{node} := distinct({union})");
        }
    }
}

/// One rule's nodes. Returns the name of the last, which is the rule's
/// contribution to its relation.
///
/// Nodes reference each other by index, so this keeps a name per index and
/// resolves as it goes. A `Scan` contributes no definition — it *is* the
/// relation's stream, already named.
fn emit_rule(out: &mut String, relation: &Relation, rule: &Rule, names: &mut Names) -> String {
    // The relation's own name, not what it is called inside a circuit: an
    // intermediate belongs to the relation whose rule it serves, and naming it
    // after the parameter carrying the previous round would say otherwise.
    let base = names.node_of_unscoped(&relation.name);
    let mut at: Vec<String> = Vec::new();
    for node in &rule.nodes {
        let name = match &node.op {
            Op::Scan { relation } => {
                at.push(names.node_of(relation));
                continue;
            }
            Op::Ground => {
                // "A rule with no positive atom is grounded on the unit
                //  relation, which is a `constant` holding the one empty row."
                //  It has to be named, because `constant` does not nest.
                let name = names.intermediate(&base);
                let _ = writeln!(out, "{name} :: zset(record())");
                let _ = writeln!(out, "{name} := constant([record()])");
                name
            }
            Op::Filter { input, expr } => {
                let name = names.intermediate(&base);
                let _ = writeln!(
                    out,
                    "{name} := filter({}, function(({ROW}) -> {}))",
                    at[*input],
                    expr_text(expr, None)
                );
                name
            }
            Op::Map { input, fields } => {
                let name = names.intermediate(&base);
                let _ = writeln!(
                    out,
                    "{name} := map({}, function(({ROW}) -> {}))",
                    at[*input],
                    record(fields)
                );
                name
            }
            Op::MapIndex { input, key, val } => {
                let name = names.intermediate(&base);
                let _ = writeln!(
                    out,
                    "{name} := map_index({}, function(({ROW}) -> record(key: {}, value: {})))",
                    at[*input],
                    row_record(key),
                    row_record(val)
                );
                name
            }
            Op::Join { left, right } => {
                emit_join(out, &base, names, rule, &at, *left, *right, &node.schema)
            }
            Op::Antijoin { left, right } => {
                emit_antijoin(out, &base, names, rule, &at, *left, *right, &node.schema)
            }
            Op::Narrow {
                input,
                name: v,
                fallback,
            } => emit_narrow(out, &base, names, node, v, fallback, &at[*input]),
        };
        at.push(name);
    }
    at.pop().expect("a rule has at least one node")
}

/// A `join`, and the two sides' `map_index` calls have already been emitted.
///
/// The output record is assembled from three places: a key variable comes from
/// the key parameter, and every other from whichever side carried it. The two
/// sides cannot disagree, because a join takes *all* the variables they share
/// as its key — so nothing is left in both values.
#[allow(clippy::too_many_arguments)]
fn emit_join(
    out: &mut String,
    base: &str,
    names: &mut Names,
    rule: &Rule,
    at: &[String],
    left: usize,
    right: usize,
    schema: &BTreeSet<String>,
) -> String {
    let key = match &rule.nodes[left].op {
        Op::MapIndex { key, .. } => key.clone(),
        _ => unreachable!("a join reads two indexed streams"),
    };
    let side = |i: usize| match &rule.nodes[i].op {
        Op::MapIndex { val, .. } => val.clone(),
        _ => unreachable!("a join reads two indexed streams"),
    };
    let (l_val, r_val) = (side(left), side(right));
    let fields: Vec<(String, String)> = schema
        .iter()
        .map(|v| {
            let from = if key.contains(v) {
                format!("k.{v}")
            } else if l_val.contains(v) {
                format!("a.{v}")
            } else {
                debug_assert!(r_val.contains(v), "a join's output comes from its inputs");
                format!("b.{v}")
            };
            (v.clone(), from)
        })
        .collect();
    let name = names.intermediate(base);
    let _ = writeln!(
        out,
        "{name} := join({}, {}, function((k, a, b) -> {}))",
        at[left],
        at[right],
        record_text(&fields)
    );
    name
}

/// An `antijoin`, and the `map` that flattens what it produces.
///
/// "The `map` after the `antijoin` is not optional: `antijoin` yields an
/// indexed stream, and a relation is flat." So this is two operators under one
/// node, and the flattening function takes the key and the value rather than a
/// row — which is what an indexed stream hands a function.
///
/// The rows that survive are the left's, so every output field comes from the
/// left: the key parameter for what both sides were indexed by, the value for
/// the rest. The right side carries no value at all — an antijoin reads only
/// whether a key is there.
#[allow(clippy::too_many_arguments)]
fn emit_antijoin(
    out: &mut String,
    base: &str,
    names: &mut Names,
    rule: &Rule,
    at: &[String],
    left: usize,
    right: usize,
    schema: &BTreeSet<String>,
) -> String {
    let key = match &rule.nodes[left].op {
        Op::MapIndex { key, .. } => key.clone(),
        _ => unreachable!("an antijoin reads two indexed streams"),
    };
    let subtracted = names.intermediate(base);
    let _ = writeln!(out, "{subtracted} := antijoin({}, {})", at[left], at[right]);
    let fields: Vec<(String, String)> = schema
        .iter()
        .map(|v| {
            let from = if key.contains(v) {
                format!("k.{v}")
            } else {
                format!("v.{v}")
            };
            (v.clone(), from)
        })
        .collect();
    let name = names.intermediate(base);
    let _ = writeln!(
        out,
        "{name} := map({subtracted}, function((k, v) -> {}))",
        record_text(&fields)
    );
    name
}

/// A record of variables read straight off the row — a `map_index`'s key or
/// value, which are always projections and never computations.
///
/// A key with no variables is `record()`, which is the unit key a cross product
/// joins on: every row of one side meets every row of the other.
fn row_record(vars: &[String]) -> String {
    let fields: Vec<(String, String)> = vars
        .iter()
        .map(|v| (v.clone(), format!("{ROW}.{v}")))
        .collect();
    record_text(&fields)
}

/// The tail of `mapping.md`'s narrowing: drop the rows with no value, then
/// rebind the variable as a definite one.
///
/// Two operators, and the filter must be above the `coalesce` — without it the
/// `coalesce` would invent a value for a row that has none, which is exactly
/// the row a zero divisor produces and exactly the row grasp says must not
/// appear.
fn emit_narrow(
    out: &mut String,
    base: &str,
    names: &mut Names,
    node: &Node,
    v: &str,
    fallback: &core::Expr,
    previous: &str,
) -> String {
    let present = names.intermediate(base);
    let _ = writeln!(
        out,
        "{present} := filter({previous}, function(({ROW}) -> ({ROW}.{v} != NONE)))"
    );
    let fields: Vec<(String, String)> = node
        .schema
        .iter()
        .map(|f| {
            let value = if f == v {
                format!("coalesce({ROW}.{v}, {})", expr_text(fallback, None))
            } else {
                format!("{ROW}.{f}")
            };
            (f.clone(), value)
        })
        .collect();
    let name = names.intermediate(base);
    let _ = writeln!(
        out,
        "{name} := map({present}, function(({ROW}) -> {}))",
        record_text(&fields)
    );
    name
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Node names, and the guarantee that none of them collide.
///
/// The relation names come first and are fixed, because `mapping.md` says the
/// node named `r` *is* relation `r`'s stream. Intermediates are then numbered
/// per relation, in plan order — a function of the program rather than of the
/// file, which is what `equivalent_to` requires.
struct Names {
    taken: BTreeSet<String>,
    /// Every relation's grasp name to the node name it got.
    nodes: Vec<(String, String)>,
    /// Node name to the name it goes by inside the circuit being emitted.
    scope: BTreeMap<String, String>,
    next: usize,
}

/// What grasp-dbsp will not accept as an identifier — `language.md`, "Reserved
/// words". grasp reserves a different set: it bars its own builtins from naming
/// a relation but says nothing about `map` or `join`, so the two lists have to
/// be reconciled here rather than assumed to agree.
const RESERVED: &[&str] = &[
    "input",
    "constant",
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
    "min",
    "max",
    "count",
    "avg",
    "coalesce",
    "if",
    "abs",
    "floor",
    "ceil",
    "round",
    "length",
    "concat",
    "lower",
    "upper",
    "trim",
    "get",
    "keys",
    "entries",
    "cast",
    "bool",
    "i64",
    "f64",
    "string",
    "json",
    "optional",
    "record",
    "array",
    "dict",
    "sql",
    "zset",
    "indexed_zset",
    "true",
    "false",
    "NONE",
    "null",
    "function",
    "return",
    "and",
    "or",
    "not",
    "circuit",
    "fixpoint",
];

impl Names {
    fn new(plan: &Plan) -> Names {
        let mut taken = BTreeSet::new();
        let mut nodes = Vec::new();
        for relation in plan.groups.iter().flat_map(|g| &g.relations) {
            let mut name = mangle(&relation.name);
            while !taken.insert(name.clone()) {
                name.push('_');
            }
            nodes.push((relation.name.clone(), name));
        }
        Names {
            taken,
            nodes,
            scope: BTreeMap::new(),
            next: 0,
        }
    }

    /// The node a relation's stream is called *here*.
    ///
    /// Inside a `circuit` body that is the parameter carrying it, which for a
    /// recursive member is the previous round's value. Outside, it is the
    /// relation's own node.
    fn node_of(&self, relation: &str) -> String {
        let node = self.node_of_unscoped(relation);
        self.scope.get(&node).cloned().unwrap_or(node)
    }

    fn node_of_unscoped(&self, relation: &str) -> String {
        self.nodes
            .iter()
            .find(|(name, _)| name == relation)
            .map(|(_, node)| node.clone())
            .expect("every relation a rule scans is declared")
    }

    /// Claim a name built from `base`, lengthening it until it is free.
    fn reserve(&mut self, base: &str) -> String {
        let mut name = base.to_string();
        while !self.taken.insert(name.clone()) {
            name.push('_');
        }
        name
    }

    fn intermediate(&mut self, base: &str) -> String {
        loop {
            self.next += 1;
            let name = format!("{base}_{}", self.next);
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
    }
}

/// A relation name grasp-dbsp can spell.
///
/// grasp admits a namespace-qualified name and permits names grasp-dbsp
/// reserves; grasp-dbsp identifiers are `[A-Za-z_][A-Za-z0-9_]*`. Only the node
/// name moves — the `input(...)` table string stays the relation name exactly,
/// because that is what anything driving a compiled program keys on.
fn mangle(relation: &str) -> String {
    let mut out: String = relation
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if RESERVED.contains(&out.as_str()) || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.push('_');
    }
    out
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A relation's batch type: `relation(col: T, …)` → `zset(record(col: T, …))`.
fn zset(columns: &[(String, Type)]) -> String {
    let fields: Vec<String> = columns
        .iter()
        .map(|(name, ty)| format!("{name}: {}", ty_text(ty)))
        .collect();
    format!("zset(record({}))", fields.join(", "))
}

/// The mapping is the identity but for one rename, and `mapping.md` says so:
/// the type constructors were named to correspond.
fn ty_text(ty: &Type) -> String {
    match ty {
        Type::Boolean => "bool".to_string(),
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::String => "string".to_string(),
        Type::Json => "json".to_string(),
        Type::Optional(t) => format!("optional({})", ty_text(t)),
        Type::Array(t) => format!("array({})", ty_text(t)),
        Type::Dict(k, v) => format!("dict({}, {})", ty_text(k), ty_text(v)),
        Type::Record(fields) => {
            // Sorted, as `ast::Type`'s own rendering sorts them: a record's
            // fields are a set in both languages.
            let mut sorted: Vec<&(String, Type)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = sorted
                .iter()
                .map(|(name, t)| format!("{name}: {}", ty_text(t)))
                .collect();
            format!("record({})", text.join(", "))
        }
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// A record literal built from plan fields, sorted by name.
fn record(fields: &[Field]) -> String {
    let mut sorted: Vec<&Field> = fields.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let text: Vec<(String, String)> = sorted
        .iter()
        .map(|f| (f.name.clone(), expr_text(&f.value, f.ty.as_ref())))
        .collect();
    record_text(&text)
}

/// One fact, as a row of the relation's `constant`.
///
/// The column types come from the relation rather than from the plan: a fact
/// has no rule to have settled anything, and its arguments are closed — which
/// is exactly when `NONE` and the empty containers have nothing else to take a
/// type from.
fn fact_row(row: &[(String, core::Expr)], columns: &[(String, Type)]) -> String {
    let mut sorted: Vec<&(String, core::Expr)> = row.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let text: Vec<(String, String)> = sorted
        .iter()
        .map(|(name, e)| {
            let ty = columns.iter().find(|(c, _)| c == name).map(|(_, t)| t);
            (name.clone(), expr_text(e, ty))
        })
        .collect();
    record_text(&text)
}

fn record_text(fields: &[(String, String)]) -> String {
    let text: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    format!("record({})", text.join(", "))
}

/// One expression, against the row binder.
///
/// `want` is the type the expression's *context* requires, where there is one.
/// It exists for three values that are complete but open — `NONE`, `[]` and
/// `{}` — which carry no type of their own and which grasp-dbsp will not accept
/// without being told what they hold. grasp settles them by inference; the
/// emitter has to say the answer out loud, and `cast` is how grasp-dbsp hears
/// it. Everywhere else `want` is threaded through and never used.
pub fn expr_text(e: &core::Expr, want: Option<&Type>) -> String {
    match e {
        core::Expr::Lit {
            value: Lit::None, ..
        } => match want {
            Some(t) => format!("cast(NONE, {})", ty_text(t)),
            None => "NONE".to_string(),
        },
        core::Expr::Lit { value, .. } => lit_text(value),
        // Every free variable of a rule body is a field of the row the previous
        // node handed on. `plan` guarantees it is there.
        core::Expr::Var { name, .. } => format!("{ROW}.{name}"),
        core::Expr::Unary { op, operand, .. } => match op {
            UnOp::Neg => format!("(-{})", expr_text(operand, None)),
            UnOp::Not => format!("(not {})", expr_text(operand, None)),
        },
        core::Expr::Binary { op, lhs, rhs, .. } => format!(
            "({} {} {})",
            expr_text(lhs, None),
            binop_text(*op),
            expr_text(rhs, None)
        ),
        core::Expr::Call { callee, args, .. } => call_text(*callee, args),
        core::Expr::ArrayLit { elems, .. } => {
            let element = match strip(want) {
                Some(Type::Array(t)) => Some(t.as_ref()),
                _ => None,
            };
            let text: Vec<String> = elems.iter().map(|e| expr_text(e, element)).collect();
            let literal = format!("[{}]", text.join(", "));
            match (elems.is_empty(), strip(want)) {
                (true, Some(t)) => format!("cast({literal}, {})", ty_text(t)),
                _ => literal,
            }
        }
        core::Expr::DictLit { entries, .. } => {
            let (k_ty, v_ty) = match strip(want) {
                Some(Type::Dict(k, v)) => (Some(k.as_ref()), Some(v.as_ref())),
                _ => (None, None),
            };
            let text: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{} => {}", expr_text(k, k_ty), expr_text(v, v_ty)))
                .collect();
            let literal = format!("{{{}}}", text.join(", "));
            match (entries.is_empty(), strip(want)) {
                (true, Some(t)) => format!("cast({literal}, {})", ty_text(t)),
                _ => literal,
            }
        }
        core::Expr::RecordLit { fields, .. } => {
            let want = strip(want);
            let fields: Vec<(String, String)> = {
                let mut sorted: Vec<&(String, core::Expr)> = fields.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                sorted
                    .iter()
                    .map(|(name, e)| {
                        let inner = match want {
                            Some(Type::Record(f)) => {
                                f.iter().find(|(n, _)| n == name).map(|(_, t)| t)
                            }
                            _ => None,
                        };
                        (name.clone(), expr_text(e, inner))
                    })
                    .collect()
            };
            record_text(&fields)
        }
    }
}

/// Desugaring, run backwards.
///
/// `record:get`, `dict:get` and `boolean:not` are grasp's reserved-namespace
/// spellings of `s.f`, a dict lookup and `not`. grasp-dbsp has no such
/// callables, so the emitter puts back what desugaring took apart — and a
/// program that wrote one by hand reaches the same text as the sugar, which is
/// what lets the two be asserted equivalent.
fn call_text(callee: core::Builtin, args: &[core::Expr]) -> String {
    match (callee, args) {
        (core::Builtin::RecordGet, [subject, field]) => {
            if let core::Expr::Lit {
                value: Lit::Str(name),
                ..
            } = field
            {
                return format!("{}.{name}", expr_text(subject, None));
            }
            // `record:get` with a computed field has no grasp-dbsp spelling and
            // no grasp one either — `s.f` is the only way to write it, and its
            // field is a name. Unreachable rather than handled.
            unreachable!("`record:get` takes a literal field name")
        }
        (core::Builtin::DictGet, [dict, k]) => {
            format!("get({}, {})", expr_text(dict, None), expr_text(k, None))
        }
        (core::Builtin::BooleanNot, [e]) => format!("(not {})", expr_text(e, None)),
        _ => {
            let text: Vec<String> = args.iter().map(|a| expr_text(a, None)).collect();
            format!("{}({})", callee.as_str(), text.join(", "))
        }
    }
}

/// The type under an `optional`, which is what a container literal has to
/// match: `optional(array(i64))` wants `[]` cast to `array(i64)`, not to
/// itself. `NONE` is the one value that wants the `optional` whole, and it is
/// handled before this is reached.
fn strip(want: Option<&Type>) -> Option<&Type> {
    match want {
        Some(Type::Optional(t)) => Some(t),
        other => other,
    }
}

fn lit_text(value: &Lit) -> String {
    match value {
        Lit::Int(n) => n.to_string(),
        // Shared with the structural key: `1.0` must not become `1`, which both
        // languages lex as an integer.
        Lit::Float(f) => key::float(*f),
        Lit::Str(s) => format!("{s:?}"),
        Lit::Bool(b) => b.to_string(),
        Lit::None => "NONE".to_string(),
    }
}

/// "The operators correspond directly, with one spelling difference."
fn binop_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "==",
        BinOp::Concat => unreachable!("`++` becomes `concat` in desugaring"),
        other => key::binop(other),
    }
}

#[cfg(test)]
mod tests {
    /// `mapping.md`'s four rules for a fixpoint, asserted against the text.
    ///
    /// They need a test of their own because **no fixture can see three of
    /// them**. A redundant `distinct` on a recursive stream is semantically
    /// identity, a missing typespec is a compile error rather than a wrong
    /// answer, and both would pass every output case there is. The document
    /// calls all of them easy to get wrong, and it is right: the only way one
    /// of these surfaces is by reading the emitted program, so something has to
    /// read it.
    fn body_of(source: &str) -> String {
        let text = crate::compile(source).expect("compiles");
        let start = text.find('{').expect("a circuit");
        let end = text.find('}').expect("a circuit");
        text[start..end].to_string()
    }

    const TC: &str = "\
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input

path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)
";

    #[test]
    fn a_recursive_stream_is_not_deduplicated_by_hand() {
        // "grasp-dbsp applies it to every recursive stream on every round —
        //  that is what makes the iteration terminate. A hand-written one
        //  lowers a redundant second `distinct`."
        assert!(
            !body_of(TC).contains("distinct"),
            "a fixpoint body must not deduplicate: {}",
            body_of(TC)
        );
    }

    #[test]
    fn a_recursive_stream_carries_its_typespec() {
        // Inference follows only the operators whose result type is one of
        // their operands, and a Datalog rule ends in a `map`, which is not one.
        assert!(
            body_of(TC).contains("path :: zset(record(dst: i64, src: i64))"),
            "a recursive stream needs its type written: {}",
            body_of(TC)
        );
    }

    #[test]
    fn a_recursive_stream_starts_empty() {
        let text = crate::compile(TC).expect("compiles");
        assert!(
            text.contains("path: empty()"),
            "the call site passes `empty()`, the base case being in the body: {text}"
        );
    }

    #[test]
    fn a_fact_is_emitted_outside_the_fixpoint() {
        // "grasp-dbsp rejects `constant` inside a `fixpoint`, where a source
        //  would fire once per iteration rather than once per transaction."
        let source = "\
step :: relation(from: i64, to: i64)
step(from:, to:) <- input

seen :: relation(node: i64)
seen(node: 1)
seen(node: b) <-
    seen(node: a)
    step(from: a, to: b)
";
        let body = body_of(source);
        assert!(
            !body.contains("constant"),
            "a `constant` may not sit in a fixpoint body: {body}"
        );
        assert!(
            crate::compile(source)
                .expect("compiles")
                .contains("constant("),
            "the fact still has to be emitted, outside"
        );
    }
}
