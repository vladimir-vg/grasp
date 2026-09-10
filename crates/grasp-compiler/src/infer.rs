//! Inference — `docs/grasp/inference.md`.
//!
//! Gives every relation column and every variable a type, and performs the
//! safety check. Types are erased below this point; what survives is the
//! relation table the emitter needs to declare its record shapes.
//!
//! Three things about the shape of this pass are load-bearing.
//!
//! **The fixpoint's gate is the mechanism, not an optimisation.** A rule is
//! processed only once every relation its body mentions is `known`, and a
//! relation becomes known from a spec, a fact, or a rule head that has been
//! processed. A relation defined *only* by a recursive rule therefore never
//! becomes known, its rule is never processed, and that is precisely how
//! ``has no non-recursive rule and no typespec`` is produced. A general
//! monotone fixpoint over `Unknown` would type it happily and accept a program
//! the specification rejects.
//!
//! **The fixpoint is silent.** It composes types and reports nothing; every
//! diagnostic comes from one `check` pass afterwards, with the settled
//! table in hand. Otherwise the same mistake is reported once per round.
//!
//! **At most one diagnostic per declaration.** The test harness matches an
//! exact set, and several fixtures have two independent faults —
//! `edge(src: 1, dst: 2)` has two untyped literals, `big(v: x) <- x > 3` has an
//! unbound variable *and* an untyped `3`. The first problem in phase order wins
//! and the rest of that declaration types as [`Ty::Error`], which composes with
//! everything and settles to nothing.
//!
//! Not implemented yet: the runtime filters of phase 4. `v :: T` reports
//! unimplemented, because a filter is the one statement the compiler adds and
//! *where* it goes is the optimizer's discipline — inserting it before `plan`
//! exists would mean deciding that twice.

use crate::ast::{Aggregator, BinOp, Lit, Type, UnOp};
use crate::core;
use crate::diag::{Diagnostic, Pass, Span};
use crate::key;
use crate::ty::{Narrowing, Open, Shape, Ty, assignable, compose, impose, narrows, settle};
use std::collections::{BTreeMap, BTreeSet};

/// The typed core.
///
/// Specs are gone: a spec is a claim about a relation, this is where claims are
/// settled, and carrying a second copy would let the two disagree.
#[derive(Debug)]
pub struct Typed {
    pub relations: BTreeMap<String, Relation>,
    pub decls: Vec<Decl>,
}

#[derive(Debug)]
pub struct Relation {
    /// Sorted by name — `compilation.md`'s structural key sorts columns, and a
    /// record's fields are a set, so declaration order is not information.
    pub columns: Vec<(String, Type)>,
    pub kind: Kind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// `r(cols:) <- input` — the rows come from outside.
    Input,
    /// Defined by facts, by rules, or by both. A relation with only a spec is
    /// derived and empty.
    Derived,
}

#[derive(Debug)]
pub enum Decl {
    Fact(core::Fact),
    Rule(TypedRule),
}

/// One `v :: T` that became a runtime filter.
///
/// An assertion the value already satisfied leaves none of these: it was a
/// compile-time check and there is nothing to emit. `infer` is where the
/// narrowing table is read, so it is where the decision belongs — the plan
/// needs the answer, not the reasoning.
#[derive(Debug, Clone)]
pub struct Assert {
    pub variable: String,
    pub ty: Type,
    /// Whether the value has to be converted before it can be tested — a
    /// `json` source, and not an `optional(T)` one, which is already the shape
    /// the check produces.
    pub cast: bool,
    /// What the check is over. Decided here because the target type does not
    /// say: `json :: array(i64)` and `array(json) :: array(i64)` ask for the
    /// same type and test different things.
    pub shape: Shape,
    pub span: Span,
}

#[derive(Debug)]
pub struct TypedRule {
    pub rule: core::Rule,
    /// Every variable the rule binds, and its type.
    pub vars: BTreeMap<String, Type>,
    /// What each call to a [partial][`core::Builtin::partial`] builtin settles
    /// to, by the span of the call.
    ///
    /// `dict:get` yields a `V` and derives no row for a missing key, so the
    /// call has to become a node — a filter and a rebinding — and the rebinding
    /// needs a definite value of `V` to coalesce to. Division gets that from
    /// its dividend, which has the result's type; a dict lookup has no operand
    /// of its value type, so this pass says what it is. It is the channel
    /// [`Assert`] already is: the plan needs the answer, not the reasoning.
    pub partials: BTreeMap<Span, Type>,
    /// The assertions that became runtime filters, in body order.
    pub asserts: Vec<Assert>,
    /// What an aggregate in this rule groups by — the variables the head's
    /// non-aggregate columns read. Empty for a rule with no aggregate.
    ///
    /// Decided here for the reason [`Assert`] is: the scope rule is read in
    /// this pass, so this pass owns the answer. The plan derives the same set
    /// from liveness and asserts the two agree, which is a check rather than a
    /// second implementation.
    pub group: BTreeSet<String>,
}

