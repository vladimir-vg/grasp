//! Plan — `docs/grasp/compilation.md`.
//!
//! Takes the typed core and produces, per rule, an ordered acyclic
//! single-sink sequence of column-level operations, plus the relation-level
//! structure that says how those sequences combine. Types are erased here:
//! nothing below this point carries one.
//!
//! Three things about the shape of this pass are load-bearing.
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
//! **Partial operators are lifted, not special-cased at emission.** `a / b` is
//! `T` in grasp and `optional(T)` in grasp-dbsp — "all errors inside grasp rule
//! bodies should cause silent row drop" — so every division is bound, tested
//! and rebound *here*, as ordinary nodes. Doing it in the emitter would hide a
//! filter from the liveness analysis and from the node order, and `v :: T` will
//! need the same construction.

use crate::ast::{Aggregator, BinOp, Lit, Type};
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
/// Groups are in name order rather than source order, for the reason every
/// other ordering here is structural: two programs whose declarations differ
/// only in sequence are one program.
#[derive(Debug)]
pub struct Plan {
    pub groups: Vec<Group>,
}

/// Relations that have to be built together.
///
/// Almost always one relation. More than one, or one that reaches itself, is a
/// strongly connected component of the dependency graph — mutually recursive
/// relations, which `mapping.md` says become one `circuit` instantiated by one
/// `fixpoint`. Nothing else in the plan distinguishes them, because nothing
/// else needs to: a recursive relation's rules are planned exactly as any
/// other's, and only emission cares that they iterate.
#[derive(Debug)]
pub struct Group {
    /// Name-ordered.
    pub relations: Vec<Relation>,
    pub recursive: bool,
    /// What the group's rules read from outside itself, name-ordered. These
    /// become the circuit's ordinary parameters. Empty unless `recursive`.
    pub reads: Vec<String>,
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

/// What a narrowing's `coalesce` falls back to — a value that is never read.
///
/// Two forms because two callers know different things. A division has its
/// dividend to hand, which has exactly the type the division yields, so nothing
/// has to work that type out. An assertion knows the type and has no value, so
/// emission builds one — which it can, `mapping.md` listing a definite value of
/// every type.
#[derive(Debug)]
pub enum Definite {
    Like(core::Expr),
    Of(Type),
}

/// One aggregate: a variable, the aggregator that fills it, and what it folds.
#[derive(Debug, Clone)]
pub struct Agg {
    pub out: String,
    pub function: Aggregator,
    /// `None` for `count<>`, which "takes no argument and counts rows".
    pub arg: Option<core::Expr>,
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
    /// Collapse every weight to one.
    ///
    /// Emitted in exactly one place: immediately before an aggregate. Every
    /// other operator preserves the invariant an aggregate depends on — that a
    /// row's weight is the number of satisfying assignments it stands for —
    /// but two of them establish it only where the row is an *injective*
    /// encoding of the assignment. An atom that omits a column, and an unnest
    /// over equal elements, both produce several rows for one assignment, and
    /// a weight-scaled `sum` would count them all.
    ///
    /// It is over every variable the body binds, which is why liveness carries
    /// them all this far. A narrower dedup at each source would be cheaper and
    /// would keep the optimiser discriminating; it is not here because the
    /// answers have to be pinned before an optimisation can be judged against
    /// them.
    Distinct {
        input: usize,
    },
    /// Group, fold, and flatten back.
    ///
    /// One node for what `compilation.md` calls three, and for the same reason
    /// the antijoin is one: "grouping is the index key, so the group columns
    /// are indexed first, folded, then flattened back", and nothing may come
    /// between. Several aggregates share the node because they share the group
    /// — that is the whole of grasp's implicit grouping.
    ///
    /// `group` is `semantics.md`'s "the head's non-aggregate columns", read as
    /// the variables those columns read and decided in `infer`, which is where
    /// the scope rule that makes it well defined is read.
    Aggregate {
        input: usize,
        group: Vec<String>,
        aggs: Vec<Agg>,
    },
    /// One row per element of a collection, with the rest of the row carried
    /// alongside each.
    ///
    /// A `flat_map` whose function is a `map_array`: the operator fans out and
    /// the `map_array` builds the rows it fans out to, which is what puts the rest of
    /// the row beside each element. `kind` decides what an element is —
    /// an array's value, or a dict entry's key and value.
    Unnest {
        input: usize,
        over: core::Expr,
        binds: Vec<String>,
        kind: core::UnnestKind,
    },
    /// Keep the rows of `left` that `right` has no match for, and flatten.
    ///
    /// Two grasp-dbsp operators under one node, like [`Op::Narrow`]: `antijoin`
    /// yields an *indexed* stream and a relation is flat, so "the `map` after
    /// the `antijoin` is not optional". Both sides are [`Op::MapIndex`] nodes
    /// keyed alike, and an empty key subtracts one whole stream from another —
    /// which is what a negated proposition means.
    Antijoin {
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
    /// The `coalesce` default is never read, the filter having seen to that.
    /// It is carried because grasp-dbsp typechecks it anyway.
    Narrow {
        input: usize,
        name: String,
        /// The type to convert into before testing — `mapping.md`'s first
        /// operator, needed for a `json` source and not for one that is already
        /// the `optional(T)` the check produces.
        cast: Option<Type>,
        /// What to rebind the variable as, once the rows with no value are gone.
        ///
        /// `None` where the assertion **keeps its wrapper**: `optional(A) ::
        /// optional(B)` leaves an `optional(B)`, so there is nothing to
        /// coalesce to and absence is one of the answers rather than one of the
        /// rows to drop. That case is emitted differently throughout — see
        /// [`crate::emit`].
        definite: Option<Definite>,
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
            | Op::Narrow { input, .. }
            | Op::Aggregate { input, .. }
            | Op::Distinct { input }
            | Op::Unnest { input, .. } => vec![*input],
            Op::Join { left, right } | Op::Antijoin { left, right } => vec![*left, *right],
        }
    }
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Plan a typed program.
pub fn plan(typed: infer::Typed) -> Result<Plan, Vec<Diagnostic>> {
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

    // The dependency graph over relations, from which the components come:
    // "an edge r -> s when a rule for r has s in its body".
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in typed.relations.keys() {
        edges.entry(name.clone()).or_default();
    }
    for rs in rules.values() {
        for r in rs {
            let out = edges.entry(r.rule.head.relation.clone()).or_default();
            for stmt in &r.rule.body {
                if let core::Stmt::Atom { relation, .. } = stmt {
                    out.insert(relation.clone());
                }
            }
        }
    }
    let components = scc(&edges);
    stratified(&components, &rules)?;

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

    // Fold the relations into their components. A component is recursive when
    // it holds more than one relation, or when its one relation reaches itself.
    let mut groups = Vec::new();
    for members in components {
        let recursive = members.len() > 1 || members.first().is_some_and(|m| edges[m].contains(m));
        let mut reads: BTreeSet<String> = BTreeSet::new();
        if recursive {
            for m in &members {
                for target in &edges[m] {
                    if !members.contains(target) {
                        reads.insert(target.clone());
                    }
                }
            }
        }
        let mut held = Vec::new();
        let mut rest = Vec::new();
        for r in relations {
            if members.contains(&r.name) {
                held.push(r);
            } else {
                rest.push(r);
            }
        }
        relations = rest;
        groups.push(Group {
            relations: held,
            recursive,
            reads: reads.into_iter().collect(),
        });
    }
    debug_assert!(relations.is_empty(), "every relation is in a component");
    groups.sort_by(|a, b| a.relations[0].name.cmp(&b.relations[0].name));

    Ok(Plan { groups })
}

/// Reject a negation that reaches back into its own recursive component.
///
/// `semantics.md`'s stratification, step 5, and the whole point of the other
/// four: "A negative edge inside a component is a relation whose definition
/// depends on the *absence* of something not yet computed." `p <- not p` has no
/// least fixpoint — neither `p` empty nor `p` full satisfies it — so the
/// language forbids the shape rather than picking one of the answers.
///
/// An aggregate marks an edge the same way, for the same reason — it would read
/// a partial value — and is checked here too.
fn stratified(
    components: &[BTreeSet<String>],
    rules: &BTreeMap<String, Vec<&infer::TypedRule>>,
) -> Result<(), Vec<Diagnostic>> {
    for component in components {
        for head in component {
            for rule in rules.get(head).into_iter().flatten() {
                // "Mark the edge NEGATIVE when `s` appears under `not`, or is
                // the subject of an aggregate." A rule's aggregate is grouped
                // over its whole body, so every relation in it is a subject.
                let aggregating = rule.rule.body.iter().any(|s| {
                    matches!(
                        s,
                        core::Stmt::Match {
                            rhs: core::Rhs::Aggregate { .. },
                            ..
                        }
                    )
                });
                for stmt in &rule.rule.body {
                    let core::Stmt::Atom {
                        relation,
                        negated,
                        span,
                        ..
                    } = stmt
                    else {
                        continue;
                    };
                    if !component.contains(relation) {
                        continue;
                    }
                    if !negated {
                        if !aggregating {
                            continue;
                        }
                        // "An aggregate over a relation still being computed
                        //  would read a partial value, and which partial value
                        //  would depend on evaluation order."
                        return Err(vec![Diagnostic::error(
                            Pass::Plan,
                            *span,
                            format!(
                                "aggregate over `{relation}` is in the same recursive \
                                 component as this rule"
                            ),
                        )]);
                    }
                    // The spec words this for two relations. A relation that
                    // negates itself is the same fault and wants its own
                    // sentence; both end alike, which is what a reader — and a
                    // fixture — takes hold of.
                    let message = if relation == head {
                        format!(
                            "`{head}` is defined by its own negation at line {} — negation \
                             cannot cross a recursive cycle",
                            span.line
                        )
                    } else {
                        format!(
                            "`{head}` and `{relation}` are mutually recursive, and \
                             `{relation}` is negated at line {} — negation cannot cross a \
                             recursive cycle",
                            span.line
                        )
                    };
                    return Err(vec![Diagnostic::error(Pass::Plan, *span, message)]);
                }
            }
        }
    }
    Ok(())
}

/// The strongly connected components of the dependency graph — Tarjan's.
///
/// Each is a set of mutually recursive relations; a relation in no cycle is its
/// own component. `semantics.md` names this as step 2 of stratification, and
/// step 5 — rejecting a NEGATIVE edge internal to a component — belongs here
/// too, in [`stratified`], which negation and aggregation — the only two things
/// that mark an edge negative — both feed.
///
/// Everything iterates in name order, so the partition is a function of the
/// program rather than of a traversal.
fn scc(edges: &BTreeMap<String, BTreeSet<String>>) -> Vec<BTreeSet<String>> {
    struct Tarjan<'a> {
        edges: &'a BTreeMap<String, BTreeSet<String>>,
        index: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        on: BTreeSet<&'a str>,
        stack: Vec<&'a str>,
        next: usize,
        out: Vec<BTreeSet<String>>,
    }
    impl<'a> Tarjan<'a> {
        fn visit(&mut self, v: &'a str) {
            self.index.insert(v, self.next);
            self.low.insert(v, self.next);
            self.next += 1;
            self.stack.push(v);
            self.on.insert(v);
            for w in self.edges.get(v).into_iter().flatten() {
                // A body may mention a relation the map does not hold only if
                // `infer` let an undeclared one through, which it does not.
                let w = self
                    .edges
                    .get_key_value(w.as_str())
                    .map(|(k, _)| k.as_str());
                let Some(w) = w else { continue };
                if !self.index.contains_key(w) {
                    self.visit(w);
                    let l = self.low[w];
                    let e = self.low.get_mut(v).expect("visited");
                    *e = (*e).min(l);
                } else if self.on.contains(w) {
                    let i = self.index[w];
                    let e = self.low.get_mut(v).expect("visited");
                    *e = (*e).min(i);
                }
            }
            if self.low[v] == self.index[v] {
                let mut component = BTreeSet::new();
                while let Some(w) = self.stack.pop() {
                    self.on.remove(w);
                    component.insert(w.to_string());
                    if w == v {
                        break;
                    }
                }
                self.out.push(component);
            }
        }
    }
    let mut t = Tarjan {
        edges,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    for v in edges.keys() {
        if !t.index.contains_key(v.as_str()) {
            t.visit(v);
        }
    }
    t.out
}

fn to_args(row: &[(String, core::Expr)]) -> Vec<(String, core::Arg)> {
    row.iter()
        .map(|(c, e)| (c.clone(), core::Arg::Expr(e.clone())))
        .collect()
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
    /// What it produces. More than one only for an unnest, which binds a key
    /// and a value together.
    binds: Vec<String>,
    consumes: BTreeSet<String>,
    key: String,
    body: Body,
}

/// Ordered as `compilation.md` orders dependent nodes that become ready
/// together: "negated atoms, then filters, then matches, then aggregates" —
/// most selective first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Negated,
    Filter,
    Match,
}

enum Body {
    Filter(core::Expr),
    Bind(String, core::Expr),
    /// A negated atom.
    ///
    /// `entry` maps the relation's columns to the key variables both sides will
    /// be indexed by, one column per variable. `equal` are the columns a
    /// repeated variable ties together — a constraint on the negated relation,
    /// not a second key field. `computed` are the key variables this rule has to
    /// work out on the left first, because the atom wrote an expression there
    /// rather than a variable.
    Negated {
        relation: String,
        entry: Vec<(String, String)>,
        equal: Vec<(String, String)>,
        computed: Vec<(String, core::Expr)>,
    },
    /// `v :: T`, where a check could settle it.
    Assert(infer::Assert),
    /// `(v) := *arr` or `(k, v) := **d`.
    Unnest {
        over: core::Expr,
        vars: Vec<String>,
        kind: core::UnnestKind,
    },
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
    /// Every aggregate of the rule, which share one group and so are one step.
    /// Always after the other dependents — an aggregate consumes a whole group
    /// and must come after everything contributing to it — and before any
    /// dependent that reads what it produced.
    Aggregate,
}

fn plan_rule(
    typed: &infer::TypedRule,
    columns: &[(String, Type)],
) -> Result<Rule, Vec<Diagnostic>> {
    let rule = &typed.rule;
    let mut fresh = Fresh::new(typed);
    let mut atoms: Vec<Atom> = Vec::new();
    let mut deps: Vec<Dependent> = Vec::new();
    let mut aggs: Vec<Agg> = Vec::new();

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
                                binds: Vec::new(),
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
            core::Stmt::Atom {
                relation,
                args,
                negated: true,
                ..
            } => {
                // Every argument becomes a key variable, and the two sides are
                // indexed by the same ones. A plain variable is already on the
                // left; anything else is computed there first, which is what
                // lets a literal argument and an expression take one path.
                let mut entry: Vec<(String, String)> = Vec::new();
                let mut equal = Vec::new();
                let mut computed = Vec::new();
                let mut consumes = BTreeSet::new();
                for (column, arg) in args {
                    match arg {
                        core::Arg::Wildcard(_) => {}
                        core::Arg::Expr(core::Expr::Var { name, .. }) => {
                            consumes.insert(name.clone());
                            // One key field per variable. A variable written
                            // twice is not two keys — it says the two columns
                            // agree, which is the negated relation's business
                            // and becomes a filter over it.
                            match entry.iter().find(|(_, v)| v == name) {
                                Some((first, _)) => equal.push((column.clone(), first.clone())),
                                None => entry.push((column.clone(), name.clone())),
                            }
                        }
                        core::Arg::Expr(e) => {
                            let name = fresh.next();
                            entry.push((column.clone(), name.clone()));
                            consumes.extend(free(e));
                            computed.push((name, e.clone()));
                        }
                    }
                }
                deps.push(Dependent {
                    kind: Kind::Negated,
                    binds: Vec::new(),
                    consumes,
                    key: key::stmt(stmt),
                    body: Body::Negated {
                        relation: relation.clone(),
                        entry,
                        equal,
                        computed,
                    },
                });
            }
            core::Stmt::Input { .. } | core::Stmt::Assert { .. } => {}
            core::Stmt::Filter { expr, .. } => deps.push(Dependent {
                kind: Kind::Filter,
                binds: Vec::new(),
                consumes: free(expr),
                key: key::expr(expr),
                body: Body::Filter(expr.clone()),
            }),
            core::Stmt::Match { lhs, rhs, .. } => {
                if let core::Pattern::Unnest { vars, kind, .. } = lhs {
                    let core::Rhs::Expr(e) = rhs else {
                        unreachable!("an aggregate has no pattern to unnest")
                    };
                    deps.push(Dependent {
                        kind: Kind::Match,
                        binds: vars.clone(),
                        consumes: free(e),
                        key: key::stmt(stmt),
                        body: Body::Unnest {
                            over: e.clone(),
                            vars: vars.clone(),
                            kind: *kind,
                        },
                    });
                    continue;
                }
                if let core::Pattern::Array { elems, rest, span } = lhs {
                    let core::Rhs::Expr(subject) = rhs else {
                        unreachable!("an aggregate has no pattern to destructure")
                    };
                    destructure_array(&mut deps, typed, subject, elems, rest, *span);
                    continue;
                }
                if let core::Pattern::Dict { fields, rest, span }
                | core::Pattern::Record { fields, rest, span } = lhs
                {
                    let core::Rhs::Expr(subject) = rhs else {
                        unreachable!("an aggregate has no pattern to destructure")
                    };
                    let record = matches!(lhs, core::Pattern::Record { .. });
                    destructure(&mut deps, typed, subject, fields, rest, record, *span);
                    continue;
                }
                let core::Pattern::Var { name, .. } = lhs else {
                    unreachable!("a pattern is a variable, an unnest or a destructure")
                };
                // Every aggregate in a rule shares one group, so they are one
                // step rather than one dependent each — and that step is always
                // last, an aggregate consuming a whole group and so having to
                // come after everything contributing to it.
                let e = match rhs {
                    core::Rhs::Expr(e) => e,
                    core::Rhs::Aggregate { function, arg, .. } => {
                        aggs.push(Agg {
                            out: name.clone(),
                            function: *function,
                            arg: arg.clone(),
                        });
                        continue;
                    }
                };
                deps.push(Dependent {
                    kind: Kind::Match,
                    binds: vec![name.clone()],
                    consumes: free(e),
                    key: key::stmt(stmt),
                    body: Body::Bind(name.clone(), e.clone()),
                });
            }
        }
    }

    // `v :: T` is a filter over a variable already bound, so it is a dependent
    // like any other — placed as early as the variable exists, which is what
    // makes "narrowed for everything after it" one type for the whole rule.
    for assert in &typed.asserts {
        deps.push(Dependent {
            kind: Kind::Filter,
            binds: Vec::new(),
            consumes: [assert.variable.clone()].into_iter().collect(),
            key: format!("{} :: {}", assert.variable, assert.ty),
            body: Body::Assert(assert.clone()),
        });
    }

    // What is still wanted when the body is done, which is what the cost model
    // and the exit widths are measured against.
    //
    // An aggregate demands *everything*: it ranges over the body's assignments,
    // so the deduplication before it is over every variable the body binds and
    // nothing may be projected away first. Saying so here is what keeps the
    // scoring honest — the alternative is a model that discriminates between
    // rootings whose real peaks are identical. It also means the cost model has
    // nothing to say about a rule with an aggregate, and the rooting falls to
    // the structural key; deduplicating at the sources instead is what would
    // give it its discrimination back.
    let mut head_vars: BTreeSet<String> =
        rule.head.args.iter().flat_map(|(_, e)| free(e)).collect();
    if !aggs.is_empty() {
        head_vars.extend(atoms.iter().flat_map(|a| a.vars.iter().cloned()));
        head_vars.extend(deps.iter().flat_map(|d| d.binds.iter().cloned()));
    }
    let head_vars = head_vars;

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
            if let Item::Dep(d) = item {
                avail.extend(deps[*d].binds.iter().cloned());
            }
        }
        if i > 0 {
            trace.push(Item::Cross);
        }
        place_ready(&mut avail, &deps, &spanning_deps, &mut placed, &mut trace);
    }

    // The aggregates go here, after everything that feeds them. A filter over
    // an aggregate's result — a rule's `having` — could not be placed before
    // this point and is placed now.
    if !aggs.is_empty() {
        trace.push(Item::Aggregate);
        avail.extend(aggs.iter().map(|a| a.out.clone()));
        place_ready(&mut avail, &deps, &spanning_deps, &mut placed, &mut trace);
    }

    debug_assert!(
        placed.iter().all(|p| *p),
        "a body statement consumes a variable nothing binds, which `infer`'s \
         safety check rejects before here"
    );

    // Name order, so several aggregates sharing a group are combined in an
    // order the program fixes rather than the file.
    aggs.sort_by(|a, b| a.out.cmp(&b.out));
    Ok(lower(
        rule, &atoms, &mut deps, aggs, trace, &mut fresh, typed, columns,
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
            if !d.binds.is_empty()
                && d.binds.iter().any(|v| !out.contains(v))
                && d.consumes.is_subset(&out)
            {
                out.extend(d.binds.iter().cloned());
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
                // added.
                //
                // **Known problem:** and there it stops. The parent is not in
                // the comparison, so when one atom ties against two visited
                // parents the winner is whichever `visited` yields first — a
                // set of atom indices, which is body source order. That is the
                // same leak `Fresh` has, in the same pass, and
                // `compilation.md`'s "never by position" covers both.
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
        avail.extend(deps[chosen].binds.iter().cloned());
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
                for v in &deps[d].binds {
                    want.remove(v);
                }
                want.extend(deps[d].consumes.iter().cloned());
            }
            // Not a kill, for the reason the same step in `lower` gives.
            Item::Enter(_) | Item::Ground | Item::Join | Item::Cross | Item::Aggregate => {}
        }
        later[i] = want;
    }
    let mut born: BTreeSet<String> = BTreeSet::new();
    let mut peak = 0;
    for (i, item) in trace.iter().enumerate() {
        match item {
            Item::Enter(a) => born.extend(atoms[*a].vars.iter().cloned()),
            Item::Dep(d) => {
                born.extend(deps[*d].binds.iter().cloned());
            }
            Item::Ground | Item::Join | Item::Cross | Item::Aggregate => {}
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
            Item::Aggregate => "<>".to_string(),
            Item::Join => "><".to_string(),
            Item::Cross => "**".to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One destructure, as the dependents it stands for.
///
/// `semantics.md`'s table, performed here rather than in `desugar` because two
/// of the three parts need types: the narrowing after a dict `get` has to name
/// the value type, and a bound record remainder is a literal over the fields
/// left, which are only known once the subject's type is.
///
/// Ordering takes care of itself. A narrow reads the variable its bind
/// produced, so `place_ready` cannot put it first; the size filter reads only
/// the subject, so it lands as early as the subject exists.
fn destructure(
    deps: &mut Vec<Dependent>,
    typed: &infer::TypedRule,
    subject: &core::Expr,
    fields: &[(String, String)],
    rest: &core::Rest,
    record: bool,
    span: Span,
) {
    let consumes = free(subject);

    // "Exactly these keys" is a claim about how many entries the dict has, and
    // a dict's size is data — so it is a filter, and a row whose dict is the
    // wrong size is simply not derived. A record's fields are its *type*, so
    // the same claim about a record was settled in `infer` and nothing is
    // emitted for it here.
    if !record && matches!(rest, core::Rest::None) {
        let size = core::Expr::Binary {
            op: BinOp::Eq,
            lhs: Box::new(core::Expr::Call {
                callee: core::Builtin::Length,
                args: vec![subject.clone()],
                span,
            }),
            rhs: Box::new(core::Expr::Lit {
                value: Lit::Int(fields.len() as i64),
                span,
            }),
            span,
        };
        deps.push(Dependent {
            kind: Kind::Filter,
            binds: Vec::new(),
            consumes: consumes.clone(),
            key: key::expr(&size),
            body: Body::Filter(size),
        });
    }

    for (key, var) in fields {
        let get = core::Expr::Call {
            callee: if record {
                core::Builtin::RecordGet
            } else {
                core::Builtin::DictGet
            },
            args: vec![
                subject.clone(),
                core::Expr::Lit {
                    value: Lit::Str(key.clone()),
                    span,
                },
            ],
            span,
        };
        deps.push(Dependent {
            kind: Kind::Match,
            binds: vec![var.clone()],
            consumes: consumes.clone(),
            key: format!("{var} := {}", key::expr(&get)),
            body: Body::Bind(var.clone(), get),
        });

        // `record:get` gives the field's type; `dict:get` gives `optional(V)`,
        // and the pattern's promise is that the key is there — so the row
        // without it is dropped, which is `optional(V) :: V`.
        if !record {
            let ty = settled(typed, var);
            deps.push(Dependent {
                kind: Kind::Filter,
                binds: Vec::new(),
                consumes: [var.clone()].into_iter().collect(),
                key: format!("{var} :: {ty}"),
                body: Body::Assert(infer::Assert {
                    variable: var.clone(),
                    ty,
                    cast: false,
                    span,
                }),
            });
        }
    }

    // A dict's remainder is not known until the program runs, so it is the
    // subtraction itself — an emission-time `filter_array` over the entries.
    if let (false, core::Rest::Bind(name)) = (record, rest) {
        let without = core::Expr::Call {
            callee: core::Builtin::DictWithoutKeys,
            args: vec![
                subject.clone(),
                core::Expr::ArrayLit {
                    elems: fields
                        .iter()
                        .map(|(key, _)| core::Expr::Lit {
                            value: Lit::Str(key.clone()),
                            span,
                        })
                        .collect(),
                    span,
                },
            ],
            span,
        };
        deps.push(Dependent {
            kind: Kind::Match,
            binds: vec![name.clone()],
            consumes,
            key: format!("{name} := {}", key::expr(&without)),
            body: Body::Bind(name.clone(), without),
        });
        return;
    }

    // A record's remainder is a set of fields known at compile time, so it is a
    // literal rather than the builtin subtraction a dict needs. `infer` typed
    // it, and reading the field names back off that type is what keeps the two
    // from deciding it separately.
    if let core::Rest::Bind(name) = rest {
        let Type::Record(left) = settled(typed, name) else {
            unreachable!("`infer` types a bound remainder as a record")
        };
        let value = core::Expr::RecordLit {
            fields: left
                .iter()
                .map(|(field, _)| {
                    (
                        field.clone(),
                        core::Expr::Call {
                            callee: core::Builtin::RecordGet,
                            args: vec![
                                subject.clone(),
                                core::Expr::Lit {
                                    value: Lit::Str(field.clone()),
                                    span,
                                },
                            ],
                            span,
                        },
                    )
                })
                .collect(),
            span,
        };
        deps.push(Dependent {
            kind: Kind::Match,
            binds: vec![name.clone()],
            consumes,
            key: format!("{name} := {}", key::expr(&value)),
            body: Body::Bind(name.clone(), value),
        });
    }
}

/// One array destructure, as the dependents it stands for.
///
/// Positional where a dict's is named, and with a size check that is `=` or
/// `>=` rather than only `=`: `[x, y]` says the array has exactly two elements
/// and `[x, y, *]` says it has at least two. Both are filters, an array's
/// length being data — the record asymmetry does not arise here.
fn destructure_array(
    deps: &mut Vec<Dependent>,
    typed: &infer::TypedRule,
    subject: &core::Expr,
    elems: &[String],
    rest: &core::Rest,
    span: Span,
) {
    let consumes = free(subject);
    let length = core::Expr::Call {
        callee: core::Builtin::Length,
        args: vec![subject.clone()],
        span,
    };
    let want = core::Expr::Lit {
        value: Lit::Int(elems.len() as i64),
        span,
    };
    let size = core::Expr::Binary {
        op: if matches!(rest, core::Rest::None) {
            BinOp::Eq
        } else {
            BinOp::Ge
        },
        lhs: Box::new(length),
        rhs: Box::new(want),
        span,
    };
    deps.push(Dependent {
        kind: Kind::Filter,
        binds: Vec::new(),
        consumes: consumes.clone(),
        key: key::expr(&size),
        body: Body::Filter(size),
    });

    let at = |i: usize| core::Expr::Call {
        callee: core::Builtin::ArrayGet,
        args: vec![
            subject.clone(),
            core::Expr::Lit {
                value: Lit::Int(i as i64),
                span,
            },
        ],
        span,
    };
    for (i, var) in elems.iter().enumerate() {
        let get = at(i);
        deps.push(Dependent {
            kind: Kind::Match,
            binds: vec![var.clone()],
            consumes: consumes.clone(),
            key: format!("{var} := {}", key::expr(&get)),
            body: Body::Bind(var.clone(), get),
        });
        // The size filter has already made the position good, but the type
        // does not know that: `array:get` is `optional(E)` and the variable is
        // an `E`. Same shape as a dict pattern's, and for the same reason.
        let ty = settled(typed, var);
        deps.push(Dependent {
            kind: Kind::Filter,
            binds: Vec::new(),
            consumes: [var.clone()].into_iter().collect(),
            key: format!("{var} :: {ty}"),
            body: Body::Assert(infer::Assert {
                variable: var.clone(),
                ty,
                cast: false,
                span,
            }),
        });
    }

    if let core::Rest::Bind(name) = rest {
        let drop = core::Expr::Call {
            callee: core::Builtin::ArrayDrop,
            args: vec![
                subject.clone(),
                core::Expr::Lit {
                    value: Lit::Int(elems.len() as i64),
                    span,
                },
            ],
            span,
        };
        deps.push(Dependent {
            kind: Kind::Match,
            binds: vec![name.clone()],
            consumes,
            key: format!("{name} := {}", key::expr(&drop)),
            body: Body::Bind(name.clone(), drop),
        });
    }
}

/// A bound variable's settled type.
///
/// `plan` runs only on a rule `check` accepted, and `check` reports every
/// variable that did not settle — so by here every one this asks about has.
fn settled(typed: &infer::TypedRule, name: &str) -> Type {
    typed
        .vars
        .get(name)
        .cloned()
        .expect("`check` settles every bound variable before `plan` runs")
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

/// The variables an expression reads, for a caller outside this module.
pub fn collect_free(e: &core::Expr, out: &mut BTreeSet<String>) {
    out.extend(free(e));
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
/// Numbered rather than derived from the expression, because a name built from
/// an expression would be unbounded in length.
///
/// **Known problem: the number comes from position in the body.** `plan_rule`
/// hands these out walking `rule.body` in source order, so two atoms carrying
/// non-variable arguments take `v0` and `v1` by which was written first. Those
/// names reach the emitted record fields, and — through `key::expr` over the
/// equality each becomes — the dependent's structural key as well, which is the
/// one place `compilation.md` says source position may never reach. Two
/// programs that mean the same then emit different text. No wrong answer comes
/// of it, and `normalization.yaml` has no case with two such atoms, which is
/// why it does not show.
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
    Narrow {
        name: String,
        cast: Option<Type>,
        definite: Option<Definite>,
    },
    /// The head: replace the row wholesale.
    Row(Vec<(String, core::Expr, Option<Type>)>),
    /// See [`Op::Aggregate`]. The group is worked out from liveness.
    Aggregate(Vec<Agg>),
    /// See [`Op::Unnest`].
    Unnest {
        over: core::Expr,
        vars: Vec<String>,
        kind: core::UnnestKind,
    },
    /// See [`Op::Antijoin`]. `entry` is column to key variable, and `equal`
    /// the columns a repeated variable ties together.
    Antijoin {
        relation: String,
        entry: Vec<(String, String)>,
        equal: Vec<(String, String)>,
    },
}

#[allow(clippy::too_many_arguments)]
fn lower(
    rule: &core::Rule,
    atoms: &[Atom],
    deps: &mut [Dependent],
    aggs: Vec<Agg>,
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
        // Only a match can compete: an unnest has no equality form, and one
        // binding a name already produced would be a rebinding rather than a
        // second computation of the same value.
        let [name] = deps[*d].binds.as_slice() else {
            continue;
        };
        let name = name.clone();
        if produced.insert(name.clone()) {
            continue;
        }
        let Body::Bind(_, e) = &deps[*d].body else {
            continue;
        };
        let eq = equals(&name, e, e.span());
        deps[*d].kind = Kind::Filter;
        deps[*d].binds = Vec::new();
        deps[*d].consumes = free(&eq);
        deps[*d].key = key::expr(&eq);
        deps[*d].body = Body::Filter(eq);
    }

    let mut steps: Vec<Step> = Vec::new();
    let mut aggs = Some(aggs);
    for item in &trace {
        match item {
            Item::Aggregate => {
                // A partial operator in an aggregate's argument is lifted here
                // as it is anywhere else, so a zero divisor removes the
                // assignment *before* it is folded. Left inline it would keep
                // the row in the group with an absent projection, and a
                // sibling aggregate would see a row grasp never derived.
                let mut lifted = Vec::new();
                for a in aggs.take().expect("one aggregate step per rule") {
                    let arg = match a.arg {
                        Some(e) => {
                            let (pre, e) = total(fresh, e);
                            steps.extend(pre);
                            Some(e)
                        }
                        None => None,
                    };
                    lifted.push(Agg { arg, ..a });
                }
                steps.push(Step::Aggregate(lifted));
            }
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
                // An `optional` target is exactly the assertion that keeps its
                // wrapper, and nothing else: `optional(T) :: T` asserts `T`,
                // and `json :: T` is gated on `holdable`, which excludes
                // `optional`. So the type says which shape this is.
                Body::Assert(a) => steps.push(Step::Narrow {
                    name: a.variable.clone(),
                    cast: a.cast.then(|| a.ty.clone()),
                    definite: (!matches!(a.ty, Type::Optional(_)))
                        .then(|| Definite::Of(a.ty.clone())),
                }),
                Body::Unnest { over, vars, kind } => {
                    let (vars, kind) = (vars.clone(), *kind);
                    push_total(
                        &mut steps,
                        fresh,
                        move |over| Step::Unnest {
                            over,
                            vars: vars.clone(),
                            kind,
                        },
                        over.clone(),
                    )
                }
                Body::Negated {
                    relation,
                    entry,
                    equal,
                    computed,
                } => {
                    // What the atom wrote as an expression is worked out on the
                    // left first, so that both sides can be indexed by a name.
                    for (name, e) in computed {
                        let name = name.clone();
                        push_total(
                            &mut steps,
                            fresh,
                            move |e| Step::Bind(name.clone(), e),
                            e.clone(),
                        );
                    }
                    steps.push(Step::Antijoin {
                        relation: relation.clone(),
                        entry: entry.clone(),
                        equal: equal.clone(),
                    });
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
    // Every name the body binds — an aggregate's dedup key. `Step::Aggregate`
    // deliberately adds nothing: its outputs are produced *by* the aggregate,
    // not by the body it ranges over.
    //
    // **Known problem:** the pass is forward and flat, so a `Bind` or `Narrow`
    // standing *after* the aggregate — a filter's lifted temporary, a narrowing
    // on a result — joins the set anyway, which is the opposite of what the
    // paragraph above claims. It is harmless only by accident: every consumer
    // intersects this with a real node schema, so a name no upstream stream
    // carries is dropped rather than looked up.
    let mut bound: BTreeSet<String> = BTreeSet::new();
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Enter(a) => {
                bound.extend(atoms[*a].vars.iter().cloned());
                shapes.push(atoms[*a].vars.clone())
            }
            Step::Ground => shapes.push(BTreeSet::new()),
            Step::Join | Step::Cross => {
                let right = shapes.pop().expect("a stream to join");
                let left = shapes.pop().expect("a stream to join");
                // Whatever the two sides share, which for two components is
                // usually nothing — that is what made them two. Usually, not
                // always: a dependent node placed inside one component can bind
                // a variable a later component's atom also binds, and then the
                // two really do meet. `(v) := *arr` followed by an atom over
                // `v` is exactly that, and joining on `v` is what the rule
                // means. The components are drawn from the atoms alone, so this
                // is the one place that can see it.
                let key: Vec<String> = left.intersection(&right).cloned().collect();
                keys.insert(i, key);
                shapes.push(left.union(&right).cloned().collect());
            }
            // Neither changes the row's shape: an antijoin keeps the rows of
            // the stream it subtracts from, and it subtracts nothing else.
            Step::Filter(_) | Step::Antijoin { .. } => {}
            Step::Aggregate(aggs) => {
                let shape = shapes.last_mut().expect("a stream");
                shape.extend(aggs.iter().map(|a| a.out.clone()));
            }
            Step::Unnest { vars, .. } => {
                bound.extend(vars.iter().cloned());
                let shape = shapes.last_mut().expect("a stream");
                shape.extend(vars.iter().cloned());
            }
            Step::Bind(name, _) | Step::Narrow { name, .. } => {
                bound.insert(name.clone());
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
            // The key is what both sides are indexed by, so it has to reach
            // here alive even where nothing downstream wants it.
            Step::Antijoin { entry, .. } => {
                want.extend(entry.iter().map(|(_, v)| v.clone()));
            }
            Step::Unnest { over, vars, .. } => {
                for v in vars {
                    want.remove(v);
                }
                want.extend(free(over));
            }
            Step::Aggregate(aggs) => {
                for a in aggs {
                    want.remove(&a.out);
                    if let Some(e) = &a.arg {
                        want.extend(free(e));
                    }
                }
                // Everything the body binds reaches the aggregate, because the
                // deduplication below is over the whole assignment: projecting
                // first would collapse two assignments that differ only in a
                // variable nothing reads, and a weight-scaled `sum` would then
                // count them once.
                want.extend(bound.iter().cloned());
            }
            Step::Bind(name, e) => {
                want.remove(name);
                want.extend(free(e));
            }
            Step::Narrow { name, definite, .. } => {
                want.insert(name.clone());
                if let Some(Definite::Like(e)) = definite {
                    want.extend(free(e));
                }
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
            Step::Antijoin {
                relation,
                entry,
                equal,
            } => {
                let input = *stack.last().expect("a stream to subtract from");
                let key: Vec<String> = {
                    let mut k: Vec<String> = entry.iter().map(|(_, v)| v.clone()).collect();
                    k.sort();
                    k
                };
                // The left keeps what is still wanted; the right keeps nothing
                // at all, an antijoin reading only whether a key is there.
                let val: Vec<String> = nodes[input]
                    .schema
                    .iter()
                    .filter(|v| carry.contains(*v) && !key.contains(*v))
                    .cloned()
                    .collect();
                let left_schema: BTreeSet<String> = key.iter().chain(val.iter()).cloned().collect();
                let left = push(
                    &mut nodes,
                    Op::MapIndex {
                        input,
                        key: key.clone(),
                        val,
                    },
                    left_schema.clone(),
                );
                let columns: BTreeSet<String> = entry
                    .iter()
                    .map(|(c, _)| c.clone())
                    .chain(equal.iter().flat_map(|(a, b)| [a.clone(), b.clone()]))
                    .collect();
                let mut side = push(
                    &mut nodes,
                    Op::Scan {
                        relation: relation.clone(),
                    },
                    columns.clone(),
                );
                // A variable the atom wrote twice: the two columns must agree,
                // which is a constraint on the negated relation and belongs on
                // this side, before it is indexed.
                for (a, b) in &equal {
                    let expr = core::Expr::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(core::Expr::Var {
                            name: a.clone(),
                            span: rule.span,
                        }),
                        rhs: Box::new(core::Expr::Var {
                            name: b.clone(),
                            span: rule.span,
                        }),
                        span: rule.span,
                    };
                    side = push(
                        &mut nodes,
                        Op::Filter { input: side, expr },
                        columns.clone(),
                    );
                }
                let scan = side;
                // Into the *left's* names, so the two keys are one type.
                let fields: Vec<Field> = entry
                    .iter()
                    .map(|(column, v)| Field {
                        name: v.clone(),
                        value: core::Expr::Var {
                            name: column.clone(),
                            span: rule.span,
                        },
                        ty: None,
                    })
                    .collect();
                let renamed = push(
                    &mut nodes,
                    Op::Map {
                        input: scan,
                        fields,
                    },
                    key.iter().cloned().collect(),
                );
                let right = push(
                    &mut nodes,
                    Op::MapIndex {
                        input: renamed,
                        key: key.clone(),
                        val: Vec::new(),
                    },
                    key.iter().cloned().collect(),
                );
                let schema: BTreeSet<String> = left_schema
                    .into_iter()
                    .filter(|v| carry.contains(v))
                    .collect();
                let n = push(&mut nodes, Op::Antijoin { left, right }, schema);
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Unnest { over, vars, kind } => {
                let input = *stack.last().expect("a stream to fan out");
                let mut schema: BTreeSet<String> = nodes[input]
                    .schema
                    .iter()
                    .filter(|v| carry.contains(*v))
                    .cloned()
                    .collect();
                schema.extend(vars.iter().filter(|v| carry.contains(*v)).cloned());
                let n = push(
                    &mut nodes,
                    Op::Unnest {
                        input,
                        over,
                        binds: vars,
                        kind,
                    },
                    schema,
                );
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Aggregate(aggs) => {
                let input = *stack.last().expect("a stream to group");
                let schema = nodes[input].schema.clone();
                let input = push(&mut nodes, Op::Distinct { input }, schema);
                // The group, decided in `infer` — "the head's non-aggregate
                // columns", read as the variables those columns read.
                //
                // Liveness computes the same set from the other side: what is
                // still wanted here and no aggregate produced. The two agree
                // only because the scope rule makes them, by refusing anything
                // that would keep a body variable alive past the group. The
                // assertion is that rule's proof, and it fails loudly rather
                // than drifting if a later change to projection breaks it.
                let outs: BTreeSet<&str> = aggs.iter().map(|a| a.out.as_str()).collect();
                let group: Vec<String> = typed.group.iter().cloned().collect();
                debug_assert_eq!(
                    typed.group,
                    nodes[input]
                        .schema
                        .iter()
                        .filter(|v| carry.contains(*v) && !outs.contains(v.as_str()))
                        .cloned()
                        .collect::<BTreeSet<String>>(),
                    "the group `infer` decided and the one liveness reaches must agree"
                );
                let mut schema: BTreeSet<String> = group.iter().cloned().collect();
                schema.extend(aggs.iter().map(|a| a.out.clone()));
                schema.retain(|v| carry.contains(v));
                let n = push(&mut nodes, Op::Aggregate { input, group, aggs }, schema);
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Filter(expr) => {
                let input = *stack.last().expect("a stream to filter");
                let schema = nodes[input].schema.clone();
                let n = push(&mut nodes, Op::Filter { input, expr }, schema);
                *stack.last_mut().expect("a stream") = n;
            }
            Step::Narrow {
                name,
                cast,
                definite,
            } => {
                let input = *stack.last().expect("a stream to narrow");
                let schema = nodes[input].schema.clone();
                let n = push(
                    &mut nodes,
                    Op::Narrow {
                        input,
                        name,
                        cast,
                        definite,
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
            steps.push(Step::Narrow {
                name: name.clone(),
                cast: None,
                definite: Some(Definite::Like(lhs)),
            });
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
