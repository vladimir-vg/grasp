//! Plan — `docs/grasp/compilation.md`.
//!
//! Takes the typed core and produces, per rule, an ordered acyclic
//! single-sink sequence of column-level operations, plus the relation-level
//! structure that says how those sequences combine. Types are erased here:
//! nothing below this point carries one.
//!
//! Four things about the shape of this pass are load-bearing.
//!
//! **A rule body is a set.** Body statements constrain; they do not sequence.
//! Every ordering decision this pass makes is therefore its own, and must be a
//! function of the program's *meaning* — never of source position, which
//! `syntax.md` forbids keying on. Ties go to [`crate::key`]. There is no
//! `HashMap` in this file for the same reason.
//!
//! **The pass is shaped for a rule with several atoms even though it plans
//! only one.** It builds the join graph as occurrences, partitions it into
//! components, and plans each component — and today that partition always has
//! exactly one member. Writing "the atom, then the statements" instead would be
//! shorter and would have to be taken apart again the moment joins land; the
//! generality is cheap and the seam is where it belongs.
//!
//! **What this compiler cannot plan, it names.** [`gaps`] enumerates the
//! constructs a program uses that this stage does not implement, and nothing
//! reaches the rest of the pass unless that list is empty. There is no
//! catch-all: `Diagnostic::UNIMPLEMENTED` holds one entry per construct, so a
//! gap nobody named is a `debug_assert` failure rather than a silent pass.
//!
//! **Partial operators are lifted, not special-cased at emission.** `a / b` is
//! `T` in grasp and `optional(T)` in grasp-dbsp — "all errors inside grasp rule
//! bodies should cause silent row drop" — so every division is bound, tested
//! and rebound *here*, as ordinary nodes. Doing it in the emitter would hide a
//! filter from the liveness analysis and from the node order, and `v :: T` will
//! need the same construction.

use crate::ast::{BinOp, Type};
use crate::core;
use crate::diag::{Diagnostic, Pass, Span};
use crate::infer;
use crate::key;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// What a plan is
// ---------------------------------------------------------------------------

/// A whole program, planned.
///
/// Relations are in name order rather than source order, for the reason every
/// other ordering here is structural: two programs whose declarations differ
/// only in sequence are one program.
#[derive(Debug)]
pub struct Plan {
    pub relations: Vec<Relation>,
}

#[derive(Debug)]
pub struct Relation {
    /// The grasp name. This is the `input(...)` table string, and what a fixture
    /// names when it pushes rows.
    pub name: String,
    pub columns: Vec<(String, Type)>,
    pub source: Source,
}

/// Where a relation's rows come from — `mapping.md`'s relation table.
#[derive(Debug)]
pub enum Source {
    /// `r(cols:) <- input`.
    Input,
    /// A typespec and no producer: "a relation with only a spec is derived and
    /// empty".
    Empty,
    /// Facts, rules, or both. At least one of the two is non-empty.
    Derived {
        /// One entry per fact, each a complete row. Sorted by structural key.
        facts: Vec<Vec<(String, core::Expr)>>,
        /// Sorted by structural key, because the union that sums them must not
        /// be ordered by the file.
        rules: Vec<Rule>,
    },
}

/// One rule, lowered.
#[derive(Debug)]
pub struct Rule {
    /// In order. The last node's output is the rule's contribution to its
    /// relation, and its schema is exactly the head's columns.
    pub nodes: Vec<Node>,
    /// This rule's structural key, which orders it against its siblings.
    pub key: String,
    pub span: Span,
}

/// One column-level operation.
///
/// The invariant that makes this readable: **a node's expressions are written
/// against the previous node's schema, and its own schema is what it hands
/// on.** So `schema` is the set of fields in the row this node emits, and
/// nothing downstream may name anything else.
#[derive(Debug)]
pub struct Node {
    pub op: Op,
    pub schema: BTreeSet<String>,
}

/// One field of an output row.
#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub value: core::Expr,
    /// What this field's type must be, where something declared it: a rule
    /// variable's settled type, or a head column's.
    ///
    /// Carried because three grasp values are complete but open — `NONE`, `[]`
    /// and `{}` — and grasp-dbsp will not take one without being told what it
    /// holds. Everywhere else the value's own type is the answer and this is
    /// `None`.
    pub ty: Option<Type>,
}