pub fn infer(program: core::Program) -> Result<Typed, Vec<Diagnostic>> {
    let mut cx = Cx::new(program);
    cx.solve();
    let diagnostics = cx.check();
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    cx.finish()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A relation's columns as the fixpoint knows them so far.
struct Columns {
    /// Declared by a spec, which is the answer rather than a contribution: "a
    /// spec never *overrides* what a rule infers — it constrains it", so rules
    /// are checked against it and never refine it.
    declared: bool,
    cols: BTreeMap<String, Ty>,
}

struct Cx {
    specs: BTreeMap<String, (Vec<(String, Type)>, Span)>,
    /// Relations with an `r(cols:) <- input` rule, and where it is.
    inputs: BTreeMap<String, Span>,
    facts: Vec<core::Fact>,
    /// Rules that are not `<- input`.
    rules: Vec<core::Rule>,
    known: BTreeMap<String, Columns>,
}

impl Cx {
    fn new(program: core::Program) -> Cx {
        let mut cx = Cx {
            specs: BTreeMap::new(),
            inputs: BTreeMap::new(),
            facts: Vec::new(),
            rules: Vec::new(),
            known: BTreeMap::new(),
        };
        for decl in program {
            match decl {
                core::Decl::Spec(s) => {
                    cx.specs.insert(s.relation, (s.columns, s.span));
                }
                core::Decl::Fact(f) => cx.facts.push(f),
                core::Decl::Rule(r) => {
                    if r.body.iter().any(|s| matches!(s, core::Stmt::Input { .. })) {
                        cx.inputs.insert(r.head.relation.clone(), r.span);
                    } else {
                        cx.rules.push(r);
                    }
                }
            }
        }
        cx
    }

    /// Every relation the program declares, by any means.
    fn declared(&self) -> BTreeSet<&str> {
        self.specs
            .keys()
            .map(String::as_str)
            .chain(self.inputs.keys().map(String::as_str))
            .chain(self.facts.iter().map(|f| f.relation.as_str()))
            .chain(self.rules.iter().map(|r| r.head.relation.as_str()))
            .collect()
    }

    // -- the fixpoint -------------------------------------------------------

    fn solve(&mut self) {
        // "known = { r : columns(r) for each r with a `:: relation(...)` spec }"
        // — and an input relation starts known from the spec it is required to
        // have, which is the same entry.
        for (name, (columns, _)) in &self.specs {
            self.known.insert(
                name.clone(),
                Columns {
                    declared: true,
                    cols: columns
                        .iter()
                        .map(|(c, t)| (c.clone(), Ty::known(t)))
                        .collect(),
                },
            );
        }

        // The lattice has no finite height — `r(x: v) <- r(x: w), v := [w]`
        // deepens a column by one array every round — and `inference.md` gives
        // no termination argument. Bounded rather than trusted.
        let limit = self.round_limit();
        for _ in 0..limit {
            let mut changed = false;
            // "A fact counts as a rule here", and it creates an entry the same
            // way a head does — which is what lets `reachable(node: 1)` seed
            // the loop for the recursive rule below it.
            for fact in std::mem::take(&mut self.facts) {
                let cols = self.fact_columns(&fact);
                changed |= self.refine(&fact.relation, cols);
                self.facts.push(fact);
            }
            for rule in std::mem::take(&mut self.rules) {
                if self.body_relations_known(&rule) {
                    let typed = self.type_rule(&rule);
                    changed |= self.refine(&rule.head.relation, typed.head);
                }
                self.rules.push(rule);
            }
            if !changed {
                return;
            }
        }
    }

    /// How many rounds are enough.
    ///
    /// Each round can add one column or deepen one by a constructor, so the
    /// bound is the work available plus room to notice it has stopped.
    fn round_limit(&self) -> usize {
        let columns: usize = self
            .facts
            .iter()
            .map(|f| f.args.len())
            .chain(self.rules.iter().map(|r| r.head.args.len()))
            .sum();
        (columns + self.specs.len() + 2) * 4
    }

    fn body_relations_known(&self, rule: &core::Rule) -> bool {
        rule.body.iter().all(|s| match s {
            core::Stmt::Atom { relation, .. } => self.known.contains_key(relation),
            _ => true,
        })
    }

    /// Merge a contribution into what is known, reporting whether it moved.
    fn refine(&mut self, relation: &str, cols: BTreeMap<String, Ty>) -> bool {
        let entry = self.known.entry(relation.to_string()).or_insert(Columns {
            declared: false,
            cols: BTreeMap::new(),
        });
        // A declared relation is the answer; a rule that disagrees is checked
        // against it later, not folded into it.
        if entry.declared {
            return false;
        }
        let mut changed = false;
        for (col, ty) in cols {
            match entry.cols.get(&col) {
                Some(existing) => {
                    let merged = compose(existing, &ty).unwrap_or(Ty::Error);
                    if merged != *existing {
                        entry.cols.insert(col, merged);
                        changed = true;
                    }
                }
                None => {
                    entry.cols.insert(col, ty);
                    changed = true;
                }
            }
        }
        changed
    }

    fn fact_columns(&self, fact: &core::Fact) -> BTreeMap<String, Ty> {
        fact.args
            .iter()
            .map(|(c, e)| (c.clone(), self.expr_ty(e, &BTreeMap::new()).0))
            .collect()
    }

    fn column_ty(&self, relation: &str, column: &str) -> Option<&Ty> {
        self.known.get(relation)?.cols.get(column)
    }
}

/// What typing one rule produced.
struct RuleTypes {
    vars: BTreeMap<String, Ty>,
    /// What the statements *contribute*, which is not always what a variable
    /// is: an assertion narrows `x` to `i64` while the atom that bound it goes
    /// on contributing `json`.
    ///
    /// This, and not `vars`, is what the next round is seeded with, and it is
    /// where a conflict between two statements is a mistake. Seeding from
    /// `vars` would make `doc(k: k, d: x)` beside `x :: i64` report `x` as both
    /// `json` and `i64` — the two are not disagreeing, they are saying
    /// different things.
    source: BTreeMap<String, Ty>,
    /// See [`TypedRule::partials`].
    partials: BTreeMap<Span, Type>,
    filters: Vec<Assert>,
    head: BTreeMap<String, Ty>,
    /// The first thing wrong, which is the only thing reported.
    fault: Option<Diagnostic>,
}

// ---------------------------------------------------------------------------
// Phase 1 — one rule's constraints, composed
// ---------------------------------------------------------------------------

impl Cx {
    /// One rule's types, to a fixpoint.
    ///
    /// Collects every constraint the rule places on its variables, composes
    /// them, and reports what the head columns come out as. Runs during the
    /// program-level fixpoint, where `fault` is discarded, and again during
    /// checking, where it is the rule's one diagnostic.
    ///
    /// A single pass reads the body top to bottom, so a variable used above the
    /// statement that binds it is `Unknown` where it is read — and a rule body
    /// is a set, so that made a program's acceptance depend on where a line was
    /// written. `q(v: n) <- n := a * 2, r(a: a)` was rejected and the same two
    /// statements swapped were not.
    ///
    /// So the pass runs again on what it learned until nothing moves. The
    /// earlier rounds are **silent** — the same discipline the program-level
    /// fixpoint keeps, and for the same reason: a fault seen before the last
    /// round may be an artefact of what that round had not yet read.
    ///
    /// Bounded rather than trusted, as [`Cx::round_limit`] is: within one rule
    /// the lattice has no finite height either, since `x := [y]` beside
    /// `y := [x]` deepens both by an array every round. A rule that does not
    /// settle falls out with its variables open, which phase 3 reports.
    ///
    /// Assertions need none of this — an assertion's type is written down, so
    /// [`Cx::type_pass`] reads them all before it reads anything else, and a
    /// narrowing is order-insensitive by construction rather than by
    /// iteration.
    fn type_rule(&self, rule: &core::Rule) -> RuleTypes {
        let limit = self.round_limit();
        let mut seed: BTreeMap<String, Ty> = BTreeMap::new();
        for _ in 0..limit {
            let next: BTreeMap<String, Ty> = self
                .type_pass(rule, &seed)
                .source
                .into_iter()
                .filter(|(_, ty)| !ty.is_poisoned())
                .collect();
            if next == seed {
                break;
            }
            seed = next;
        }
        self.type_pass(rule, &seed)
    }

    fn type_pass(&self, rule: &core::Rule, seed: &BTreeMap<String, Ty>) -> RuleTypes {
        let mut fault: Option<Diagnostic> = None;
        let note = |d: Diagnostic, fault: &mut Option<Diagnostic>| {
            if fault.is_none() {
                *fault = Some(d);
            }
        };

        // **Assertions first.** A rule body is a set, so `x :: i64` is a claim
        // about the whole rule rather than about what follows it — and an
        // assertion's type is *written down* rather than inferred, so it can be
        // read before anything else is. That is what makes the claim
        // order-insensitive: every statement sees the narrowed type, however
        // the two were arranged.
        //
        // Two assertions on one variable compose, which is order-insensitive
        // too; where they cannot, neither is the truth and both are reported.
        let mut narrowed: BTreeMap<String, Ty> = BTreeMap::new();
        for stmt in &rule.body {
            let core::Stmt::Assert { variable, ty, span } = stmt else {
                continue;
            };
            let want = Ty::known(ty);
            let merged = match narrowed.get(variable) {
                Some(already) => match compose(already, &want) {
                    Ok(t) => t,
                    Err(c) => {
                        note(
                            Diagnostic::error(
                                Pass::Infer,
                                *span,
                                format!(
                                    "variable `{variable}` is asserted as `{}` and as `{}`",
                                    c.left, c.right
                                ),
                            ),
                            &mut fault,
                        );
                        Ty::Error
                    }
                },
                None => want,
            };
            narrowed.insert(variable.clone(), merged);
        }

        // Two tables. `source` is what the statements *contribute*, which is
        // where a conflict between them is a mistake; `vars` is what a
        // variable *is*, which for an asserted one is what the assertion says.
        // Keeping them apart is what lets `doc(k: k, d: x)` and `x :: i64`
        // stand together instead of reporting `x` as both `json` and `i64`.
        let mut source: BTreeMap<String, Ty> = seed.clone();
        let mut vars: BTreeMap<String, Ty> = seed.clone();
        for (name, ty) in &narrowed {
            if vars.contains_key(name) {
                vars.insert(name.clone(), ty.clone());
            }
        }
        let mut filters: Vec<Assert> = Vec::new();

        // One contribution, composed into `source` — and mirrored into `vars`
        // unless an assertion has already said what the variable is.
        let constrain = |source: &mut BTreeMap<String, Ty>,
                         vars: &mut BTreeMap<String, Ty>,
                         name: &str,
                         ty: Ty,
                         span: Span,
                         fault: &mut Option<Diagnostic>| {
            let merged = match source.get(name) {
                Some(existing) => match compose(existing, &ty) {
                    Ok(t) => t,
                    Err(c) => {
                        note(
                            Diagnostic::error(
                                Pass::Infer,
                                span,
                                format!(
                                    "variable `{name}` is used as `{}` here and as `{}` \
                                     elsewhere",
                                    c.right, c.left
                                ),
                            ),
                            fault,
                        );
                        Ty::Error
                    }
                },
                None => ty,
            };
            source.insert(name.to_string(), merged.clone());
            vars.insert(
                name.to_string(),
                narrowed.get(name).cloned().unwrap_or(merged),
            );
        };

        for stmt in &rule.body {
            match stmt {
                core::Stmt::Atom {
                    relation,
                    args,
                    span,
                    ..
                } => {
                    for (column, arg) in args {
                        // A wildcard says "there is a column here I do not care
                        // about", so it constrains nothing and binds nothing.
                        let core::Arg::Expr(e) = arg else { continue };
                        let want = self.column_ty(relation, column).cloned();
                        match e {
                            core::Expr::Var { name, span } => {
                                constrain(
                                    &mut source,
                                    &mut vars,
                                    name,
                                    want.unwrap_or(Ty::Unknown),
                                    *span,
                                    &mut fault,
                                );
                            }
                            other => {
                                // "A literal in an atom's argument takes the
                                // column's type" — a constraint on the column,
                                // binding nothing.
                                let (mut got, err) = self.expr_ty(other, &vars);
                                if let Some(d) = err {
                                    note(d, &mut fault);
                                }
                                if let Some(w) = &want
                                    && (impose(&mut got, w).is_err() || !assignable(&got, w))
                                {
                                    note(
                                        Diagnostic::error(
                                            Pass::Infer,
                                            other.span(),
                                            format!(
                                                "`{got}` is not assignable to column \
                                                     `{column}` of `{relation}`, which is \
                                                     `{w}`"
                                            ),
                                        ),
                                        &mut fault,
                                    );
                                }
                            }
                        }
                    }
                    let _ = span;
                }

                core::Stmt::Match { lhs, rhs, span } => match lhs {
                    core::Pattern::Var { name, span: vs } => {
                        let (ty, err) = self.rhs_ty(rhs, &vars);
                        if let Some(d) = err {
                            note(d, &mut fault);
                        }
                        constrain(&mut source, &mut vars, name, ty, *vs, &mut fault);
                    }
                    // `inference.md`: "`arr` is `array(E)`; `x` and `y` are
                    // `E`." Definite, as a dict pattern's are: `array:get`
                    // yields `optional(E)` and the pattern's promise is that
                    // the position is filled.
                    core::Pattern::Array {
                        elems,
                        rest,
                        span: ps,
                    } => {
                        let (subject, err) = self.rhs_ty(rhs, &vars);
                        if let Some(d) = err {
                            note(d, &mut fault);
                        }
                        let element = match &subject {
                            Ty::Array(e) => (**e).clone(),
                            Ty::Unknown => Ty::Unknown,
                            other => {
                                note(
                                    Diagnostic::error(
                                        Pass::Infer,
                                        *ps,
                                        format!("`{other}` is not an array to destructure"),
                                    ),
                                    &mut fault,
                                );
                                Ty::Error
                            }
                        };
                        for var in elems {
                            constrain(
                                &mut source,
                                &mut vars,
                                var,
                                element.clone(),
                                *ps,
                                &mut fault,
                            );
                        }
                        // The remainder is an array of the same elements — the
                        // one destructure whose rest needs no type of its own.
                        if let core::Rest::Bind(r) = rest {
                            constrain(
                                &mut source,
                                &mut vars,
                                r,
                                Ty::Array(Box::new(element)),
                                *ps,
                                &mut fault,
                            );
                        }
                    }

                    // `inference.md`: "`d` is `dict(string, V)`; `x` is `V`."
                    // Definite, not `optional(V)` — the pattern's promise is
                    // that the key is there, and `plan` is where the row
                    // without it is dropped.
                    core::Pattern::Dict {
                        fields,
                        rest,
                        span: ps,
                    } => {
                        let (subject, err) = self.rhs_ty(rhs, &vars);
                        if let Some(d) = err {
                            note(d, &mut fault);
                        }
                        let value = match &subject {
                            Ty::Dict(k, v) if **k == Ty::String => (**v).clone(),
                            // A pattern names its key literally, and a literal
                            // key is a string. Nothing else can be written.
                            Ty::Dict(k, _) => {
                                note(
                                    Diagnostic::error(
                                        Pass::Infer,
                                        *ps,
                                        format!(
                                            "a dict pattern names a string key, but this \
                                             dict is keyed by `{k}`"
                                        ),
                                    ),
                                    &mut fault,
                                );
                                Ty::Error
                            }
                            Ty::Unknown => Ty::Unknown,
                            other => {
                                note(
                                    Diagnostic::error(
                                        Pass::Infer,
                                        *ps,
                                        format!("`{other}` is not a dict to destructure"),
                                    ),
                                    &mut fault,
                                );
                                Ty::Error
                            }
                        };
                        for (_, var) in fields {
                            constrain(&mut source, &mut vars, var, value.clone(), *ps, &mut fault);
                        }
                        // The remainder is the same dict, minus some entries —
                        // the one destructure whose rest needs no type of its
                        // own beyond the subject's.
                        if let core::Rest::Bind(r) = rest {
                            constrain(&mut source, &mut vars, r, subject.clone(), *ps, &mut fault);
                        }
                    }

                    // `inference.md`: "`s` is a record with field `a`; `x` is
                    // its type." A record's fields are its type, so everything
                    // here is a compile-time claim and no row is ever dropped
                    // for failing one.
                    core::Pattern::Record {
                        fields,
                        rest,
                        span: ps,
                    } => {
                        let (subject, err) = self.rhs_ty(rhs, &vars);
                        if let Some(d) = err {
                            note(d, &mut fault);
                        }
                        match &subject {
                            Ty::Record(have) => {
                                for (key, var) in fields {
                                    match have.iter().find(|(n, _)| n == key) {
                                        Some((_, t)) => {
                                            constrain(
                                                &mut source,
                                                &mut vars,
                                                var,
                                                t.clone(),
                                                *ps,
                                                &mut fault,
                                            );
                                        }
                                        None => note(
                                            Diagnostic::error(
                                                Pass::Infer,
                                                *ps,
                                                format!("`{subject}` has no field `{key}`"),
                                            ),
                                            &mut fault,
                                        ),
                                    }
                                }
                                // After the fields, not before: a pattern
                                // naming a field the record lacks is also a
                                // count that does not match, and "no field `z`"
                                // is the more useful of the two.
                                if matches!(rest, core::Rest::None) && have.len() != fields.len() {
                                    note(
                                        Diagnostic::error(
                                            Pass::Infer,
                                            *ps,
                                            format!(
                                                "`{subject}` has fields this pattern does \
                                                 not name; write `**` to allow them"
                                            ),
                                        ),
                                        &mut fault,
                                    );
                                }
                                // The remainder is known here and nowhere else,
                                // which is what makes it a record literal in
                                // `plan` rather than a builtin grasp-dbsp lacks.
                                if let core::Rest::Bind(r) = rest {
                                    let left: Vec<(String, Ty)> = have
                                        .iter()
                                        .filter(|(n, _)| !fields.iter().any(|(k, _)| k == n))
                                        .cloned()
                                        .collect();
                                    constrain(
                                        &mut source,
                                        &mut vars,
                                        r,
                                        Ty::Record(left),
                                        *ps,
                                        &mut fault,
                                    );
                                }
                            }
                            Ty::Unknown => {}
                            other => note(
                                Diagnostic::error(
                                    Pass::Infer,
                                    *ps,
                                    format!("`{other}` is not a record to destructure"),
                                ),
                                &mut fault,
                            ),
                        }
                    }

                    core::Pattern::Unnest {
                        vars: names,
                        kind,
                        span: ps,
                    } => {
                        let (subject, err) = self.rhs_ty(rhs, &vars);
                        if let Some(d) = err {
                            note(d, &mut fault);
                        }
                        match kind {
                            core::UnnestKind::Array => {
                                let elem = match &subject {
                                    Ty::Array(e) => (**e).clone(),
                                    Ty::Unknown => Ty::Unknown,
                                    other => {
                                        note(
                                            Diagnostic::error(
                                                Pass::Infer,
                                                *span,
                                                format!("`{other}` is not an array to unnest"),
                                            ),
                                            &mut fault,
                                        );
                                        Ty::Error
                                    }
                                };
                                // `(v)` binds the element; `(i, v)` binds its
                                // index first, which is an `i64` whatever the
                                // array holds.
                                match names.as_slice() {
                                    [v] => {
                                        constrain(&mut source, &mut vars, v, elem, *ps, &mut fault)
                                    }
                                    [i, v] => {
                                        constrain(
                                            &mut source,
                                            &mut vars,
                                            i,
                                            Ty::I64,
                                            *ps,
                                            &mut fault,
                                        );
                                        constrain(&mut source, &mut vars, v, elem, *ps, &mut fault);
                                    }
                                    // `check_unnest` admits no other arity, and
                                    // reports before this pass runs.
                                    _ => {}
                                }
                            }
                            core::UnnestKind::Dict => {
                                let (k, v) = match &subject {
                                    Ty::Dict(k, v) => ((**k).clone(), (**v).clone()),
                                    Ty::Unknown => (Ty::Unknown, Ty::Unknown),
                                    other => {
                                        note(
                                            Diagnostic::error(
                                                Pass::Infer,
                                                *span,
                                                format!("`{other}` is not a dict to unnest"),
                                            ),
                                            &mut fault,
                                        );
                                        (Ty::Error, Ty::Error)
                                    }
                                };
                                if let Some(n) = names.first() {
                                    constrain(&mut source, &mut vars, n, k, *ps, &mut fault);
                                }
                                if let Some(n) = names.get(1) {
                                    constrain(&mut source, &mut vars, n, v, *ps, &mut fault);
                                }
                            }
                        }
                    }
                },

                core::Stmt::Filter { expr, span } => {
                    let (ty, err) = self.expr_ty(expr, &vars);
                    if let Some(d) = err {
                        note(d, &mut fault);
                    } else if !matches!(ty, Ty::Boolean | Ty::Error | Ty::Unknown) {
                        note(
                            Diagnostic::error(
                                Pass::Infer,
                                *span,
                                format!("a filter must be `boolean`, but this one is `{ty}`"),
                            ),
                            &mut fault,
                        );
                    }
                }

                // Read before the walk, into `narrowed`. An assertion is a
                // claim about a variable rather than a contribution to it, so
                // it contributes nothing here.
                core::Stmt::Assert { .. } => {}

                core::Stmt::Input { .. } => {}
            }
        }

        // What each assertion turned out to *be*, now that the statements have
        // had their say: a compile-time check, a filter, or a mistake. What it
        // narrowed to is settled already — `vars` has carried it throughout.
        //
        // Reported ahead of anything the walk found. An assertion that can
        // never pass explains every type error under it, and reporting `no
        // version of + takes string` first would send a reader after the wrong
        // line.
        let mut wrong: Option<Diagnostic> = None;
        for stmt in &rule.body {
            let core::Stmt::Assert { variable, ty, span } = stmt else {
                continue;
            };
            let want = Ty::known(ty);
            let Some(have) = source.get(variable) else {
                // Unbound: the safety check reports it, and saying so twice
                // would be two diagnostics for one fault.
                continue;
            };
            match narrows(have, &want) {
                Narrowing::Already => {}
                Narrowing::Filter { cast, shape } => filters.push(Assert {
                    variable: variable.clone(),
                    ty: ty.clone(),
                    cast,
                    shape,
                    span: *span,
                }),
                Narrowing::Unsupported => note(
                    Diagnostic::unimplemented(Pass::Infer, *span, "narrowing under two wrappers"),
                    &mut wrong,
                ),
                Narrowing::Never => note(
                    Diagnostic::error(
                        Pass::Infer,
                        *span,
                        format!(
                            "no `{have}` value is a `{ty}`: this assertion would discard \
                             every row"
                        ),
                    ),
                    &mut wrong,
                ),
            }
        }
        if wrong.is_some() {
            fault = wrong;
        }

        // Phase 3, upwards: a declared head column reaches back into the
        // variable that fills it. This is what settles `n := 6 * 7` under
        // `answer :: relation(v: i64)` — the body alone leaves `n` an integer
        // literal with nothing beside it, and the head is the only thing that
        // says which numeric type it is.
        for (column, expr) in &rule.head.args {
            if let core::Expr::Var { name, .. } = expr
                && let Some(want) = self.column_ty(&rule.head.relation, column).cloned()
                && let Some(ty) = vars.get_mut(name)
                && impose(ty, &want).is_err()
            {
                note(
                    Diagnostic::error(
                        Pass::Infer,
                        expr.span(),
                        format!(
                            "`{name}` is `{ty}` here, but column `{column}` of `{}` is \
                             `{want}`",
                            rule.head.relation
                        ),
                    ),
                    &mut fault,
                );
            }
        }

        // The head, which is what the relation learns.
        let mut head = BTreeMap::new();
        for (column, expr) in &rule.head.args {
            let (mut ty, err) = self.expr_ty(expr, &vars);
            if let Some(d) = err {
                note(d, &mut fault);
            }
            // A declared column reaches back into the expression that fills it,
            // which is what lets `answer :: relation(v: i64)` settle `6 * 7`.
            if let Some(want) = self.column_ty(&rule.head.relation, column) {
                let _ = impose(&mut ty, want);
            }
            head.insert(column.clone(), ty);
        }

        // What each partial call settles to, walked once with the finished
        // table rather than collected during it: a type read mid-walk may be
        // the one a later statement corrects.
        let mut partials = BTreeMap::new();
        for e in expressions(rule) {
            self.collect_partials(e, &vars, &mut partials);
        }

        RuleTypes {
            vars,
            source,
            head,
            fault,
            filters,
            partials,
        }
    }

    fn rhs_ty(&self, rhs: &core::Rhs, vars: &BTreeMap<String, Ty>) -> (Ty, Option<Diagnostic>) {
        match rhs {
            core::Rhs::Expr(e) => self.expr_ty(e, vars),
            core::Rhs::Aggregate {
                function,
                arg,
                span,
            } => {
                let name = key::aggregator(*function);
                let counting = *function == Aggregator::Count;
                // "`count<>` takes no argument"; every other aggregator needs
                // the expression it folds. Neither was checked, so `sum<>`
                // quietly aggregated a literal and `count<e>` quietly meant
                // grasp-dbsp's count — the values that are present, not the
                // assignments in the group.
                match (counting, arg) {
                    (true, Some(e)) => {
                        return (
                            Ty::I64,
                            Some(Diagnostic::error(
                                Pass::Infer,
                                e.span(),
                                "`count` takes no argument: it counts the assignments in \
                                 the group, not the values of an expression",
                            )),
                        );
                    }
                    (false, None) => {
                        return (
                            Ty::Error,
                            Some(Diagnostic::error(
                                Pass::Infer,
                                *span,
                                format!("`{name}` needs an argument: the expression to fold"),
                            )),
                        );
                    }
                    (true, None) => return (Ty::I64, None),
                    (false, Some(_)) => {}
                }
                let e = arg.as_ref().expect("checked above");
                let (inner, err) = self.expr_ty(e, vars);
                if err.is_some() {
                    return (Ty::Error, err);
                }
                // What each aggregator can fold. An `optional` is looked
                // through: absence is skipped, and the result keeps the
                // wrapper.
                let want = under_optional(&inner);
                let wrong = match function {
                    Aggregator::Sum | Aggregator::Avg if !numeric(want) => Some(format!(
                        "`{name}` needs a numeric argument, found `{inner}`"
                    )),
                    // The same rule `<` enforces, and for the same reason:
                    // "any invention would be arbitrary in a way that silently
                    // decides `min` and `max`".
                    Aggregator::Min | Aggregator::Max if !scalar(want) => Some(format!(
                        "ordering is not defined on `{inner}`, so `{name}` has no meaning \
                         over it"
                    )),
                    _ => None,
                };
                if let Some(message) = wrong {
                    return (
                        Ty::Error,
                        Some(Diagnostic::error(Pass::Infer, e.span(), message)),
                    );
                }
                // "Every aggregator but `count` gives back its argument's
                //  type", `avg` included — the mean of `i64`s is an `i64`.
                let ty = match function {
                    Aggregator::Count => Ty::I64,
                    _ => inner,
                };
                (ty, None)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

impl Cx {
    /// The type of an expression, and the first thing wrong inside it.
    ///
    /// Returns a type in every case — [`Ty::Error`] where it could not work one
    /// out — so a rule with one mistake still types the rest of itself and
    /// produces one diagnostic rather than a cascade.
    fn expr_ty(&self, e: &core::Expr, vars: &BTreeMap<String, Ty>) -> (Ty, Option<Diagnostic>) {
        match e {
            core::Expr::Lit { value, .. } => (
                match value {
                    // An integer literal is not an `i64` until something says
                    // so. A float has one numeric type it can inhabit, and a
                    // string or a boolean spells its own.
                    Lit::Int(_) => Ty::Int,
                    Lit::Float(_) => Ty::F64,
                    Lit::Str(_) => Ty::String,
                    Lit::Bool(_) => Ty::Boolean,
                    // "`NONE` has no type of its own; it takes `optional(T)`
                    //  from wherever it sits."
                    Lit::None => Ty::Optional(Box::new(Ty::Unknown)),
                },
                None,
            ),

            // An unbound variable is the safety check's business, not this
            // one's — reporting it here as well would double the diagnostic.
            core::Expr::Var { name, .. } => (vars.get(name).cloned().unwrap_or(Ty::Unknown), None),

            core::Expr::Unary { op, operand, span } => {
                let (ty, err) = self.expr_ty(operand, vars);
                match op {
                    UnOp::Neg => {
                        if numeric(&ty) {
                            (ty, err)
                        } else {
                            (
                                Ty::Error,
                                err.or_else(|| {
                                    Some(Diagnostic::error(
                                        Pass::Infer,
                                        *span,
                                        format!("no version of `-` takes `{ty}`"),
                                    ))
                                }),
                            )
                        }
                    }
                    // Desugared into `boolean:not`, so this is unreachable —
                    // written as a diagnostic rather than a panic, because a
                    // reachable internal error is worse than a redundant arm.
                    UnOp::Not => (
                        Ty::Error,
                        err.or_else(|| {
                            Some(Diagnostic::error(
                                Pass::Infer,
                                *span,
                                "`not` should have been desugared to `boolean:not`",
                            ))
                        }),
                    ),
                }
            }

            core::Expr::Binary { op, lhs, rhs, span } => {
                let (l, e1) = self.expr_ty(lhs, vars);
                let (r, e2) = self.expr_ty(rhs, vars);
                let err = e1.or(e2);
                let both = compose(&l, &r).ok();
                let fail = |what: &str| {
                    (
                        Ty::Error,
                        Some(Diagnostic::error(
                            Pass::Infer,
                            *span,
                            format!("no version of `{what}` takes `{l}` and `{r}`"),
                        )),
                    )
                };
                if err.is_some() {
                    return (Ty::Error, err);
                }
                match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => match both {
                        Some(t) if numeric(&t) => (t, None),
                        _ => fail(op.as_str()),
                    },
                    // Equality is structural and defined on every type.
                    BinOp::Eq | BinOp::Ne => match both {
                        Some(_) => (Ty::Boolean, None),
                        None => fail(op.as_str()),
                    },
                    // "Ordering is defined on scalars only."
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => match both {
                        Some(t) if scalar(&t) => (Ty::Boolean, None),
                        Some(t) => (
                            Ty::Error,
                            Some(Diagnostic::error(
                                Pass::Infer,
                                *span,
                                format!("ordering is not defined on `{t}`; compare its parts"),
                            )),
                        ),
                        None => fail(op.as_str()),
                    },
                    BinOp::And | BinOp::Or => match both {
                        Some(Ty::Boolean) | Some(Ty::Unknown) => (Ty::Boolean, None),
                        _ => fail(op.as_str()),
                    },
                    // Desugared into `concat`, so unreachable; see `Not` above.
                    BinOp::Concat => (
                        Ty::Error,
                        Some(Diagnostic::error(
                            Pass::Infer,
                            *span,
                            "`++` should have been desugared to `concat`",
                        )),
                    ),
                }
            }

            core::Expr::Call { callee, args, span } => {
                let mut tys = Vec::with_capacity(args.len());
                let mut err = None;
                for a in args {
                    let (t, e) = self.expr_ty(a, vars);
                    err = err.or(e);
                    tys.push(t);
                }
                if err.is_some() {
                    return (Ty::Error, err);
                }
                self.apply(*callee, &tys, args, *span)
            }

            core::Expr::ArrayLit { elems, span } => {
                let mut elem = Ty::Unknown;
                for e in elems {
                    let (t, err) = self.expr_ty(e, vars);
                    if err.is_some() {
                        return (Ty::Error, err);
                    }
                    match compose(&elem, &t) {
                        Ok(c) => elem = c,
                        Err(c) => {
                            return (
                                Ty::Error,
                                Some(Diagnostic::error(
                                    Pass::Infer,
                                    *span,
                                    format!(
                                        "an array has one element type, but this one has \
                                         `{}` and `{}`",
                                        c.left, c.right
                                    ),
                                )),
                            );
                        }
                    }
                }
                (Ty::Array(Box::new(elem)), None)
            }

            core::Expr::DictLit { entries, span } => {
                let (mut k, mut v) = (Ty::Unknown, Ty::Unknown);
                for (key, value) in entries {
                    let (kt, e1) = self.expr_ty(key, vars);
                    let (vt, e2) = self.expr_ty(value, vars);
                    if let Some(e) = e1.or(e2) {
                        return (Ty::Error, Some(e));
                    }
                    match (compose(&k, &kt), compose(&v, &vt)) {
                        (Ok(a), Ok(b)) => {
                            k = a;
                            v = b;
                        }
                        _ => {
                            return (
                                Ty::Error,
                                Some(Diagnostic::error(
                                    Pass::Infer,
                                    *span,
                                    "a dict has one key type and one value type",
                                )),
                            );
                        }
                    }
                }
                if !matches!(k, Ty::Unknown) && !scalar(&k) {
                    return (
                        Ty::Error,
                        Some(Diagnostic::error(
                            Pass::Infer,
                            *span,
                            format!("a dict key must be a scalar, but this one is `{k}`"),
                        )),
                    );
                }
                (Ty::Dict(Box::new(k), Box::new(v)), None)
            }

            core::Expr::RecordLit { fields, .. } => {
                let mut out = Vec::with_capacity(fields.len());
                let mut err = None;
                for (name, e) in fields {
                    let (t, e2) = self.expr_ty(e, vars);
                    err = err.or(e2);
                    out.push((name.clone(), t));
                }
                if err.is_some() {
                    return (Ty::Error, err);
                }
                out.sort_by(|a, b| a.0.cmp(&b.0));
                (Ty::Record(out), None)
            }
        }
    }

    /// `semantics.md`'s builtin table, applied.
    fn apply(
        &self,
        callee: core::Builtin,
        args: &[Ty],
        exprs: &[core::Expr],
        span: Span,
    ) -> (Ty, Option<Diagnostic>) {
        use core::Builtin as B;
        let name = callee.as_str();
        let arity = |n: usize| -> Option<Diagnostic> {
            (args.len() != n).then(|| {
                Diagnostic::error(
                    Pass::Infer,
                    span,
                    format!(
                        "`{name}` takes {n} argument(s), but {} were given",
                        args.len()
                    ),
                )
            })
        };
        let wrong = || {
            let shown: Vec<String> = args.iter().map(|t| format!("`{t}`")).collect();
            (
                Ty::Error,
                Some(Diagnostic::error(
                    Pass::Infer,
                    span,
                    format!("no version of `{name}` takes {}", shown.join(" and ")),
                )),
            )
        };
        let open = |t: &Ty| matches!(t, Ty::Unknown);
        // One argument of one type, one result. Most of a namespaced library
        // looks like this, which is the point of namespacing it.
        let one = |args: &[Ty],
                   arity: &dyn Fn(usize) -> Option<Diagnostic>,
                   want: Ty,
                   out: Ty,
                   wrong: &dyn Fn() -> (Ty, Option<Diagnostic>)| {
            if let Some(d) = arity(1) {
                return (Ty::Error, Some(d));
            }
            if args[0] == want || open(&args[0]) {
                (out, None)
            } else {
                wrong()
            }
        };

        match callee {
            // Namespaced, so each of these is one type and one signature —
            // `string:length` and `array:length` are two functions rather than
            // one asked to guess. What the arms below check is the argument
            // *is* what the name already said it would be.
            B::IntegerAbs => one(args, &arity, Ty::I64, Ty::I64, &wrong),
            B::FloatAbs | B::FloatFloor | B::FloatCeil | B::FloatRound => {
                one(args, &arity, Ty::F64, Ty::F64, &wrong)
            }
            B::StringLower | B::StringUpper | B::StringTrim => {
                one(args, &arity, Ty::String, Ty::String, &wrong)
            }
            B::StringLength => one(args, &arity, Ty::String, Ty::I64, &wrong),
            B::BooleanNot => one(args, &arity, Ty::Boolean, Ty::Boolean, &wrong),
            B::StringConcat => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                if args.iter().all(|t| matches!(t, Ty::String | Ty::Unknown)) {
                    (Ty::String, None)
                } else {
                    wrong()
                }
            }
            B::ArrayLength => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Array(_) | Ty::Unknown => (Ty::I64, None),
                    _ => wrong(),
                }
            }
            B::DictLength => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Dict(..) | Ty::Unknown => (Ty::I64, None),
                    _ => wrong(),
                }
            }
            B::DictKeys => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Dict(k, _) => (Ty::Array(k.clone()), None),
                    Ty::Unknown => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            B::DictEntries => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Dict(k, v) => (
                        Ty::Array(Box::new(Ty::Record(vec![
                            ("key".to_string(), (**k).clone()),
                            ("value".to_string(), (**v).clone()),
                        ]))),
                        None,
                    ),
                    Ty::Unknown => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            // `d[k]`. Unlike `record:get`, the key is a value rather than part
            // of the subject's type — so it has one, and it has to be the
            // dict's. Without this check `d[1]` on a `dict(string, …)` reaches
            // grasp-dbsp, which reports it against text the program never
            // wrote.
            // `array:get(a, i)` — an element, `optional(E)` because the index
            // may be past the end. The index is an `i64`; a pattern only ever
            // writes a literal one, so this is a check on the emitter as much
            // as on a program.
            B::ArrayAt => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Array(e) if matches!(args[1], Ty::I64 | Ty::Int) => ((**e).clone(), None),
                    Ty::Unknown => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            // `array:drop(a, n)` — the same array, shorter.
            B::ArrayDrop => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Array(_) if matches!(args[1], Ty::I64 | Ty::Int) => (args[0].clone(), None),
                    Ty::Unknown => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            // `dict:without_keys(d, ks)` — the same dict, smaller.
            B::DictWithoutKeys => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                match (&args[0], &args[1]) {
                    (Ty::Dict(k, _), Ty::Array(e)) if k == e => (args[0].clone(), None),
                    // An empty `[]` has no element type to compare, and a
                    // pattern that names no key produces exactly that.
                    (Ty::Dict(..), Ty::Array(e)) if **e == Ty::Unknown => (args[0].clone(), None),
                    (Ty::Unknown, _) => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            B::DictGet => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Dict(k, v) => {
                        if !open(&args[1]) && !assignable(&args[1], k) {
                            return (
                                Ty::Error,
                                Some(Diagnostic::error(
                                    Pass::Infer,
                                    span,
                                    format!(
                                        "this dict is keyed by `{k}`, but the key is \
                                         `{}`",
                                        args[1]
                                    ),
                                )),
                            );
                        }
                        ((**v).clone(), None)
                    }
                    Ty::Unknown => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            // Not a signature list: its result depends on *which* field, so it
            // reads the key literal rather than a type.
            B::RecordGet => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                let field = match exprs.get(1) {
                    Some(core::Expr::Lit {
                        value: Lit::Str(f), ..
                    }) => f.clone(),
                    _ => return wrong(),
                };
                match &args[0] {
                    Ty::Record(fields) => match fields.iter().find(|(n, _)| *n == field) {
                        Some((_, t)) => (t.clone(), None),
                        None => (
                            Ty::Error,
                            Some(Diagnostic::error(
                                Pass::Infer,
                                span,
                                format!("`{}` has no field `{field}`", args[0]),
                            )),
                        ),
                    },
                    Ty::Unknown => (Ty::Unknown, None),
                    // `inference.md` writes this as "`e` is not a record",
                    // quoting source the compiler does not have. The span points
                    // at it; the message says what it found instead.
                    other => (
                        Ty::Error,
                        Some(Diagnostic::error(
                            Pass::Infer,
                            span,
                            format!("`{other}` is not a record, so it has no field `{field}`"),
                        )),
                    ),
                }
            }
        }
    }
}

