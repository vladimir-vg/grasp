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
        input: usize,
        expr: core::Expr,
    },
    /// The complete output row. This is projection as well as binding: a
    /// variable absent from `fields` is gone.
    Map {
        input: usize,
        fields: Vec<Field>,
    },
    /// Index a stream by the variables the join ahead of it needs.
    ///
    /// The only entry to a join, and the second place projection happens: `val`
    /// is what is still live, so anything the rest of the rule has finished
    /// with is dropped here rather than carried through the join.
    MapIndex {
        input: usize,
        key: Vec<String>,
        val: Vec<String>,
    },
    /// Equi-join two indexed streams on the key they were indexed by.
    ///
    /// An empty key is a **cross product**, and needs nothing else: both sides
    /// indexed on nothing means every row of one meets every row of the other.
    /// `compilation.md` chose an empty key over a constant column precisely so
    /// that a cross would need no new node kind — "every key variable of a
    /// `join` is in both input schemas" holds vacuously of no key at all.
    Join {
        left: usize,
        right: usize,
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
        input: usize,
        name: String,
        fallback: core::Expr,
    },
}

impl Op {
    /// The nodes this one reads, for the invariant that inputs precede use.
    fn inputs(&self) -> Vec<usize> {
        match self {
            Op::Scan { .. } | Op::Ground => Vec::new(),
            Op::Filter { input, .. }
            | Op::Map { input, .. }
            | Op::MapIndex { input, .. }
            | Op::Narrow { input, .. } => vec![*input],
            Op::Join { left, right } => vec![*left, *right],
        }
    }
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
    }

    // The rank. `Diagnostic::UNIMPLEMENTED` lists the plan constructs in this
    // order too, so the two cannot drift far apart unnoticed.
    const RANK: &[&str] = &[
        "recursion",
        "aggregation",
        "negation",
        "unnesting",
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
fn components(vars: &[BTreeSet<String>]) -> Vec<Vec<usize>> {
    let mut owner: Vec<usize> = (0..vars.len()).collect();
    for i in 0..vars.len() {
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
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, o) in owner.iter().enumerate() {
        groups.entry(*o).or_default().push(i);
    }
    groups.into_values().collect()
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

/// One occurrence of a positive atom, with its own constraints lifted out.
///
/// After lifting, an atom **consumes nothing**: a column bound to a plain
/// variable binds it, and every other argument — a literal, an expression, a
/// repeat — has become an equality filter over a fresh name. That is what
/// `mapping.md` requires anyway, since nothing can push a computed value into a
/// relation scan, and it has a consequence worth stating: with no atom
/// consuming anything, `compilation.md`'s dependency partial order over atoms
/// is empty. Every rooting is valid and no component precedes another, so the
/// two rejections the optimizer is specified to make — "no valid rooting" and
/// "a cycle in the lifted order" — cannot arise. They are unreachable rather
/// than unimplemented.
struct Atom {
    relation: String,
    /// Column to the variable it binds.
    entry: Vec<(String, String)>,
    vars: BTreeSet<String>,
    key: String,
}

/// A statement that is not a positive atom: it consumes variables and may
/// produce one, but it is not in the spanning tree and has to be placed.
struct Dependent {
    kind: Kind,
    binds: Option<String>,
    consumes: BTreeSet<String>,
    key: String,
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

/// One step of the sequence a rule becomes, as a stack machine over streams:
/// [`Item::Enter`] and [`Item::Ground`] push one, [`Item::Join`] and
/// [`Item::Cross`] pop two and push one, and a [`Item::Dep`] rewrites the top.
///
/// Linear, but it describes a tree — which is the point. A post-order traversal
/// is exactly a stack machine, so the tree the optimizer chose survives as an
/// order without a second structure to keep in step with it.
#[derive(Clone, Copy)]
enum Item {
    Enter(usize),
    Ground,
    Join,
    Cross,
    Dep(usize),
}

fn plan_rule(
    typed: &infer::TypedRule,
    columns: &[(String, Type)],
) -> Result<Rule, Vec<Diagnostic>> {
    let rule = &typed.rule;
    let mut fresh = Fresh::new(typed);
    let mut atoms: Vec<Atom> = Vec::new();
    let mut deps: Vec<Dependent> = Vec::new();

    for stmt in &rule.body {
        match stmt {
            core::Stmt::Atom {
                relation,
                args,
                negated: false,
                ..
            } => {
                let mut entry = Vec::new();
                let mut bound: BTreeSet<String> = BTreeSet::new();
                for (column, arg) in args {
                    match arg {
                        core::Arg::Wildcard(_) => {}
                        core::Arg::Expr(core::Expr::Var { name, .. })
                            if bound.insert(name.clone()) =>
                        {
                            entry.push((column.clone(), name.clone()));
                        }
                        core::Arg::Expr(e) => {
                            let name = fresh.next();
                            entry.push((column.clone(), name.clone()));
                            let eq = equals(&name, e, e.span());
                            deps.push(Dependent {
                                kind: Kind::Filter,
                                binds: None,
                                consumes: free(&eq),
                                key: key::expr(&eq),
                                body: Body::Filter(eq),
                            });
                        }
                    }
                }
                atoms.push(Atom {
                    relation: relation.clone(),
                    vars: entry.iter().map(|(_, n)| n.clone()).collect(),
                    entry,
                    key: key::stmt(stmt),
                });
            }
            core::Stmt::Atom { .. } | core::Stmt::Input { .. } | core::Stmt::Assert { .. } => {}
            core::Stmt::Filter { expr, .. } => deps.push(Dependent {
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
                deps.push(Dependent {
                    kind: Kind::Match,
                    binds: Some(name.clone()),
                    consumes: free(e),
                    key: key::stmt(stmt),
                    body: Body::Bind(name.clone(), e.clone()),
                });
            }
        }
    }

    let head_vars: BTreeSet<String> = rule.head.args.iter().flat_map(|(_, e)| free(e)).collect();

    // The components of the weighted join graph, each planned and scored on its
    // own before any of them are crossed.
    // A rule with no atom is grounded on the unit relation, which is then its
    // join graph's root — so it is one component holding no atom rather than a
    // case beside the others.
    let mut comps = components(&atoms.iter().map(|a| a.vars.clone()).collect::<Vec<_>>());
    if comps.is_empty() {
        comps.push(Vec::new());
    }

    // Which component can satisfy each dependent by itself. One that no single
    // component can satisfy spans them, and is placed after the cross that
    // brings its last input into one stream.
    let produced: Vec<BTreeSet<String>> = comps
        .iter()
        .map(|comp| closure(&atoms, comp, &deps))
        .collect();
    let owners: Vec<Option<usize>> = deps
        .iter()
        .map(|d| (0..comps.len()).find(|c| d.consumes.is_subset(&produced[*c])))
        .collect();
    let spanning: BTreeSet<String> = deps
        .iter()
        .zip(&owners)
        .filter(|(_, o)| o.is_none())
        .flat_map(|(d, _)| d.consumes.clone())
        .collect();

    // Plan and score each component. Its peak is the cost of its own plan; its
    // exit width is how many of its variables are still live when it ends —
    // those in the head, and those a node placed after the cross consumes.
    let mut planned: Vec<Component> = Vec::new();
    for (ci, comp) in comps.iter().enumerate() {
        let own: Vec<usize> = (0..deps.len()).filter(|d| owners[*d] == Some(ci)).collect();
        let exit_vars: BTreeSet<String> = produced[ci]
            .iter()
            .filter(|v| head_vars.contains(*v) || spanning.contains(*v))
            .cloned()
            .collect();
        let trace = plan_component(&atoms, comp, &deps, &own, &head_vars, &spanning);
        let peak = cost(&trace, &atoms, &deps, &head_vars);
        let mut keys: Vec<&str> = comp.iter().map(|a| atoms[*a].key.as_str()).collect();
        keys.sort();
        planned.push(Component {
            trace,
            peak,
            exit: exit_vars.len(),
            key: keys.join("; "),
        });
    }

    // "Components are ordered by descending headroom", where headroom is
    // `peak - exit`: how far a component swells above what it leaves behind.
    // A component with exit width 0 goes first — it leaves nothing behind, so
    // its position is free on the peak. Ties by ascending exit, then by key.
    planned.sort_by(|a, b| {
        (a.exit > 0, -(a.peak as i64 - a.exit as i64), a.exit, &a.key).cmp(&(
            b.exit > 0,
            -(b.peak as i64 - b.exit as i64),
            b.exit,
            &b.key,
        ))
    });

    let mut trace: Vec<Item> = Vec::new();
    let mut placed = vec![false; deps.len()];
    for (i, d) in deps.iter().enumerate() {
        let _ = d;
        if owners[i].is_some() {
            // Its own component's plan already placed it.
            placed[i] = true;
        }
    }
    let spanning_deps: Vec<usize> = (0..deps.len()).filter(|d| owners[*d].is_none()).collect();
    let mut avail: BTreeSet<String> = BTreeSet::new();
    for (i, component) in planned.iter().enumerate() {
        trace.extend(component.trace.iter().copied());
        for item in &component.trace {
            if let Item::Enter(a) = item {
                avail.extend(atoms[*a].vars.iter().cloned());
            }
            if let Item::Dep(d) = item
                && let Some(v) = &deps[*d].binds
            {
                avail.insert(v.clone());
            }
        }
        if i > 0 {
            trace.push(Item::Cross);
        }
        place_ready(&mut avail, &deps, &spanning_deps, &mut placed, &mut trace);
    }

    debug_assert!(
        placed.iter().all(|p| *p),
        "a body statement consumes a variable nothing binds, which `infer`'s \
         safety check rejects before here"
    );

    Ok(lower(
        rule, &atoms, &mut deps, trace, &mut fresh, typed, columns,
    ))
}

/// One component, planned and scored.
struct Component {
    trace: Vec<Item>,
    peak: usize,
    exit: usize,
    key: String,
}

/// Everything a component can bind on its own: its atoms' variables, plus what
/// its own dependents produce from those, to a fixpoint.
fn closure(atoms: &[Atom], comp: &[usize], deps: &[Dependent]) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = comp
        .iter()
        .flat_map(|a| atoms[*a].vars.iter().cloned())
        .collect();
    loop {
        let mut grew = false;
        for d in deps {
            if let Some(v) = &d.binds
                && !out.contains(v)
                && d.consumes.is_subset(&out)
            {
                out.insert(v.clone());
                grew = true;
            }
        }
        if !grew {
            return out;
        }
    }
}

/// Plan one component: try every atom as root, and keep the cheapest.
///
/// "The optimizer tries every atom in the component as root. For each, a
/// post-order traversal of the rooted tree gives an evaluation order: children
/// before parents, each parent joining its children's results with its own
/// stream." The component's own dependents are placed into that order *before*
/// it is scored — a match produces a variable, so a component's peak is not its
/// own number until its matches are in it.
fn plan_component(
    atoms: &[Atom],
    comp: &[usize],
    deps: &[Dependent],
    own: &[usize],
    head_vars: &BTreeSet<String>,
    spanning: &BTreeSet<String>,
) -> Vec<Item> {
    let mut needed = head_vars.clone();
    needed.extend(spanning.iter().cloned());
    if comp.is_empty() {
        let mut trace = vec![Item::Ground];
        let mut placed = vec![false; deps.len()];
        let mut avail = BTreeSet::new();
        place_ready(&mut avail, deps, own, &mut placed, &mut trace);
        return trace;
    }
    let mut best: Option<(usize, String, Vec<Item>)> = None;
    for root in comp {
        let tree = spanning_tree(atoms, comp, *root);
        let mut trace = Vec::new();
        let mut placed = vec![false; deps.len()];
        let mut avail = BTreeSet::new();
        walk(
            atoms,
            &tree,
            *root,
            deps,
            own,
            &mut placed,
            &mut avail,
            &mut trace,
        );
        let c = cost(&trace, atoms, deps, &needed);
        let k = trace_key(&trace, atoms, deps);
        if best.as_ref().is_none_or(|(bc, bk, _)| (c, &k) < (*bc, bk)) {
            best = Some((c, k, trace));
        }
    }
    best.expect("a component holds at least one atom").2
}

/// A maximum-weight spanning tree of one component, rooted at `root` — Prim's
/// algorithm, always taking the heaviest edge from the visited set.
///
/// For an acyclic rule it coincides with a classical join tree; for a cyclic
/// one it is the best tree-shaped approximation, cutting the lightest edges to
/// break cycles. Nothing is lost by the cut: a join takes *every* variable its
/// two sides share, so an equality the tree does not carry is still enforced
/// where the two atoms finally meet.
fn spanning_tree(atoms: &[Atom], comp: &[usize], root: usize) -> BTreeMap<usize, Vec<usize>> {
    let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut visited: BTreeSet<usize> = BTreeSet::new();
    visited.insert(root);
    while visited.len() < comp.len() {
        let mut best: Option<(usize, &str, usize, usize)> = None;
        for u in comp.iter().filter(|u| !visited.contains(u)) {
            for v in visited.iter() {
                let weight = atoms[*u].vars.intersection(&atoms[*v].vars).count();
                if weight == 0 {
                    continue;
                }
                // Heaviest wins; ties by the structural key of the atom being
                // added, then of the one it attaches to. Never by position.
                let candidate = (weight, atoms[*u].key.as_str(), *u, *v);
                let better = match &best {
                    None => true,
                    Some((w, k, _, _)) => (weight, atoms[*u].key.as_str()) > (*w, k),
                };
                if better {
                    best = Some(candidate);
                }
            }
        }
        let (_, _, u, v) = best.expect("a component is connected");
        children.entry(v).or_default().push(u);
        visited.insert(u);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|k| atoms[*k].key.clone());
    }
    children
}

/// Post-order over the rooted tree, placing dependents as soon as their inputs
/// are in one stream.
#[allow(clippy::too_many_arguments)]
fn walk(
    atoms: &[Atom],
    tree: &BTreeMap<usize, Vec<usize>>,
    node: usize,
    deps: &[Dependent],
    own: &[usize],
    placed: &mut [bool],
    avail: &mut BTreeSet<String>,
    trace: &mut Vec<Item>,
) {
    trace.push(Item::Enter(node));
    avail.extend(atoms[node].vars.iter().cloned());
    place_ready(avail, deps, own, placed, trace);
    for child in tree.get(&node).into_iter().flatten() {
        walk(atoms, tree, *child, deps, own, placed, avail, trace);
        trace.push(Item::Join);
        place_ready(avail, deps, own, placed, trace);
    }
}

/// Every dependent whose inputs are now bound, until none is.
///
/// "Each is placed at the earliest point where all its inputs are bound. Early
/// is always right: a filter that runs sooner shrinks everything downstream."
/// When several become ready together they go by kind, then by structural key.
fn place_ready(
    avail: &mut BTreeSet<String>,
    deps: &[Dependent],
    pool: &[usize],
    placed: &mut [bool],
    trace: &mut Vec<Item>,
) {
    loop {
        let mut ready: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|d| !placed[*d] && deps[*d].consumes.is_subset(avail))
            .collect();
        if ready.is_empty() {
            return;
        }
        ready.sort_by(|a, b| (&deps[*a].kind, &deps[*a].key).cmp(&(&deps[*b].kind, &deps[*b].key)));
        let chosen = ready[0];
        placed[chosen] = true;
        if let Some(v) = &deps[chosen].binds {
            avail.insert(v.clone());
        }
        trace.push(Item::Dep(chosen));
    }
}

/// "The maximum number of distinct variables in scope at any step."
///
/// A variable is born when the atom or match producing it is visited, stays
/// alive while any unvisited node or the head still needs it, and is projected
/// away once nothing does. Structural: no cardinality estimates, no statistics
/// — what it knows is which plans blow up regardless of the data.
fn cost(
    trace: &[Item],
    atoms: &[Atom],
    deps: &[Dependent],
    needed_after: &BTreeSet<String>,
) -> usize {
    let mut later: Vec<BTreeSet<String>> = vec![BTreeSet::new(); trace.len() + 1];
    later[trace.len()] = needed_after.clone();
    for i in (0..trace.len()).rev() {
        let mut want = later[i + 1].clone();
        match trace[i] {
            Item::Dep(d) => {
                if let Some(v) = &deps[d].binds {
                    want.remove(v);
                }
                want.extend(deps[d].consumes.iter().cloned());
            }
            // Not a kill, for the reason the same step in `lower` gives.
            Item::Enter(_) | Item::Ground | Item::Join | Item::Cross => {}
        }
        later[i] = want;
    }
    let mut born: BTreeSet<String> = BTreeSet::new();
    let mut peak = 0;
    for (i, item) in trace.iter().enumerate() {
        match item {
            Item::Enter(a) => born.extend(atoms[*a].vars.iter().cloned()),
            Item::Dep(d) => {
                if let Some(v) = &deps[*d].binds {
                    born.insert(v.clone());
                }
            }
            Item::Ground | Item::Join | Item::Cross => {}
        }
        peak = peak.max(born.intersection(&later[i + 1]).count());
    }
    peak
}

/// A candidate order's key: the sequence of its nodes' keys.
fn trace_key(trace: &[Item], atoms: &[Atom], deps: &[Dependent]) -> String {
    trace
        .iter()
        .map(|item| match item {
            Item::Enter(a) => atoms[*a].key.clone(),
            Item::Dep(d) => deps[*d].key.clone(),
            Item::Ground => "()".to_string(),
            Item::Join => "><".to_string(),
            Item::Cross => "**".to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
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

/// A step in the flat sequence a rule becomes, after partiality is lifted out.
enum Step {
    Enter(usize),
    Ground,
    Join,
    Cross,
    Filter(core::Expr),
    /// Bind one name, keeping everything else that is still live.
    Bind(String, core::Expr),
    /// See [`Op::Narrow`].
    Narrow(String, core::Expr),
    /// The head: replace the row wholesale.
    Row(Vec<(String, core::Expr, Option<Type>)>),
}

#[allow(clippy::too_many_arguments)]
fn lower(
    rule: &core::Rule,
    atoms: &[Atom],
    deps: &mut [Dependent],
    trace: Vec<Item>,
    fresh: &mut Fresh,
    typed: &infer::TypedRule,
    columns: &[(String, Type)],
) -> Rule {
    // "The primary producer is whichever comes first in the post-order. Every
    // other producer becomes an equality filter." Writing `x` twice says both
    // computations agree; one supplies the value and the rest check it.
    let mut produced: BTreeSet<String> = BTreeSet::new();
    for item in &trace {
        let Item::Dep(d) = item else { continue };
        let Some(name) = deps[*d].binds.clone() else {
            continue;
        };
        if produced.insert(name.clone()) {
            continue;
        }
        let Body::Bind(_, e) = &deps[*d].body else {
            continue;
        };
        let eq = equals(&name, e, e.span());
        deps[*d].kind = Kind::Filter;
        deps[*d].binds = None;
        deps[*d].consumes = free(&eq);
        deps[*d].key = key::expr(&eq);
        deps[*d].body = Body::Filter(eq);
    }

    let mut steps: Vec<Step> = Vec::new();
    for item in &trace {
        match item {
            Item::Enter(a) => steps.push(Step::Enter(*a)),
            Item::Ground => steps.push(Step::Ground),
            Item::Join => steps.push(Step::Join),
            Item::Cross => steps.push(Step::Cross),
            Item::Dep(d) => match &deps[*d].body {
                Body::Filter(e) => push_total(&mut steps, fresh, Step::Filter, e.clone()),
                Body::Bind(name, e) => {
                    let name = name.clone();
                    push_total(
                        &mut steps,
                        fresh,
                        move |e| Step::Bind(name.clone(), e),
                        e.clone(),
                    )
                }
            },
        }
    }
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

    // A pre-pass over the stack, ignoring projection, to learn what each join
    // joins *on*. Its key is what the two streams share — every shared variable,
    // not only the tree edge that put them together, which is what enforces the
    // equalities a spanning tree had to cut.
    let mut shapes: Vec<BTreeSet<String>> = Vec::new();
    let mut keys: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Enter(a) => shapes.push(atoms[*a].vars.clone()),
            Step::Ground => shapes.push(BTreeSet::new()),
            Step::Join | Step::Cross => {
                let right = shapes.pop().expect("a stream to join");
                let left = shapes.pop().expect("a stream to join");
                let key: Vec<String> = left.intersection(&right).cloned().collect();
                debug_assert!(
                    !matches!(step, Step::Cross) || key.is_empty(),
                    "components share no variable, so a cross has no key"
                );
                keys.insert(i, key);
                shapes.push(left.union(&right).cloned().collect());
            }
            Step::Filter(_) => {}
            Step::Bind(name, _) | Step::Narrow(name, _) => {
                shapes.last_mut().expect("a stream").insert(name.clone());
            }
            Step::Row(fields) => {
                *shapes.last_mut().expect("a stream") =
                    fields.iter().map(|(n, _, _)| n.clone()).collect();
            }
        }
    }

    // Liveness, backwards: what every step still needs. This is projection — a
    // variable absent from the set is gone — and it is the same analysis the
    // cost model reads. A join's key counts as consumed, or a variable shared
    // by two atoms and used nowhere else would be dropped before they met.
    let mut needed: Vec<BTreeSet<String>> = vec![BTreeSet::new(); steps.len() + 1];
    for i in (0..steps.len()).rev() {
        let mut want = needed[i + 1].clone();
        match &steps[i] {
            // An atom does *not* kill the variables it binds. A join key is
            // bound on both sides — that is what makes it a key — so the atom
            // reached first is not its only producer, and killing there would
            // project the key away before the two streams ever met. What
            // bounds the entry projection is the atom's own variables, which
            // it is intersected with.
            Step::Enter(_) | Step::Ground => {}
            Step::Join | Step::Cross => {
                want.extend(keys[&i].iter().cloned());
            }
            Step::Filter(e) => want.extend(free(e)),
            Step::Bind(name, e) => {
                want.remove(name);
                want.extend(free(e));
            }
            Step::Narrow(name, fallback) => {
                want.insert(name.clone());
                want.extend(free(fallback));
            }
            Step::Row(fields) => {
                want.clear();
                for (_, e, _) in fields {
                    want.extend(free(e));
                }
            }
        }
        needed[i] = want;
    }

    let mut nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let push = |nodes: &mut Vec<Node>, op: Op, schema: BTreeSet<String>| {
        nodes.push(Node { op, schema });
        nodes.len() - 1
    };

    for (i, step) in steps.into_iter().enumerate() {
        let carry = &needed[i + 1];
        match step {
            Step::Enter(a) => {
                let atom = &atoms[a];
                let scan = push(
                    &mut nodes,
                    Op::Scan {
                        relation: atom.relation.clone(),
                    },
                    atom.entry.iter().map(|(c, _)| c.clone()).collect(),
                );
                // Out of the relation's column names and into the rule's
                // variables, keeping only what something still wants.
                let fields: Vec<Field> = atom
                    .entry
                    .iter()
                    .filter(|(_, v)| carry.contains(v))
                    .map(|(column, v)| Field {
                        name: v.clone(),
                        value: core::Expr::Var {
                            name: column.clone(),
                            span: rule.span,
                        },
                        ty: None,
                    })
                    .collect();
                let schema = fields.iter().map(|f| f.name.clone()).collect();
                let renamed = push(
                    &mut nodes,
                    Op::Map {
                        input: scan,
                        fields,
                    },
                    schema,
                );
                stack.push(renamed);
            }
            Step::Ground => {
                let g = push(&mut nodes, Op::Ground, BTreeSet::new());
                stack.push(g);
            }
            Step::Join | Step::Cross => {
                let right = stack.pop().expect("a stream to join");
                let left = stack.pop().expect("a stream to join");
                let key = keys[&i].clone();
                let side = |nodes: &mut Vec<Node>, input: usize| {
                    let val: Vec<String> = nodes[input]
                        .schema
                        .iter()
                        .filter(|v| carry.contains(*v) && !key.contains(*v))
                        .cloned()
                        .collect();
                    let schema: BTreeSet<String> = key.iter().chain(val.iter()).cloned().collect();
                    let op = Op::MapIndex {
                        input,
                        key: key.clone(),
                        val,
                    };
                    nodes.push(Node { op, schema });
                    nodes.len() - 1
                };
                let l = side(&mut nodes, left);
                let r = side(&mut nodes, right);
                let schema: BTreeSet<String> = nodes[l]
                    .schema
                    .union(&nodes[r].schema)
                    .filter(|v| carry.contains(*v))
                    .cloned()
                    .collect();
                let j = push(&mut nodes, Op::Join { left: l, right: r }, schema);
                stack.push(j);
            }
            Step::Filter(expr) => {
                let input = *stack.last().expect("a stream to filter");
                let schema = nodes[input].schema.clone();
                let n = push(&mut nodes, Op::Filter { input, expr }, schema);
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Narrow(name, fallback) => {
                let input = *stack.last().expect("a stream to narrow");
                let schema = nodes[input].schema.clone();
                let n = push(
                    &mut nodes,
                    Op::Narrow {
                        input,
                        name,
                        fallback,
                    },
                    schema,
                );
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Bind(name, e) => {
                let input = *stack.last().expect("a stream to map");
                let mut fields: Vec<Field> = nodes[input]
                    .schema
                    .iter()
                    .filter(|v| **v != name && carry.contains(*v))
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
                    fields.push(Field {
                        name: name.clone(),
                        value: e,
                        ty: typed.vars.get(&name).cloned(),
                    });
                }
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                let schema = fields.iter().map(|f| f.name.clone()).collect();
                let n = push(&mut nodes, Op::Map { input, fields }, schema);
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Row(fields) => {
                let input = *stack.last().expect("a stream to project");
                let mut fields: Vec<Field> = fields
                    .into_iter()
                    .map(|(name, value, ty)| Field { name, value, ty })
                    .collect();
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                let schema = fields.iter().map(|f| f.name.clone()).collect();
                let n = push(&mut nodes, Op::Map { input, fields }, schema);
                *stack.last_mut().expect("a stream") = n;
            }
        }
    }

    debug_assert_eq!(stack.len(), 1, "a computation DAG is single-sink");
    debug_assert!(
        nodes
            .iter()
            .enumerate()
            .all(|(i, n)| n.op.inputs().iter().all(|input| *input < i)),
        "inputs precede use"
    );

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