#[derive(Debug)]
pub enum Op {
    /// The stream of a relation, entered by an atom. Its schema is the
    /// relation's *columns* — the only node whose names are not variables.
    Scan {
        relation: String,
    },
    /// The unit relation: one empty row, which grounds a rule with no atom.
    Ground,
    Filter {
        expr: core::Expr,
    },
    /// The complete output row. This is projection as well as binding: a
    /// variable absent from `fields` is gone.
    Map {
        fields: Vec<Field>,
    },
    /// `mapping.md`'s narrowing, minus its first operator: drop the rows where
    /// `name` has no value, then rebind it as a definite one.
    ///
    /// Two grasp-dbsp operators under one node, and they may not be separated —
    /// the rebinding without the filter above it would invent a value. It is a
    /// node of its own rather than a [`Op::Map`] because the expression it needs
    /// is `coalesce`, which is grasp-dbsp's and has no spelling in grasp: the
    /// two languages meet at emission, not here.
    ///
    /// `fallback` is never read, the filter having seen to that. It is carried
    /// because grasp-dbsp typechecks it anyway.
    Narrow {
        name: String,
        fallback: core::Expr,
    },
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Plan a typed program.
pub fn plan(typed: infer::Typed) -> Result<Plan, Vec<Diagnostic>> {
    let gaps = gaps(&typed);
    if !gaps.is_empty() {
        return Err(gaps);
    }

    // Facts and rules, gathered per relation. `infer` hands them back in one
    // list because it had no reason to separate them; emission does.
    let mut facts: BTreeMap<String, Vec<&core::Fact>> = BTreeMap::new();
    let mut rules: BTreeMap<String, Vec<&infer::TypedRule>> = BTreeMap::new();
    for decl in &typed.decls {
        match decl {
            infer::Decl::Fact(f) => facts.entry(f.relation.clone()).or_default().push(f),
            infer::Decl::Rule(r) => rules
                .entry(r.rule.head.relation.clone())
                .or_default()
                .push(r),
        }
    }

    let mut relations = Vec::new();
    for (name, relation) in &typed.relations {
        let source = if relation.kind == infer::Kind::Input {
            Source::Input
        } else {
            let f = facts.remove(name).unwrap_or_default();
            let r = rules.remove(name).unwrap_or_default();
            if f.is_empty() && r.is_empty() {
                Source::Empty
            } else {
                let mut rows: Vec<Vec<(String, core::Expr)>> =
                    f.iter().map(|fact| fact.args.clone()).collect();
                rows.sort_by_key(|row| key::atom(name, &to_args(row)));
                let mut planned: Vec<Rule> = r
                    .iter()
                    .map(|typed_rule| plan_rule(typed_rule, &relation.columns))
                    .collect::<Result<_, _>>()?;
                planned.sort_by(|a, b| a.key.cmp(&b.key));
                Source::Derived {
                    facts: rows,
                    rules: planned,
                }
            }
        };
        relations.push(Relation {
            name: name.clone(),
            columns: relation.columns.clone(),
            source,
        });
    }

    Ok(Plan { relations })
}

fn to_args(row: &[(String, core::Expr)]) -> Vec<(String, core::Arg)> {
    row.iter()
        .map(|(c, e)| (c.clone(), core::Arg::Expr(e.clone())))
        .collect()
}

// ---------------------------------------------------------------------------
// What this stage cannot do yet
// ---------------------------------------------------------------------------

/// Every construct the program uses that this stage does not implement.
///
/// Ranked most-dependent first, because the harness attributes a case to the
/// first diagnostic: a program blocked by both a join and recursion should
/// count against recursion, since implementing joins alone would free nothing.
/// The attribution re-settles on its own as constructs land.
///
/// This function is the reason `Diagnostic::UNIMPLEMENTED` has no catch-all.
/// Every shape this stage cannot handle is named here or the `debug_assert` in
/// `Diagnostic::unimplemented` fires.
pub fn gaps(typed: &infer::Typed) -> Vec<Diagnostic> {
    let mut found: BTreeMap<&'static str, Span> = BTreeMap::new();
    let mut note = |construct: &'static str, span: Span| {
        found.entry(construct).or_insert(span);
    };