/// What is under an `optional`, which is what an aggregator folds: absence is
/// skipped rather than aggregated, and the wrapper survives into the result.
fn under_optional(t: &Ty) -> &Ty {
    match t {
        Ty::Optional(inner) => inner,
        other => other,
    }
}

fn numeric(t: &Ty) -> bool {
    matches!(t, Ty::I64 | Ty::F64 | Ty::Int | Ty::Unknown)
}

fn scalar(t: &Ty) -> bool {
    matches!(
        t,
        Ty::Boolean | Ty::I64 | Ty::F64 | Ty::String | Ty::Int | Ty::Unknown
    )
}

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

impl Cx {
    /// Every diagnostic this pass produces, in one walk over the settled table.
    fn check(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();

        // Relation-level, once each.
        for (name, span) in &self.inputs {
            if let Some(d) = self.check_input(name, *span) {
                out.push(d);
            }
        }
        if !out.is_empty() {
            return out;
        }

        // A relation mentioned but never declared. If any, stop: every other
        // untyped relation in the program is downstream of one of these, and
        // reporting those too would be a second diagnostic the fixtures forbid.
        let declared = self.declared();
        for rule in &self.rules {
            for stmt in &rule.body {
                if let core::Stmt::Atom { relation, span, .. } = stmt
                    && !declared.contains(relation.as_str())
                    && !out
                        .iter()
                        .any(|d: &Diagnostic| d.message.contains(relation))
                {
                    out.push(Diagnostic::error(
                        Pass::Infer,
                        *span,
                        format!("relation `{relation}` is not defined and has no typespec"),
                    ));
                }
            }
        }
        if !out.is_empty() {
            return out;
        }

        // Declared, but nothing ever gave it a type — which is the fixpoint's
        // gate never admitting its rules. A relation defined only by a
        // recursive rule lands here, and that is the diagnostic.
        for rule in &self.rules {
            let name = &rule.head.relation;
            if !self.known.contains_key(name)
                && !out.iter().any(|d: &Diagnostic| d.message.contains(name))
            {
                out.push(Diagnostic::error(
                    Pass::Infer,
                    rule.head.span,
                    format!(
                        "relation `{name}` has no non-recursive rule and no typespec, \
                         so nothing gives it a type"
                    ),
                ));
            }
        }
        if !out.is_empty() {
            return out;
        }

        // Per declaration, at most one.
        for fact in &self.facts {
            if let Some(d) = self.check_fact(fact) {
                out.push(d);
            }
        }
        for rule in &self.rules {
            if let Some(d) = self.check_rule(rule) {
                out.push(d);
            }
        }
        out
    }

