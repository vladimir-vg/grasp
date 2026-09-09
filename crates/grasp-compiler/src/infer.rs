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
use crate::ty::{Narrowing, Open, Ty, assignable, compose, impose, narrows, settle};
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
    pub span: Span,
}

#[derive(Debug)]
pub struct TypedRule {
    pub rule: core::Rule,
    /// Every variable the rule binds, and its type.
    pub vars: BTreeMap<String, Type>,
    /// The assertions that became runtime filters, in body order.
    pub asserts: Vec<Assert>,
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
    filters: Vec<Assert>,
    head: BTreeMap<String, Ty>,
    /// The first thing wrong, which is the only thing reported.
    fault: Option<Diagnostic>,
}

// ---------------------------------------------------------------------------
// Phase 1 — one rule's constraints, composed
// ---------------------------------------------------------------------------

impl Cx {
    /// Collect every constraint the rule places on its variables, compose them,
    /// and report what the head columns come out as.
    ///
    /// Runs during the fixpoint, where `fault` is discarded, and again during
    /// checking, where it is the rule's one diagnostic.
    fn type_rule(&self, rule: &core::Rule) -> RuleTypes {
        let mut vars: BTreeMap<String, Ty> = BTreeMap::new();
        let mut fault: Option<Diagnostic> = None;
        let mut asserts: Vec<(&String, &Type, Span)> = Vec::new();
        let mut filters: Vec<Assert> = Vec::new();
        let note = |d: Diagnostic, fault: &mut Option<Diagnostic>| {
            if fault.is_none() {
                *fault = Some(d);
            }
        };

        let constrain = |vars: &mut BTreeMap<String, Ty>,
                         name: &str,
                         ty: Ty,
                         span: Span,
                         fault: &mut Option<Diagnostic>| {
            let merged = match vars.get(name) {
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
            vars.insert(name.to_string(), merged);
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
                        constrain(&mut vars, name, ty, *vs, &mut fault);
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
                                if let Some(v) = names.first() {
                                    constrain(&mut vars, v, elem, *ps, &mut fault);
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
                                    constrain(&mut vars, n, k, *ps, &mut fault);
                                }
                                if let Some(n) = names.get(1) {
                                    constrain(&mut vars, n, v, *ps, &mut fault);
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

                // Applied after the walk: an assertion is a claim about a
                // variable, not a contribution to it, and it has to see what
                // everything else concluded before it can override that.
                core::Stmt::Assert { variable, ty, span } => {
                    asserts.push((variable, ty, *span));
                }

                core::Stmt::Input { .. } => {}
            }
        }

        // Assertions, now that every other statement has had its say.
        //
        // An assertion *replaces* a variable's type rather than composing with
        // it: `n : optional(string)` from the atom and `n :: string` do not
        // meet in the lattice, they are a narrowing — and after it, "`n` is
        // `string` for everything below". A rule body being a set, "below" is
        // wherever the filter lands, and the filter lands as early as the
        // variable is bound; so one type for the whole rule is the truth.
        for (variable, ty, span) in asserts {
            let want = Ty::known(ty);
            let Some(have) = vars.get_mut(variable) else {
                // Unbound: the safety check reports it, and saying so twice
                // would be two diagnostics for one fault.
                continue;
            };
            match narrows(have, &want) {
                Narrowing::Already => *have = want,
                Narrowing::Filter { cast } => {
                    filters.push(Assert {
                        variable: variable.clone(),
                        ty: ty.clone(),
                        cast,
                        span,
                    });
                    *have = want;
                }
                Narrowing::Unsupported => note(
                    Diagnostic::unimplemented(
                        Pass::Infer,
                        span,
                        "narrowing that keeps its wrapper",
                    ),
                    &mut fault,
                ),
                Narrowing::Never => note(
                    Diagnostic::error(
                        Pass::Infer,
                        span,
                        format!(
                            "no `{have}` value is a `{ty}`: this assertion would discard \
                             every row"
                        ),
                    ),
                    &mut fault,
                ),
            }
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
                            "`{name}` is `{ty}` here, but column `{column}` of `{}` is                              `{want}`",
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

        RuleTypes {
            vars,
            head,
            fault,
            filters,
        }
    }

    fn rhs_ty(&self, rhs: &core::Rhs, vars: &BTreeMap<String, Ty>) -> (Ty, Option<Diagnostic>) {
        match rhs {
            core::Rhs::Expr(e) => self.expr_ty(e, vars),
            core::Rhs::Aggregate { function, arg, .. } => {
                let (inner, err) = match arg {
                    Some(e) => self.expr_ty(e, vars),
                    None => (Ty::I64, None),
                };
                let ty = match function {
                    // "`count<>` takes no argument and counts rows."
                    Aggregator::Count => Ty::I64,
                    // "`avg` yields `f64` whatever it was given, since a mean
                    //  is not an integer."
                    Aggregator::Avg => Ty::F64,
                    _ => inner,
                };
                (ty, err)
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

        match callee {
            B::Abs | B::Floor | B::Ceil | B::Round => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                if numeric(&args[0]) || open(&args[0]) {
                    (args[0].clone(), None)
                } else {
                    wrong()
                }
            }
            B::Length => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::String | Ty::Array(_) | Ty::Dict(..) | Ty::Unknown => (Ty::I64, None),
                    _ => wrong(),
                }
            }
            B::Concat => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                if args.iter().all(|t| matches!(t, Ty::String | Ty::Unknown)) {
                    (Ty::String, None)
                } else {
                    wrong()
                }
            }
            B::Lower | B::Upper | B::Trim => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::String | Ty::Unknown => (Ty::String, None),
                    _ => wrong(),
                }
            }
            B::BooleanNot => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Boolean | Ty::Unknown => (Ty::Boolean, None),
                    _ => wrong(),
                }
            }
            B::If => {
                if let Some(d) = arity(3) {
                    return (Ty::Error, Some(d));
                }
                if !matches!(args[0], Ty::Boolean | Ty::Unknown) {
                    return wrong();
                }
                match compose(&args[1], &args[2]) {
                    Ok(t) => (t, None),
                    Err(_) => wrong(),
                }
            }
            B::Keys => {
                if let Some(d) = arity(1) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Json => (
                        Ty::Optional(Box::new(Ty::Array(Box::new(Ty::String)))),
                        None,
                    ),
                    Ty::Dict(k, _) => (Ty::Array(k.clone()), None),
                    Ty::Unknown => (Ty::Unknown, None),
                    _ => wrong(),
                }
            }
            B::Entries => {
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
            B::DictGet => {
                if let Some(d) = arity(2) {
                    return (Ty::Error, Some(d));
                }
                match &args[0] {
                    Ty::Dict(_, v) => (Ty::Optional(v.clone()), None),
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
        // Safety, before typing: an unbound variable makes everything after it
        // unknowable, and reporting both would be two diagnostics for one
        // mistake.
        if let Some(d) = self.check_safety(rule) {
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
                core::Stmt::Match { lhs, .. } => match lhs {
                    core::Pattern::Var { name, .. } => {
                        bound.insert(name);
                    }
                    core::Pattern::Unnest { vars, .. } => {
                        bound.extend(vars.iter().map(String::as_str));
                    }
                },
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
            decls.push(Decl::Rule(TypedRule {
                rule: rule.clone(),
                vars,
                asserts: typed.filters,
            }));
        }
        for fact in &self.facts {
            decls.push(Decl::Fact(fact.clone()));
        }

        Ok(Typed { relations, decls })
    }
}