    // A relation is recursive when it can reach itself. The graph is over
    // relations, edge `head -> body relation`, exactly as `semantics.md`'s
    // stratification algorithm builds it.
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut rule_count: BTreeMap<&str, usize> = BTreeMap::new();
    let mut has_fact: BTreeSet<&str> = BTreeSet::new();
    for decl in &typed.decls {
        match decl {
            infer::Decl::Fact(f) => {
                has_fact.insert(&f.relation);
            }
            infer::Decl::Rule(r) => {
                *rule_count.entry(&r.rule.head.relation).or_default() += 1;
                let out = edges.entry(&r.rule.head.relation).or_default();
                for stmt in &r.rule.body {
                    if let core::Stmt::Atom { relation, .. } = stmt {
                        out.insert(relation);
                    }
                }
            }
        }
    }
    if let Some(span) = recursive(&edges, typed) {
        note("recursion", span);
    }

    for decl in &typed.decls {
        let infer::Decl::Rule(r) = decl else { continue };
        let rule = &r.rule;

        let mut atoms: Vec<&core::Stmt> = Vec::new();
        for stmt in &rule.body {
            match stmt {
                core::Stmt::Atom {
                    negated: true,
                    span,
                    ..
                } => note("negation", *span),
                core::Stmt::Atom { negated: false, .. } => atoms.push(stmt),
                core::Stmt::Match { lhs, rhs, span } => {
                    if matches!(rhs, core::Rhs::Aggregate { .. }) {
                        note("aggregation", *span);
                    }
                    if matches!(lhs, core::Pattern::Unnest { .. }) {
                        note("unnesting", *span);
                    }
                }
                // `infer` reports this one, so a program carrying it never
                // reaches here — but naming it costs nothing and the day the
                // assertion lands, this is where it stops being a gap.
                core::Stmt::Assert { span, .. } => note("type assertions", *span),
                core::Stmt::Filter { .. } | core::Stmt::Input { .. } => {}
            }
        }

        // Two atoms are one component when they share a variable. So a second
        // atom is a join if it meets the first, and a cross product if it does
        // not — different machinery, landing in different slices, which is why
        // they are counted apart rather than as "more than one atom".
        if atoms.len() > 1 {
            let construct = if components(&atoms).len() > 1 {
                "cross products"
            } else {
                "joins"
            };
            note(construct, rule.span);
        }
    }

    for (relation, count) in &rule_count {
        if *count > 1 || (*count == 1 && has_fact.contains(*relation)) {
            let span = typed
                .decls
                .iter()
                .find_map(|d| match d {
                    infer::Decl::Rule(r) if r.rule.head.relation == **relation => Some(r.rule.span),
                    _ => None,
                })
                .unwrap_or(Span::new(1, 1, 0));
            note("unions of rules", span);
        }
    }

    // The rank. `Diagnostic::UNIMPLEMENTED` lists the plan constructs in this
    // order too, so the two cannot drift far apart unnoticed.
    const RANK: &[&str] = &[
        "recursion",
        "aggregation",
        "negation",
        "cross products",
        "joins",
        "unnesting",
        "unions of rules",
        "type assertions",
    ];
    let mut out = Vec::new();
    for construct in RANK {
        if let Some(span) = found.get(construct) {
            out.push(Diagnostic::unimplemented(Pass::Plan, *span, *construct));
        }
    }
    debug_assert_eq!(
        out.len(),
        found.len(),
        "a construct was noted that `RANK` does not list, so it would be dropped"
    );
    out
}