    /// `r(cols:) <- input`, and the two things that stops it being.
    fn check_input(&self, relation: &str, span: Span) -> Option<Diagnostic> {
        let columns = self.specs.get(relation).map(|(c, _)| c.len());
        match columns {
            None => {
                return Some(Diagnostic::error(
                    Pass::Infer,
                    span,
                    format!("relation `{relation}` is an input and needs a `::` typespec"),
                ));
            }
            // A relation with no columns is a proposition, and pushing rows
            // into one could only count them.
            Some(0) => {
                return Some(Diagnostic::error(
                    Pass::Infer,
                    span,
                    format!("relation `{relation}` has no columns and cannot be an input"),
                ));
            }
            _ => {}
        }
        // "A relation is defined by the program or comes from outside, not
        //  both."
        if let Some(f) = self.facts.iter().find(|f| f.relation == relation) {
            return Some(Diagnostic::error(
                Pass::Infer,
                f.span,
                format!("relation `{relation}` has both an input rule and facts"),
            ));
        }
        if let Some(r) = self.rules.iter().find(|r| r.head.relation == relation) {
            return Some(Diagnostic::error(
                Pass::Infer,
                r.span,
                format!("relation `{relation}` is both an input and derived by a rule"),
            ));
        }
        None
    }

    /// A fact is a rule with an empty body, and is checked as one.
    fn check_fact(&self, fact: &core::Fact) -> Option<Diagnostic> {
        let empty = BTreeMap::new();
        for (column, expr) in &fact.args {
            // Safety first: a variable here is one the head uses and no body
            // binds, because there is no body.
            if let Some(d) = unbound_in(expr, &empty) {
                return Some(d);
            }
            let (mut ty, err) = self.expr_ty(expr, &empty);
            if err.is_some() {
                return err;
            }
            if let Some(want) = self.column_ty(&fact.relation, column) {
                let _ = impose(&mut ty, want);
            }
            if let Err(open) = settle(&ty) {
                return open_diagnostic(open, expr.span());
            }
        }
        self.check_head_columns(&fact.relation, fact.args.iter().map(|(c, _)| c), fact.span)
    }

