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

use crate::ast::{Aggregator, BinOp, Lit, Type, UnOp};
use crate::core;
use crate::key;
use crate::plan::{Agg, Definite, Field, Group, Narrowed, Node, Op, Plan, Relation, Rule, Source};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// The row binder every emitted function takes.
///
/// One name for all of them: functions never nest, so it can never shadow.
const ROW: &str = "row";

/// The binder an aggregator's projection takes, which is one group value
/// rather than a row.
const VALUE: &str = "v";

/// The binder a `map_array` gives one element of the collection being unnested.
const ELEMENT: &str = "e";

/// The binder it gives that element's index, where the unnest asked for one.
const INDEX: &str = "i";

/// Where each grasp variable is found in the grasp-dbsp being written.
///
/// Almost always a field of one row — every rule variable is — and for a long
/// time one name was enough. A `map_array` inside a `flat_map` breaks that: the
/// unnested variables come off the element binder while the rest of the row
/// still comes off the row, and both are in scope at once. So this is a lookup
/// rather than a name, with the row as what a variable falls back to.
struct Scope<'a> {
    row: &'a str,
    bound: Option<&'a BTreeMap<String, String>>,
}

impl<'a> Scope<'a> {
    fn row(row: &'a str) -> Scope<'a> {
        Scope { row, bound: None }
    }

    fn of(&self, v: &str) -> String {
        match self.bound.and_then(|b| b.get(v)) {
            Some(text) => text.clone(),
            None => format!("{}.{v}", self.row),
        }
    }
}

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
        //
        // The `distinct` is what makes an input relation a set: the outside
        // world writes a Z-set, and the relation is the rows whose accumulated
        // weight is positive. The raw stream takes the intermediate name and
        // the relation keeps its own, so "the node named `r` is relation `r`'s
        // stream" still holds — it is the `input` node that moves.
        Source::Input => {
            let raw = names.intermediate(node);
            let _ = writeln!(out, "{raw} :: {ty}");
            let _ = writeln!(out, "{raw} := input({:?})", relation.name);
            let _ = writeln!(out, "{node} := distinct({raw})");
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
                // "The facts of one relation collect into a single
                // `constant`", which is an operand of the union like a rule's
                // output. It needs a name and a typespec whether or not
                // anything else defines the relation, because `constant` does
                // not nest and takes its type from a `::`.
                let name = names.intermediate(node);
                let rows: Vec<String> = facts
                    .iter()
                    .map(|row| fact_row(row, &relation.columns))
                    .collect();
                let _ = writeln!(out, "{name} :: {ty}");
                let _ = writeln!(out, "{name} := constant([{}])", rows.join(", "));
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
            // A relation is a set, and neither `plus` nor `constant` makes one:
            // `plus` adds weights, and a `constant` consolidates identical rows
            // by summing theirs — so a fact written twice, or written once and
            // once again as an expression that evaluates the same, arrives at
            // weight 2. Facts are program text and a relation is a set, so
            // asserting a member twice is asserting it once.
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
            Op::Unnest {
                input,
                over,
                binds,
                kind,
            } => emit_unnest(
                out,
                &base,
                names,
                &at[*input],
                over,
                binds,
                *kind,
                &node.schema,
            ),
            Op::Distinct { input } => {
                let name = names.intermediate(&base);
                let _ = writeln!(out, "{name} := distinct({})", at[*input]);
                name
            }
            Op::Aggregate { input, group, aggs } => {
                emit_aggregate(out, &base, names, &at[*input], group, aggs, &node.schema)
            }
            Op::Narrow {
                input,
                name: v,
                cast,
                into,
            } => emit_narrow(out, &base, names, node, v, cast.as_ref(), into, &at[*input]),
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

/// Group, fold, flatten — `compilation.md`'s three nodes, and the extras that
/// several aggregates in one rule need.
///
/// The group is the index key, so it is indexed first; each aggregator folds
/// the same indexed stream; and the result is flattened back, because a
/// relation is flat. Two or more aggregates then have to be brought together,
/// which is a `join_index` per extra one on the key they already share.
///
/// `count<>` folds a constant. grasp counts rows in the group and grasp-dbsp
/// counts the projections that are not absent, so a projection that can never
/// be absent makes the two agree — and a literal is the simplest of those.
///
/// `avg` is the one aggregator whose types differ across the two languages:
/// grasp says `f64`, grasp-dbsp says `optional(f64)` because a mean of no
/// contributing rows is undefined. So it is narrowed, on the same terms as a
/// division — the row with no answer does not appear.
#[allow(clippy::too_many_arguments)]
fn emit_aggregate(
    out: &mut String,
    base: &str,
    names: &mut Names,
    input: &str,
    group: &[String],
    aggs: &[Agg],
    schema: &BTreeSet<String>,
) -> String {
    let mut projected: BTreeSet<String> = BTreeSet::new();
    for a in aggs {
        if let Some(e) = &a.arg {
            crate::plan::collect_free(e, &mut projected);
        }
    }
    let val: Vec<String> = projected.into_iter().collect();
    let indexed = names.intermediate(base);
    let _ = writeln!(
        out,
        "{indexed} := map_index({input}, function(({ROW}) -> record(key: {}, value: {})))",
        row_record(group),
        row_record(&val)
    );

    // One `aggregate` per aggregator over the one indexed stream, each wrapped
    // so that the values combine as records rather than as bare scalars.
    let mut combined: Option<String> = None;
    let mut carried: Vec<String> = Vec::new();
    for a in aggs {
        let folded = names.intermediate(base);
        let projection = match &a.arg {
            Some(e) => expr_in(&Scope::row(VALUE), e, None),
            None => "0".to_string(),
        };
        let _ = writeln!(
            out,
            "{folded} := aggregate({indexed}, {}, function(({VALUE}) -> {projection}))",
            aggregator_text(a.function)
        );
        let wrapped = names.intermediate(base);
        let _ = writeln!(
            out,
            "{wrapped} := map_index({folded}, function((k, {VALUE}) -> \
             record(key: k, value: record({}: {VALUE}))))",
            a.out
        );
        combined = Some(match combined {
            None => wrapped,
            Some(acc) => {
                let joined = names.intermediate(base);
                let mut fields: Vec<(String, String)> = carried
                    .iter()
                    .map(|v| (v.clone(), format!("a.{v}")))
                    .collect();
                fields.push((a.out.clone(), format!("b.{}", a.out)));
                let _ = writeln!(
                    out,
                    "{joined} := join_index({acc}, {wrapped}, function((k, a, b) -> \
                     record(key: k, value: {})))",
                    record_text(&fields)
                );
                joined
            }
        });
        carried.push(a.out.clone());
    }
    let combined = combined.expect("an aggregate step holds at least one aggregate");

    let fields: Vec<(String, String)> = schema
        .iter()
        .map(|v| {
            let from = if group.contains(v) {
                format!("k.{v}")
            } else {
                format!("{VALUE}.{v}")
            };
            (v.clone(), from)
        })
        .collect();
    let flat = names.intermediate(base);
    let _ = writeln!(
        out,
        "{flat} := map({combined}, function((k, {VALUE}) -> {}))",
        record_text(&fields)
    );

    flat
}

fn aggregator_text(a: Aggregator) -> &'static str {
    // grasp's five are grasp-dbsp's five, under the same names.
    key::aggregator(a)
}

/// One row per element, with the rest of the row carried alongside each.
///
/// A `flat_map` fans out over the array its function returns, so the row it
/// emits *is* an element — and the rest of the row would be lost. `map_array` is
/// what builds the rows to fan out to, putting the carried fields back beside
/// each element. That is the whole reason grasp-dbsp has it.
///
/// A dict goes through `entries`, which already yields
/// `array(record(key, value))`, so both kinds take one shape and differ only in
/// what an element's parts are called.
#[allow(clippy::too_many_arguments)]
fn emit_unnest(
    out: &mut String,
    base: &str,
    names: &mut Names,
    input: &str,
    over: &core::Expr,
    binds: &[String],
    kind: core::UnnestKind,
    schema: &BTreeSet<String>,
) -> String {
    let array = match kind {
        core::UnnestKind::Array => expr_text(over, None),
        core::UnnestKind::Dict => format!("entries({})", expr_text(over, None)),
    };
    // What the unnested variables are called on the element, which is the whole
    // of the difference between the three shapes. Only the indexed one needs
    // the second binder, so only it names one — an unnamed slot is bound all
    // the same, and writing it would only be noise in the emitted text.
    let mut bound: BTreeMap<String, String> = BTreeMap::new();
    let mut params = ELEMENT.to_string();
    match (kind, binds) {
        (core::UnnestKind::Array, [v]) => {
            bound.insert(v.clone(), ELEMENT.to_string());
        }
        (core::UnnestKind::Array, [i, v]) => {
            bound.insert(i.clone(), INDEX.to_string());
            bound.insert(v.clone(), ELEMENT.to_string());
            params = format!("{ELEMENT}, {INDEX}");
        }
        (core::UnnestKind::Dict, [k, v]) => {
            bound.insert(k.clone(), format!("{ELEMENT}.key"));
            bound.insert(v.clone(), format!("{ELEMENT}.value"));
        }
        // `infer::check_unnest` is what makes this total: it admits these three
        // shapes and rejects every other arity. Without it a grammatical
        // `(k) := **d` reaches here.
        _ => unreachable!(
            "`check_unnest` admits a value, an index and a value, or a key and a value"
        ),
    }
    let scope = Scope {
        row: ROW,
        bound: Some(&bound),
    };
    let fields: Vec<(String, String)> = schema.iter().map(|v| (v.clone(), scope.of(v))).collect();
    let name = names.intermediate(base);
    let _ = writeln!(
        out,
        "{name} := flat_map({input}, function(({ROW}) -> \
         map_array({array}, function(({params}) -> {}))))",
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
#[allow(clippy::too_many_arguments)]
fn emit_narrow(
    out: &mut String,
    base: &str,
    names: &mut Names,
    node: &Node,
    v: &str,
    cast: Option<&Type>,
    into: &Narrowed,
    previous: &str,
) -> String {
    // A field of the row, rebuilt with `v` replaced by `value`.
    let row_with = |value: &str| {
        let fields: Vec<(String, String)> = node
            .schema
            .iter()
            .map(|f| {
                let text = if f == v {
                    value.to_string()
                } else {
                    format!("{ROW}.{f}")
                };
                (f.clone(), text)
            })
            .collect();
        record_text(&fields)
    };

    // An assertion that keeps its wrapper is two operators rather than three,
    // and different in kind: `optional(A) :: optional(B)` leaves an
    // `optional(B)`, so there is no `coalesce` and absence is one of the
    // answers rather than one of the rows to drop.
    //
    // Which means the test cannot be `!= NONE`: a `cast` maps absence and a
    // failed extraction alike to `NONE`, and only the first is to be kept. So
    // the filter reads the value on both sides of the conversion, and the
    // conversion is written twice rather than bound to a field this would have
    // to invent a name for. Every expression here is total, so the second
    // evaluation is a cost and never a difference.
    // A container narrowing tests every part and converts every part, so both
    // operators walk the value: `filter_array` counts the parts that would
    // survive against the parts there are, and `map_array` rebuilds it. Neither
    // touches the value as a whole, which is why `cast` is `None` here.
    if let Narrowed::Elements(el) | Narrowed::Values(el) = into {
        let dict = matches!(into, Narrowed::Values(_));
        let parts = if dict {
            format!("entries({ROW}.{v})")
        } else {
            format!("{ROW}.{v}")
        };
        let part = if dict {
            format!("{ELEMENT}.value")
        } else {
            ELEMENT.to_string()
        };
        let converted = format!("cast({part}, optional({}))", ty_text(el));
        let kept = names.intermediate(base);
        let _ = writeln!(
            out,
            "{kept} := filter({previous}, function(({ROW}) -> \
             (length(filter_array({parts}, function(({ELEMENT}) -> ({converted} != NONE)))) \
             == length({ROW}.{v}))))"
        );
        // The `coalesce` default is unreachable for the same reason it is in
        // the whole-value shape: the filter above has dropped every row with a
        // part that would take it.
        let definite = format!("coalesce({converted}, {})", definite_value(el));
        let rebuilt = if dict {
            format!(
                "dict(map_array({parts}, function(({ELEMENT}) -> \
                 record(key: {ELEMENT}.key, value: {definite}))))"
            )
        } else {
            format!("map_array({parts}, function(({ELEMENT}) -> {definite}))")
        };
        let name = names.intermediate(base);
        let _ = writeln!(
            out,
            "{name} := map({kept}, function(({ROW}) -> {}))",
            row_with(&rebuilt)
        );
        return name;
    }

    if let Narrowed::Wrapper = into {
        let ty = cast.expect("a wrapper-keeping narrowing converts");
        let converted = format!("cast({ROW}.{v}, {})", ty_text(ty));
        let kept = names.intermediate(base);
        let _ = writeln!(
            out,
            "{kept} := filter({previous}, function(({ROW}) -> \
             ({ROW}.{v} == NONE or {converted} != NONE)))"
        );
        let name = names.intermediate(base);
        let _ = writeln!(
            out,
            "{name} := map({kept}, function(({ROW}) -> {}))",
            row_with(&converted)
        );
        return name;
    }

    let Narrowed::Definite(definite) = into else {
        unreachable!("the container and wrapper shapes returned above")
    };

    // The first of `mapping.md`'s three operators, where one is needed: bind
    // the value at the `optional(T)` the check produces. A `json` source needs
    // it; a value that is already an `optional(T)`, as a division's result is,
    // does not.
    let mut previous = previous.to_string();
    if let Some(ty) = cast {
        let bound = names.intermediate(base);
        let _ = writeln!(
            out,
            "{bound} := map({previous}, function(({ROW}) -> {}))",
            row_with(&format!("cast({ROW}.{v}, optional({}))", ty_text(ty)))
        );
        previous = bound;
    }
    let present = names.intermediate(base);
    let _ = writeln!(
        out,
        "{present} := filter({previous}, function(({ROW}) -> ({ROW}.{v} != NONE)))"
    );
    let default = match definite {
        Definite::Like(e) => expr_text(e, None),
        Definite::Of(ty) => definite_value(ty),
    };
    let name = names.intermediate(base);
    let _ = writeln!(
        out,
        "{name} := map({present}, function(({ROW}) -> {}))",
        row_with(&format!("coalesce({ROW}.{v}, {default})"))
    );
    name
}

/// Some definite value of a type, for a `coalesce` whose default is never read.
///
/// "An emitter needs some definite value of every type, which is `0`, `0.0`,
/// `\"\"`, `false`, `cast([], array(T))`, `cast({}, dict(K,V))`, or a record
/// built from those." A document takes one the same way, out of an integer.
fn definite_value(ty: &Type) -> String {
    match ty {
        Type::Boolean => "false".to_string(),
        Type::I64 => "0".to_string(),
        Type::F64 => "0.0".to_string(),
        Type::String => "\"\"".to_string(),
        Type::Json => "cast(0, json)".to_string(),
        Type::Optional(_) => "NONE".to_string(),
        // An empty container has no element type of its own, so it is said.
        Type::Array(_) | Type::Dict(..) => {
            let empty = if matches!(ty, Type::Array(_)) {
                "[]"
            } else {
                "{}"
            };
            format!("cast({empty}, {})", ty_text(ty))
        }
        Type::Record(fields) => {
            let mut sorted: Vec<&(String, Type)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<(String, String)> = sorted
                .iter()
                .map(|(n, t)| (n.clone(), definite_value(t)))
                .collect();
            record_text(&text)
        }
    }
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
    "map_array",
    "filter_array",
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
    expr_in(&Scope::row(ROW), e, want)
}

/// One expression against a scope.
fn expr_in(scope: &Scope<'_>, e: &core::Expr, want: Option<&Type>) -> String {
    let expr_text = |e: &core::Expr, want: Option<&Type>| expr_in(scope, e, want);
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
        core::Expr::Var { name, .. } => scope.of(name),
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
        core::Expr::Call { callee, args, .. } => call_text(scope, *callee, args),
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
fn call_text(scope: &Scope<'_>, callee: core::Builtin, args: &[core::Expr]) -> String {
    let expr_text = |e: &core::Expr, want: Option<&Type>| expr_in(scope, e, want);
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
        (core::Builtin::ArrayGet, [array, index]) => {
            format!(
                "get({}, {})",
                expr_text(array, None),
                expr_text(index, None)
            )
        }
        // The one place the emitter introduces a binder of its own. `e` and `i`
        // are free here: an expression is emitted inside a `map`'s function,
        // whose one parameter is the row, and `array:drop` never nests inside
        // another array function — a pattern produces at most one.
        // The keys are the pattern's, so they are literals and the predicate is
        // a conjunction the emitter writes out — no membership builtin needed.
        // A pattern naming no key at all leaves `true`.
        (core::Builtin::DictWithoutKeys, [dict, keys]) => {
            let core::Expr::ArrayLit { elems, .. } = keys else {
                unreachable!("`dict:without_keys` takes a literal array of keys")
            };
            let tests: Vec<String> = elems
                .iter()
                .map(|k| format!("{ELEMENT}.key != {}", expr_text(k, None)))
                .collect();
            let predicate = if tests.is_empty() {
                "true".to_string()
            } else {
                tests.join(" and ")
            };
            format!(
                "dict(filter_array(entries({}), function(({ELEMENT}) -> ({predicate}))))",
                expr_text(dict, None)
            )
        }
        (core::Builtin::ArrayDrop, [array, from]) => format!(
            "filter_array({}, function(({ELEMENT}, {INDEX}) -> ({INDEX} >= {})))",
            expr_text(array, None),
            expr_text(from, None)
        ),
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
    /// The reserved list is a copy of grasp-dbsp's, and nothing held the two
    /// together.
    ///
    /// `mangle` is what stops a grasp relation named `map` or `map_array` emitting
    /// a program the target cannot parse, and it reads this list — so a word
    /// grasp-dbsp reserves and this list omits is a program that fails to parse
    /// for a reason no test would explain. The two crates meet at grasp-dbsp
    /// text and a dev-dependency is enough to ask.
    #[test]
    fn the_reserved_list_is_grasp_dbsp_s() {
        for word in super::RESERVED {
            assert!(
                grasp_dbsp_runner::lang::is_reserved(word),
                "`{word}` is reserved here but not by grasp-dbsp"
            );
        }
        // The direction that actually breaks a program: a word grasp-dbsp
        // reserves and this list omits is one `mangle` does not escape.
        for word in grasp_dbsp_runner::lang::reserved_words() {
            assert!(
                super::RESERVED.contains(&word),
                "grasp-dbsp reserves `{word}` and `mangle` would not escape it"
            );
        }
    }

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
        // The recursive stream's own definition, not the whole body: an
        // aggregate inside a fixpoint deduplicates the assignments it folds,
        // which is a different `distinct` and a legitimate one.
        let body = body_of(TC);
        let definition = body
            .lines()
            .find(|l| l.trim_start().starts_with("path :="))
            .expect("the recursive stream is defined");
        assert!(
            !definition.contains("distinct"),
            "a recursive stream must not be deduplicated by hand: {definition}"
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