/// The span of some rule in a cycle, if the dependency graph has one.
///
/// A relation that reaches itself is recursive; a self-loop is the one-member
/// case. An input relation is never a rule head, so it has no out-edge and
/// cannot be in a cycle — which is why `semantics.md`'s "an input relation may
/// not take part in recursion" needs no diagnostic.
fn recursive(edges: &BTreeMap<&str, BTreeSet<&str>>, typed: &infer::Typed) -> Option<Span> {
    for start in edges.keys() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = edges.get(start).into_iter().flatten().copied().collect();
        while let Some(r) = stack.pop() {
            if r == *start {
                return typed.decls.iter().find_map(|d| match d {
                    infer::Decl::Rule(rule) if rule.rule.head.relation == *start => {
                        Some(rule.rule.span)
                    }
                    _ => None,
                });
            }
            if !seen.insert(r) {
                continue;
            }
            stack.extend(edges.get(r).into_iter().flatten().copied());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The join graph
// ---------------------------------------------------------------------------

/// The positive atoms of one body, partitioned into connected components by the
/// variables they share.
///
/// `compilation.md`: "Two atoms sharing a variable are in one component and
/// will be joined on it; two sharing none cannot be joined at all." The
/// partition is what decides join against cross product, and it is what the
/// optimizer will plan and order — so it exists at one atom, where it always
/// returns a single component, rather than being introduced later.
fn components<'a>(atoms: &[&'a core::Stmt]) -> Vec<Vec<&'a core::Stmt>> {
    let vars: Vec<BTreeSet<&str>> = atoms.iter().map(|a| atom_vars(a)).collect();
    let mut owner: Vec<usize> = (0..atoms.len()).collect();
    // Union-find, flattened by hand: the sets are tiny and this keeps the
    // iteration order a function of position in `atoms`, which is itself a
    // function of the body's own order — settled by the caller.
    for i in 0..atoms.len() {
        for j in 0..i {
            if !vars[i].is_disjoint(&vars[j]) {
                let (a, b) = (owner[i], owner[j]);
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                for o in owner.iter_mut() {
                    if *o == hi {
                        *o = lo;
                    }
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<&core::Stmt>> = BTreeMap::new();
    for (i, atom) in atoms.iter().enumerate() {
        groups.entry(owner[i]).or_default().push(atom);
    }
    groups.into_values().collect()
}

fn atom_vars(stmt: &core::Stmt) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    if let core::Stmt::Atom { args, .. } = stmt {
        for (_, arg) in args {
            if let core::Arg::Expr(e) = arg {
                collect_vars(e, &mut out);
            }
        }
    }
    out
}

fn collect_vars<'a>(e: &'a core::Expr, out: &mut BTreeSet<&'a str>) {
    match e {
        core::Expr::Var { name, .. } => {
            out.insert(name);
        }
        core::Expr::Lit { .. } => {}
        core::Expr::Unary { operand, .. } => collect_vars(operand, out),
        core::Expr::Binary { lhs, rhs, .. } => {
            collect_vars(lhs, out);
            collect_vars(rhs, out);
        }
        core::Expr::Call { args, .. } => args.iter().for_each(|a| collect_vars(a, out)),
        core::Expr::ArrayLit { elems, .. } => elems.iter().for_each(|a| collect_vars(a, out)),
        core::Expr::DictLit { entries, .. } => entries.iter().for_each(|(k, v)| {
            collect_vars(k, out);
            collect_vars(v, out);
        }),
        core::Expr::RecordLit { fields, .. } => {
            fields.iter().for_each(|(_, v)| collect_vars(v, out))
        }
    }
}

// ---------------------------------------------------------------------------
// Planning one rule
// ---------------------------------------------------------------------------

/// A statement that is not a positive atom: it consumes variables and may
/// produce one, but it is not in the spanning tree and has to be placed.
struct Dependent {
    kind: Kind,
    /// What it produces, if anything.
    binds: Option<String>,
    consumes: BTreeSet<String>,
    key: String,
    /// The work itself, already lowered to the ops it becomes.
    body: Body,
}

/// Ordered as `compilation.md` orders dependent nodes that become ready
/// together: "negated atoms, then filters, then matches, then aggregates" —
/// most selective first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Filter,
    Match,
}

enum Body {
    Filter(core::Expr),
    Bind(String, core::Expr),
}