    fn check_rule(&self, rule: &core::Rule) -> Option<Diagnostic> {
        // Shape first: a pattern with no settled meaning makes every check
        // below it read something that is not there.
        if let Some(d) = self.check_unnest(rule) {
            return Some(d);
        }
        // Safety, before typing: an unbound variable makes everything after it
        // unknowable, and reporting both would be two diagnostics for one
        // mistake.
        if let Some(d) = self.check_safety(rule) {
            return Some(d);
        }
        // Scope, also before typing, and for the same reason: it needs no types,
        // so nothing it says can be poisoned by an earlier fault. It is
        // structural — reads and binds, no order — so it does not care that
        // `type_rule` needs several rounds to reach the same conclusion.
        if let Some(d) = check_aggregate_scope(rule) {
            return Some(d);
        }

        let typed = self.type_rule(rule);
        if typed.fault.is_some() {
            return typed.fault;
        }

        // Phase 3: every variable and every head column must have settled.
        for (name, ty) in &typed.vars {
            if let Err(open) = settle(ty) {
                let span = binding_span(rule, name).unwrap_or(rule.span);
                return open_diagnostic(open, span);
            }
        }
        for (column, expr) in &rule.head.args {
            let ty = typed.head.get(column).cloned().unwrap_or(Ty::Unknown);
            if let Err(open) = settle(&ty) {
                return open_diagnostic(open, expr.span());
            }
            // A spec is the answer; a rule that disagrees is the error.
            if let Some(want) = self.column_ty(&rule.head.relation, column)
                && self.specs.contains_key(&rule.head.relation)
                && !assignable(&ty, want)
            {
                return Some(Diagnostic::error(
                    Pass::Infer,
                    expr.span(),
                    format!(
                        "this rule gives `{}.{column}` type `{ty}`, but it is declared `{want}`",
                        rule.head.relation
                    ),
                ));
            }
        }

        self.check_head_columns(
            &rule.head.relation,
            rule.head.args.iter().map(|(c, _)| c),
            rule.head.span,
        )
    }