fn plan_rule(
    typed: &infer::TypedRule,
    columns: &[(String, Type)],
) -> Result<Rule, Vec<Diagnostic>> {
    let rule = &typed.rule;
    let mut fresh = Fresh::new(typed);

    let atoms: Vec<&core::Stmt> = rule
        .body
        .iter()
        .filter(|s| matches!(s, core::Stmt::Atom { negated: false, .. }))
        .collect();

    let mut dependents: Vec<Dependent> = Vec::new();

    // The atom's own constraints. A column bound to a plain variable binds it;
    // anything else — a literal, an expression, a repeat of a variable this
    // atom already bound — is an equality filter over a fresh name, which is
    // the same machinery competing producers use.
    let mut entry: Vec<(String, String)> = Vec::new(); // (column, name)
    if let Some(core::Stmt::Atom { args, .. }) = atoms.first().copied() {
        let mut bound: BTreeSet<String> = BTreeSet::new();
        for (column, arg) in args {
            match arg {
                core::Arg::Wildcard(_) => {}
                core::Arg::Expr(core::Expr::Var { name, .. }) if bound.insert(name.clone()) => {
                    entry.push((column.clone(), name.clone()));
                }
                core::Arg::Expr(e) => {
                    let name = fresh.next();
                    entry.push((column.clone(), name.clone()));
                    let eq = equals(&name, e, e.span());
                    dependents.push(Dependent {
                        kind: Kind::Filter,
                        binds: None,
                        consumes: free(&eq),
                        key: key::expr(&eq),
                        body: Body::Filter(eq),
                    });
                }
            }
        }
    }

    for stmt in &rule.body {
        match stmt {
            core::Stmt::Atom { .. } | core::Stmt::Input { .. } | core::Stmt::Assert { .. } => {}
            core::Stmt::Filter { expr, .. } => dependents.push(Dependent {
                kind: Kind::Filter,
                binds: None,
                consumes: free(expr),
                key: key::expr(expr),
                body: Body::Filter(expr.clone()),
            }),
            core::Stmt::Match { lhs, rhs, .. } => {
                let core::Pattern::Var { name, .. } = lhs else {
                    unreachable!("unnest is reported by `gaps`")
                };
                let core::Rhs::Expr(e) = rhs else {
                    unreachable!("an aggregate is reported by `gaps`")
                };
                dependents.push(Dependent {
                    kind: Kind::Match,
                    binds: Some(name.clone()),
                    consumes: free(e),
                    key: key::stmt(stmt),
                    body: Body::Bind(name.clone(), e.clone()),
                });
            }
        }
    }

    // Placement: earliest point where every input is bound, ties by kind then
    // by structural key. With one component the "post-order" is the atom alone,
    // so this loop is the whole ordering — and it is the same loop that will
    // place these nodes among several atoms.
    let mut available: BTreeSet<String> = entry.iter().map(|(_, n)| n.clone()).collect();
    let mut order: Vec<Dependent> = Vec::new();
    let mut pending = dependents;
    while !pending.is_empty() {
        let mut ready: Vec<usize> = (0..pending.len())
            .filter(|i| pending[*i].consumes.is_subset(&available))
            .collect();
        if ready.is_empty() {
            // Every remaining node wants a variable nothing produces. `infer`'s
            // safety check rejects that before here, so this is unreachable —
            // and a rule silently missing its filters would be worse than a
            // panic in a debug build.
            unreachable!("a body statement consumes a variable nothing binds");
        }
        ready.sort_by(|a, b| {
            (&pending[*a].kind, &pending[*a].key).cmp(&(&pending[*b].kind, &pending[*b].key))
        });
        let chosen = pending.remove(ready[0]);
        if let Some(v) = &chosen.binds {
            available.insert(v.clone());
        }
        order.push(chosen);
    }

    // Competing producers. "The primary producer is whichever comes first in
    // the post-order. Every other producer becomes an equality filter."
    let mut produced: BTreeSet<String> = BTreeSet::new();
    for node in order.iter_mut() {
        let Some(name) = node.binds.clone() else {
            continue;
        };
        if produced.insert(name.clone()) {
            continue;
        }
        let Body::Bind(_, e) = &node.body else {
            continue;
        };
        let eq = equals(&name, e, e.span());
        node.kind = Kind::Filter;
        node.binds = None;
        node.consumes = free(&eq);
        node.key = key::expr(&eq);
        node.body = Body::Filter(eq);
    }

    Ok(lower(
        rule, &atoms, entry, order, &mut fresh, typed, columns,
    ))
}

fn equals(name: &str, e: &core::Expr, span: Span) -> core::Expr {
    core::Expr::Binary {
        op: BinOp::Eq,
        lhs: Box::new(core::Expr::Var {
            name: name.to_string(),
            span,
        }),
        rhs: Box::new(e.clone()),
        span,
    }
}

fn free(e: &core::Expr) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_vars(e, &mut out);
    out.into_iter().map(str::to_string).collect()
}

// ---------------------------------------------------------------------------
// Lowering to nodes, and the liveness that projects
// ---------------------------------------------------------------------------

/// A source of names no grasp program can be using.
///
/// Numbered rather than derived from the expression, because the number comes
/// from position in the plan — which is already a function of the program — and
/// a name built from an expression would be unbounded in length.
struct Fresh {
    taken: BTreeSet<String>,
    next: usize,
}

impl Fresh {
    fn new(typed: &infer::TypedRule) -> Fresh {
        Fresh {
            taken: typed.vars.keys().cloned().collect(),
            next: 0,
        }
    }

    fn next(&mut self) -> String {
        loop {
            let name = format!("v{}", self.next);
            self.next += 1;
            if self.taken.insert(name.clone()) {
                return name;
            }
        }
    }
}

/// A step in the flat sequence a rule becomes, before liveness decides schemas.
enum Step {
    Filter(core::Expr),
    /// Bind one name, keeping everything else that is still live.
    Bind(String, core::Expr),
    /// Replace the row wholesale — the entry rename, and the head.
    Row(Vec<(String, core::Expr, Option<Type>)>),
    /// See [`Op::Narrow`].
    Narrow(String, core::Expr),
}

fn lower(
    rule: &core::Rule,
    atoms: &[&core::Stmt],
    entry: Vec<(String, String)>,
    order: Vec<Dependent>,
    fresh: &mut Fresh,
    typed: &infer::TypedRule,
    columns: &[(String, Type)],
) -> Rule {
    let mut steps: Vec<Step> = Vec::new();

    // The rename: out of the relation's column names and into the rule's
    // variables. Every rule has one, because every later node speaks variables.
    steps.push(Step::Row(
        entry
            .iter()
            .map(|(column, name)| {
                (
                    name.clone(),
                    core::Expr::Var {
                        name: column.clone(),
                        span: rule.span,
                    },
                    None,
                )
            })
            .collect(),
    ));

    for node in order {
        match node.body {
            Body::Filter(e) => push_total(&mut steps, fresh, Step::Filter, e),
            Body::Bind(name, e) => {
                push_total(&mut steps, fresh, move |e| Step::Bind(name.clone(), e), e)
            }
        }
    }

    // The head is the sink, and its schema is exactly the relation's columns.
    let (pre, head) = total_row(fresh, rule.head.args.clone());
    steps.extend(pre);
    steps.push(Step::Row(
        head.into_iter()
            .map(|(name, e)| {
                let ty = columns
                    .iter()
                    .find(|(c, _)| *c == name)
                    .map(|(_, t)| t.clone());
                (name, e, ty)
            })
            .collect(),
    ));

    // Liveness, backwards: a node's schema is what everything after it needs.
    // This is projection — a variable absent from the set is gone — and it is
    // the same analysis the cost model will read.
    let mut needed: Vec<BTreeSet<String>> = vec![BTreeSet::new(); steps.len() + 1];
    for i in (0..steps.len()).rev() {
        let mut want = needed[i + 1].clone();
        match &steps[i] {
            Step::Filter(e) => want.extend(free(e)),
            Step::Bind(name, e) => {
                want.remove(name);
                want.extend(free(e));
            }
            Step::Row(fields) => {
                want.clear();
                for (_, e, _) in fields {
                    want.extend(free(e));
                }
            }
            Step::Narrow(name, fallback) => {
                want.insert(name.clone());
                want.extend(free(fallback));
            }
        }
        needed[i] = want;
    }

    let source = match atoms.first().copied() {
        Some(core::Stmt::Atom { relation, .. }) => Op::Scan {
            relation: relation.clone(),
        },
        // "A rule with no positive atom is grounded on the unit relation."
        _ => Op::Ground,
    };
    let mut nodes = vec![Node {
        op: source,
        schema: needed[0].clone(),
    }];

    for (i, step) in steps.into_iter().enumerate() {
        let carry = &needed[i + 1];
        let op = match step {
            Step::Filter(expr) => Op::Filter { expr },
            Step::Bind(name, e) => {
                let mut fields: Vec<Field> = carry
                    .iter()
                    .filter(|v| **v != name)
                    .map(|v| Field {
                        name: v.clone(),
                        value: core::Expr::Var {
                            name: v.clone(),
                            span: e.span(),
                        },
                        ty: None,
                    })
                    .collect();
                if carry.contains(&name) {
                    let ty = typed.vars.get(&name).cloned();
                    fields.push(Field {
                        name: name.clone(),
                        value: e,
                        ty,
                    });
                }
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                Op::Map { fields }
            }
            Step::Row(fields) => {
                let mut fields: Vec<Field> = fields
                    .into_iter()
                    .map(|(name, value, ty)| Field { name, value, ty })
                    .collect();
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                Op::Map { fields }
            }
            Step::Narrow(name, fallback) => Op::Narrow { name, fallback },
        };
        // A filter hands on exactly what it was given; everything else states
        // its own row.
        let schema = match &op {
            // Neither changes the row's shape; the next `Map` is what projects.
            Op::Filter { .. } | Op::Narrow { .. } => {
                nodes.last().expect("a source node").schema.clone()
            }
            Op::Map { fields } => fields.iter().map(|f| f.name.clone()).collect(),
            Op::Scan { .. } | Op::Ground => unreachable!("only the source is a source"),
        };
        nodes.push(Node { op, schema });
    }

    Rule {
        nodes,
        key: key::rule(rule),
        span: rule.span,
    }
}