    /// "A rule head must name **every** column of a declared relation."
    fn check_head_columns<'a>(
        &self,
        relation: &str,
        named: impl Iterator<Item = &'a String>,
        span: Span,
    ) -> Option<Diagnostic> {
        let (columns, _) = self.specs.get(relation)?;
        let named: BTreeSet<&str> = named.map(String::as_str).collect();
        for (column, _) in columns {
            if !named.contains(column.as_str()) {
                return Some(Diagnostic::error(
                    Pass::Infer,
                    span,
                    format!("this head is missing column `{column}` of relation `{relation}`"),
                ));
            }
        }
        // Not in `inference.md`'s table, and needed: without it a typo invents
        // a column on an unspecced relation and nothing notices.
        for column in named {
            if !columns.iter().any(|(c, _)| c == column) {
                return Some(Diagnostic::error(
                    Pass::Infer,
                    span,
                    format!("relation `{relation}` has no column `{column}`"),
                ));
            }
        }
        None
    }

    /// An unnest binds a value, an index and a value, or a key and a value.
    ///
    /// `syntax.md`'s `unnest_pattern` admits any number of variables, and the
    /// three meanings are the ones the language has: the marker says which
    /// container is being taken apart and the arity says what is wanted from
    /// it. Nothing downstream is written for another arity — `emit` matches the
    /// three shapes and has no fourth — so this is what keeps that match total.
    ///
    /// It runs before every other check because a malformed pattern has no
    /// settled meaning, and the checks after it all read what a statement
    /// binds.
    /// Every call to a partial builtin under `e`, and what it settles to.
    ///
    /// Bottom-up, so a lookup inside a lookup is recorded too — the inner one
    /// becomes a node first and the outer reads its variable.
    fn collect_partials(
        &self,
        e: &core::Expr,
        vars: &BTreeMap<String, Ty>,
        out: &mut BTreeMap<Span, Type>,
    ) {
        walk(e, &mut |sub| {
            if let core::Expr::Call { callee, span, .. } = sub
                && callee.partial()
                && let Ok(ty) = settle(&self.expr_ty(sub, vars).0)
            {
                out.insert(*span, ty);
            }
        });
    }

    fn check_unnest(&self, rule: &core::Rule) -> Option<Diagnostic> {
        for stmt in &rule.body {
            let core::Stmt::Match {
                lhs: core::Pattern::Unnest { vars, kind, span },
                ..
            } = stmt
            else {
                continue;
            };
            match (kind, vars.len()) {
                (core::UnnestKind::Array, 1 | 2) | (core::UnnestKind::Dict, 2) => {}
                (core::UnnestKind::Array, _) => {
                    return Some(Diagnostic::error(
                        Pass::Infer,
                        *span,
                        "an array unnest binds a value, or an index and a value",
                    ));
                }
                (core::UnnestKind::Dict, _) => {
                    return Some(Diagnostic::error(
                        Pass::Infer,
                        *span,
                        "a dict unnest binds a key and a value; write `(k, _v)` to \
                         ignore the value",
                    ));
                }
            }
        }
        None
    }

    /// `semantics.md`, "Safety": every variable used must be bound by a
    /// positive atom or a match.
    fn check_safety(&self, rule: &core::Rule) -> Option<Diagnostic> {
        let mut bound: BTreeSet<&str> = BTreeSet::new();
        for stmt in &rule.body {
            match stmt {
                core::Stmt::Atom {
                    args,
                    negated: false,
                    ..
                } => {
                    for (_, arg) in args {
                        if let core::Arg::Expr(core::Expr::Var { name, .. }) = arg {
                            bound.insert(name);
                        }
                    }
                }
                core::Stmt::Match { lhs, .. } => bound.extend(lhs.binds()),
                _ => {}
            }
        }

        // The head first: it is the most useful place to be told.
        for (_, expr) in &rule.head.args {
            if let Some(d) = unbound_in(expr, &bound_map(&bound)) {
                return Some(d);
            }
        }
        for stmt in &rule.body {
            match stmt {
                core::Stmt::Atom {
                    args,
                    negated: true,
                    ..
                } => {
                    for (_, arg) in args {
                        if let core::Arg::Expr(e) = arg
                            && let Some(d) =
                                unbound(e, &bound, "a negated atom, which binds nothing")
                        {
                            return Some(d);
                        }
                    }
                }
                core::Stmt::Filter { expr, .. } => {
                    if let Some(d) = unbound(expr, &bound, "a filter") {
                        return Some(d);
                    }
                }
                // "An assertion binds nothing — the variable must already
                // exist." Unreachable until assertions were implemented, which
                // is why it was not here.
                core::Stmt::Assert { variable, span, .. } => {
                    if !bound.contains(variable.as_str()) {
                        return Some(Diagnostic::error(
                            Pass::Infer,
                            *span,
                            format!(
                                "variable `{variable}` appears in an assertion, which binds \
                                 nothing, but nothing in the body binds it"
                            ),
                        ));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// Everything an aggregate puts out of reach, and what may still read it.
///
/// An aggregate folds a whole group into one value, so past that point the only
/// things with a value are the group and the aggregates' results. A variable
/// that varies *within* the group does not have one — which is why
/// `s > r` is refused rather than answered.
///
/// Note what the diagnostics below never say: *after*. A rule body is a set,
/// not a sequence, and a message that spoke of statements running in an order
/// would teach the opposite of what the language promises. The group is a
/// scope, and that is how they are worded.
fn check_aggregate_scope(rule: &core::Rule) -> Option<Diagnostic> {
    let after = after(rule);
    if after.is_empty() {
        return None;
    }
    let group = group_of(rule, &after);
    let allowed: BTreeSet<String> = group.union(&after).cloned().collect();

    // The name of some aggregate result, for a message that has to point at one.
    let an_aggregate = |reads: &BTreeSet<String>| -> String {
        reads
            .iter()
            .find(|v| after.contains(*v))
            .cloned()
            .unwrap_or_default()
    };

    for stmt in &rule.body {
        let read = reads(stmt);
        match stmt {
            // An aggregate folds the body's assignments, and an aggregate's own
            // result is not one of them. `compilation.md` says so as an
            // invariant — "the aggregated column is in the input schema" — and
            // nothing enforced it.
            core::Stmt::Match {
                rhs: core::Rhs::Aggregate { function, arg, .. },
                ..
            } => {
                if let Some(e) = arg
                    && let Some(v) = pick(&free(e), |v| after.contains(v))
                {
                    return Some(Diagnostic::error(
                        Pass::Infer,
                        var_span(e, &v).unwrap_or(e.span()),
                        format!(
                            "`{}` folds the body's assignments, and `{v}` is an aggregate \
                             result rather than one of them",
                            key::aggregator(*function)
                        ),
                    ));
                }
            }
            // An atom is one of the things the aggregate folds, so it cannot
            // mention what the folding produced — whether it reads the result
            // in an argument expression or binds a column to its name.
            core::Stmt::Atom {
                relation,
                args,
                negated: false,
                span,
                ..
            } => {
                let mentions: BTreeSet<String> = read.union(&binds(stmt)).cloned().collect();
                if let Some(v) = pick(&mentions, |v| after.contains(v)) {
                    let at = args
                        .iter()
                        .find_map(|(_, a)| match a {
                            core::Arg::Expr(e) => var_span(e, &v),
                            core::Arg::Wildcard(_) => None,
                        })
                        .unwrap_or(*span);
                    return Some(Diagnostic::error(
                        Pass::Infer,
                        at,
                        format!(
                            "atom `{relation}` mentions the aggregate result `{v}`, but an \
                             atom is one of the things `{v}` is folded over"
                        ),
                    ));
                }
            }
            _ => {}
        }

        // An aggregate is what *puts* its result out of reach, so it is not a
        // statement that reads nothing and binds something out of reach.
        let aggregating = matches!(
            stmt,
            core::Stmt::Match {
                rhs: core::Rhs::Aggregate { .. },
                ..
            }
        );

        // A variable cannot be both something the group ranges over and the
        // group's answer.
        if read.is_disjoint(&after) {
            for v in binds(stmt) {
                if after.contains(&v) && !aggregating {
                    return Some(Diagnostic::error(
                        Pass::Infer,
                        stmt.span(),
                        format!(
                            "variable `{v}` is bound by the body and again from an aggregate \
                             result, and those are not one value"
                        ),
                    ));
                }
            }
            continue;
        }

        // And the general rule: reading an aggregate's result puts a statement
        // in the group's scope, where only the group has values.
        if let Some(v) = pick(&read, |v| !allowed.contains(v)) {
            let at = statement_var_span(stmt, &v).unwrap_or(stmt.span());
            return Some(Diagnostic::error(
                Pass::Infer,
                at,
                format!(
                    "variable `{v}` is not in the group, so it has no value where the \
                     aggregate `{}` does",
                    an_aggregate(&read)
                ),
            ));
        }
    }

    // The head is one statement for this purpose: a column that reads an
    // aggregate result and a body variable widens the group exactly as a filter
    // would, and nothing in the body would catch it.
    let head_reads: BTreeSet<String> = rule.head.args.iter().flat_map(|(_, e)| free(e)).collect();
    if !head_reads.is_disjoint(&after)
        && let Some(v) = pick(&head_reads, |v| !allowed.contains(v))
    {
        let at = rule
            .head
            .args
            .iter()
            .find_map(|(_, e)| var_span(e, &v))
            .unwrap_or(rule.head.span);
        return Some(Diagnostic::error(
            Pass::Infer,
            at,
            format!(
                "variable `{v}` is not in the group, so it has no value where the aggregate \
                 `{}` does",
                an_aggregate(&head_reads)
            ),
        ));
    }
    None
}

/// The first of `read` that is in `within`, by name so the choice is the
/// program's rather than a set's iteration order.
fn pick(read: &BTreeSet<String>, within: impl Fn(&str) -> bool) -> Option<String> {
    read.iter().find(|v| within(v)).cloned()
}

/// Where a variable is written inside an expression, for a span that points at
/// the mistake rather than at the line.
fn var_span(e: &core::Expr, name: &str) -> Option<Span> {
    let mut found = None;
    walk(e, &mut |x| {
        if let core::Expr::Var { name: n, span } = x
            && n == name
            && found.is_none()
        {
            found = Some(*span);
        }
    });
    found
}

fn statement_var_span(stmt: &core::Stmt, name: &str) -> Option<Span> {
    match stmt {
        core::Stmt::Atom { args, .. } => args.iter().find_map(|(_, a)| match a {
            core::Arg::Expr(e) => var_span(e, name),
            core::Arg::Wildcard(_) => None,
        }),
        core::Stmt::Match { rhs, .. } => match rhs {
            core::Rhs::Expr(e) => var_span(e, name),
            core::Rhs::Aggregate { arg, .. } => arg.as_ref().and_then(|e| var_span(e, name)),
        },
        core::Stmt::Filter { expr, .. } => var_span(expr, name),
        core::Stmt::Assert { span, .. } => Some(*span),
        core::Stmt::Input { .. } => None,
    }
}

/// The variables an expression reads.
fn free(e: &core::Expr) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(e, &mut |x| {
        if let core::Expr::Var { name, .. } = x {
            out.insert(name.clone());
        }
    });
    out
}

/// What one statement reads — every variable it needs a value for.
///
/// A positive atom's plain-variable arguments *bind*; anything else it writes
/// there is a constraint, so it reads. A negated atom binds nothing, so all of
/// it reads.
fn reads(stmt: &core::Stmt) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match stmt {
        core::Stmt::Atom { args, negated, .. } => {
            for (_, arg) in args {
                match arg {
                    core::Arg::Wildcard(_) => {}
                    core::Arg::Expr(core::Expr::Var { .. }) if !negated => {}
                    core::Arg::Expr(e) => out.extend(free(e)),
                }
            }
        }
        core::Stmt::Match { rhs, .. } => match rhs {
            core::Rhs::Expr(e) => out.extend(free(e)),
            core::Rhs::Aggregate { arg, .. } => {
                if let Some(e) = arg {
                    out.extend(free(e));
                }
            }
        },
        core::Stmt::Filter { expr, .. } => out.extend(free(expr)),
        core::Stmt::Assert { variable, .. } => {
            out.insert(variable.clone());
        }
        core::Stmt::Input { .. } => {}
    }
    out
}

/// What one statement binds.
fn binds(stmt: &core::Stmt) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match stmt {
        core::Stmt::Atom {
            args,
            negated: false,
            ..
        } => {
            for (_, arg) in args {
                if let core::Arg::Expr(core::Expr::Var { name, .. }) = arg {
                    out.insert(name.clone());
                }
            }
        }
        core::Stmt::Match { lhs, .. } => {
            out.extend(lhs.binds().into_iter().map(str::to_string));
        }
        _ => {}
    }
    out
}

/// Everything that has no value until the group is formed.
///
/// The aggregates' results, and whatever is computed from them. Closed under
/// reading rather than under statement order, because a rule body is a set:
/// which line came first is not a question the language can answer.
fn after(rule: &core::Rule) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for stmt in &rule.body {
        if let core::Stmt::Match {
            lhs: core::Pattern::Var { name, .. },
            rhs: core::Rhs::Aggregate { .. },
            ..
        } = stmt
        {
            out.insert(name.clone());
        }
    }
    loop {
        let mut grew = false;
        for stmt in &rule.body {
            if reads(stmt).is_disjoint(&out) {
                continue;
            }
            for v in binds(stmt) {
                grew |= out.insert(v);
            }
        }
        if !grew {
            return out;
        }
    }
}

/// The variables the head's non-aggregate columns read — the group.
///
/// "The group is the head's non-aggregate columns", and it is the *variables*
/// they read rather than the columns themselves. The difference is visible:
/// `q(tag: length(d), total: s)` groups by `d`, while binding `t := length(d)`
/// first and writing `q(tag: t, …)` groups by `t`, so two departments whose
/// names are the same length give two rows in the first and one in the second.
fn group_of(rule: &core::Rule, after: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (_, expr) in &rule.head.args {
        let vars = free(expr);
        if vars.is_disjoint(after) {
            out.extend(vars);
        }
    }
    out
}

fn bound_map(bound: &BTreeSet<&str>) -> BTreeMap<String, Ty> {
    bound.iter().map(|n| (n.to_string(), Ty::Unknown)).collect()
}

/// A head or fact argument mentioning a variable nothing binds.
fn unbound_in(e: &core::Expr, bound: &BTreeMap<String, Ty>) -> Option<Diagnostic> {
    let mut found = None;
    walk(e, &mut |x| {
        if let core::Expr::Var { name, span } = x
            && !bound.contains_key(name)
            && found.is_none()
        {
            found = Some(Diagnostic::error(
                Pass::Infer,
                *span,
                format!("variable `{name}` appears in the head but nothing in the body binds it"),
            ));
        }
    });
    found
}

fn unbound(e: &core::Expr, bound: &BTreeSet<&str>, where_: &str) -> Option<Diagnostic> {
    let mut found = None;
    walk(e, &mut |x| {
        if let core::Expr::Var { name, span } = x
            && !bound.contains(name.as_str())
            && found.is_none()
        {
            found = Some(Diagnostic::error(
                Pass::Infer,
                *span,
                format!("variable `{name}` appears in {where_} but nothing in the body binds it"),
            ));
        }
    });
    found
}

/// Every expression a rule holds, head and body alike.
fn expressions(rule: &core::Rule) -> Vec<&core::Expr> {
    let mut out: Vec<&core::Expr> = rule.head.args.iter().map(|(_, e)| e).collect();
    for stmt in &rule.body {
        match stmt {
            core::Stmt::Atom { args, .. } => out.extend(args.iter().filter_map(|(_, a)| match a {
                core::Arg::Expr(e) => Some(e),
                core::Arg::Wildcard(_) => None,
            })),
            core::Stmt::Filter { expr, .. } => out.push(expr),
            core::Stmt::Match { rhs, .. } => match rhs {
                core::Rhs::Expr(e) => out.push(e),
                core::Rhs::Aggregate { arg, .. } => out.extend(arg.as_ref()),
            },
            core::Stmt::Assert { .. } | core::Stmt::Input { .. } => {}
        }
    }
    out
}

fn walk(e: &core::Expr, f: &mut impl FnMut(&core::Expr)) {
    f(e);
    match e {
        core::Expr::Unary { operand, .. } => walk(operand, f),
        core::Expr::Binary { lhs, rhs, .. } => {
            walk(lhs, f);
            walk(rhs, f);
        }
        core::Expr::Call { args, .. } | core::Expr::ArrayLit { elems: args, .. } => {
            for a in args {
                walk(a, f);
            }
        }
        core::Expr::DictLit { entries, .. } => {
            for (k, v) in entries {
                walk(k, f);
                walk(v, f);
            }
        }
        core::Expr::RecordLit { fields, .. } => {
            for (_, v) in fields {
                walk(v, f);
            }
        }
        _ => {}
    }
}

/// Where a rule binds a variable, for a phase-3 diagnostic's span.
fn binding_span(rule: &core::Rule, name: &str) -> Option<Span> {
    for stmt in &rule.body {
        match stmt {
            core::Stmt::Match {
                lhs: core::Pattern::Var { name: n, span },
                ..
            } if n == name => return Some(*span),
            core::Stmt::Atom { args, .. } => {
                for (_, arg) in args {
                    if let core::Arg::Expr(core::Expr::Var { name: n, span }) = arg
                        && n == name
                    {
                        return Some(*span);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn open_diagnostic(open: Open, span: Span) -> Option<Diagnostic> {
    let message = match open {
        // Already reported; saying so again would be the second diagnostic an
        // exact-set match forbids.
        Open::Reported => return None,
        Open::Numeric => {
            "this literal has no type here; nothing determines whether it is `i64` or `f64`"
        }
        Open::Absence => "cannot infer a type for `NONE` here",
        Open::Container => "this empty container has no element type here",
        Open::Nothing => "nothing determines this type",
    };
    Some(Diagnostic::error(Pass::Infer, span, message))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

impl Cx {
    fn finish(self) -> Result<Typed, Vec<Diagnostic>> {
        let mut relations = BTreeMap::new();
        for (name, columns) in &self.known {
            let mut cols: Vec<(String, Type)> = Vec::new();
            for (column, ty) in &columns.cols {
                match settle(ty) {
                    Ok(t) => cols.push((column.clone(), t)),
                    // `check` has already reported anything unsettled, so this
                    // is unreachable — and a relation missing a column would
                    // mislead the emitter more than an error does.
                    Err(_) => {
                        return Err(vec![Diagnostic::error(
                            Pass::Infer,
                            None,
                            format!("relation `{name}` column `{column}` has no type"),
                        )]);
                    }
                }
            }
            cols.sort_by(|a, b| a.0.cmp(&b.0));
            relations.insert(
                name.clone(),
                Relation {
                    columns: cols,
                    kind: if self.inputs.contains_key(name) {
                        Kind::Input
                    } else {
                        Kind::Derived
                    },
                },
            );
        }

        let mut decls = Vec::new();
        for rule in &self.rules {
            let typed = self.type_rule(rule);
            let mut vars = BTreeMap::new();
            for (name, ty) in &typed.vars {
                match settle(ty) {
                    Ok(t) => {
                        vars.insert(name.clone(), t);
                    }
                    // Unreachable for the same reason as the column arm above —
                    // `check_rule` settles every variable before this runs — and
                    // worth the same treatment. A dropped variable reads to
                    // anything downstream as a rule that never bound it, which
                    // is a harder thing to recognise than an error saying so.
                    Err(_) => {
                        return Err(vec![Diagnostic::error(
                            Pass::Infer,
                            rule.span,
                            format!(
                                "rule for `{}` variable `{name}` has no type",
                                rule.head.relation
                            ),
                        )]);
                    }
                }
            }
            let after = after(rule);
            decls.push(Decl::Rule(TypedRule {
                rule: rule.clone(),
                vars,
                partials: typed.partials,
                asserts: typed.filters,
                group: group_of(rule, &after),
            }));
        }
        for fact in &self.facts {
            decls.push(Decl::Fact(fact.clone()));
        }

        Ok(Typed { relations, decls })
    }
}