/// Push a step whose expression may be partial, lifting the partiality out.
fn push_total(
    steps: &mut Vec<Step>,
    fresh: &mut Fresh,
    make: impl Fn(core::Expr) -> Step,
    e: core::Expr,
) {
    let (pre, e) = total(fresh, e);
    steps.extend(pre);
    steps.push(make(e));
}

fn total_row(
    fresh: &mut Fresh,
    row: Vec<(String, core::Expr)>,
) -> (Vec<Step>, Vec<(String, core::Expr)>) {
    let mut pre = Vec::new();
    let mut out = Vec::new();
    for (name, e) in row {
        let (p, e) = total(fresh, e);
        pre.extend(p);
        out.push((name, e));
    }
    (pre, out)
}

/// Lift every partial subexpression out of `e`, leaving one that is total.
///
/// `/` and `%` are the partial operators: grasp says they yield `T` and a zero
/// divisor derives no row, grasp-dbsp says they yield `optional(T)`. The
/// difference is exactly `mapping.md`'s narrowing — bind, test, rebind:
///
/// ```text
/// v := a / b      →   map    v = a / b            -- optional(T)
///                     filter v != NONE
///                     map    v = coalesce(v, a)   -- T
/// ```
///
/// Innermost first, so a division inside a division is already a plain variable
/// by the time the outer one is lifted.
///
/// The `coalesce` default is **the dividend**, which is never read — the filter
/// above it saw to that — and is chosen because it has precisely the type the
/// division yields, in every case, without this pass having to work out what
/// that type is.
fn total(fresh: &mut Fresh, e: core::Expr) -> (Vec<Step>, core::Expr) {
    let mut steps = Vec::new();
    let out = lift(fresh, &mut steps, e);
    (steps, out)
}

fn lift(fresh: &mut Fresh, steps: &mut Vec<Step>, e: core::Expr) -> core::Expr {
    match e {
        core::Expr::Binary { op, lhs, rhs, span } => {
            let lhs = lift(fresh, steps, *lhs);
            let rhs = lift(fresh, steps, *rhs);
            let joined = core::Expr::Binary {
                op,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs),
                span,
            };
            if !matches!(op, BinOp::Div | BinOp::Rem) {
                return joined;
            }
            let name = fresh.next();
            steps.push(Step::Bind(name.clone(), joined));
            steps.push(Step::Narrow(name.clone(), lhs));
            core::Expr::Var { name, span }
        }
        core::Expr::Unary { op, operand, span } => core::Expr::Unary {
            op,
            operand: Box::new(lift(fresh, steps, *operand)),
            span,
        },
        core::Expr::Call { callee, args, span } => core::Expr::Call {
            callee,
            args: args.into_iter().map(|a| lift(fresh, steps, a)).collect(),
            span,
        },
        core::Expr::ArrayLit { elems, span } => core::Expr::ArrayLit {
            elems: elems.into_iter().map(|a| lift(fresh, steps, a)).collect(),
            span,
        },
        core::Expr::DictLit { entries, span } => core::Expr::DictLit {
            entries: entries
                .into_iter()
                .map(|(k, v)| (lift(fresh, steps, k), lift(fresh, steps, v)))
                .collect(),
            span,
        },
        core::Expr::RecordLit { fields, span } => core::Expr::RecordLit {
            fields: fields
                .into_iter()
                .map(|(n, v)| (n, lift(fresh, steps, v)))
                .collect(),
            span,
        },
        other => other,
    }
}
