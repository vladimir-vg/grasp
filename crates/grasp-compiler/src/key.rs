//! Structural keys — `docs/grasp/compilation.md`, "Determinism".
//!
//! Where the cost model leaves a choice open, something still has to settle it,
//! and `syntax.md` says what may not: *"Nothing downstream is allowed to key on
//! source position — spans exist for diagnostics and nothing else, so that
//! reformatting a program cannot change what it compiles to."* "Whichever came
//! first" is exactly what that forbids, which rules out source order and
//! iteration order alike.
//!
//! So every tie is broken by a **structural key**: a canonical rendering of a
//! node's own content, built from relation, column and variable names, literal
//! values and operator spellings — what a program *means* — and never from a
//! line, a column, or which statement was written first.
//!
//! Three things about this module are worth knowing before changing it.
//!
//! **It is not the emitter.** These strings are grasp-side and are never
//! written to a file; the emitter produces grasp-dbsp, rewrites `=` to `==`,
//! resolves variables against a row binder, and undoes desugaring. Two printers
//! with two jobs. This one exists to be *compared*, which is why it is a
//! `String` rather than an `Ord` on `core::Expr` — [`crate::ast::Lit::Float`]
//! holds an `f64` and `f64` is not `Ord`.
//!
//! **The key need not be injective**, and that is the point. Two candidates
//! with equal keys emit identical grasp-dbsp, because emission reads a node's
//! content and its position and nothing else. The key is total on everything
//! observable, which is all determinism needs.
//!
//! **Floats go through [`float`]**, shared with the emitter. `format!("{}",
//! 1.0f64)` is `1`, which grasp-dbsp lexes as an integer; if the two printers
//! disagreed about that, a normalization fixture would fail for a reason nobody
//! would guess.

use crate::ast::{Aggregator, BinOp, Lit, UnOp};
use crate::core;
use std::fmt::Write;

/// The canonical text of one literal.
///
/// Strings are quoted and escaped so that `"a"` and `a` cannot collide, which
/// they would if a string rendered as its own contents.
pub fn lit(value: &Lit) -> String {
    match value {
        Lit::Int(n) => n.to_string(),
        Lit::Float(f) => float(*f),
        Lit::Str(s) => format!("{s:?}"),
        Lit::Bool(b) => b.to_string(),
        Lit::None => "NONE".to_string(),
    }
}

/// A float that reads back as a float.
///
/// `1.0` must not render as `1`: both languages lex that as an integer, and an
/// integer literal is not an `f64` in either. `{:?}` is the shortest rendering
/// that round-trips, which is what both printers want.
pub fn float(f: f64) -> String {
    let s = format!("{f:?}");
    // `{:?}` gives `inf` and `NaN`, which no literal spells.
    //
    // **Known problem:** a program can contain one anyway. `lex` reads a float
    // with Rust's `f64` parse, which returns `Ok(inf)` on overflow rather than
    // an error, so `1.0e400` lexes to a non-finite `Lit::Float`. In a debug
    // build this fires; in a release one it renders `inf`, which grasp-dbsp
    // cannot lex. The fix belongs in `lex`, where the range is known.
    debug_assert!(f.is_finite(), "a literal float is finite");
    s
}

/// The canonical text of an expression.
///
/// Fully parenthesised: precedence is a property of the grammar that produced
/// this tree, and re-deriving it here would be a second place to get it wrong.
pub fn expr(e: &core::Expr) -> String {
    let mut out = String::new();
    write_expr(&mut out, e);
    out
}

fn write_expr(out: &mut String, e: &core::Expr) {
    match e {
        core::Expr::Lit { value, .. } => out.push_str(&lit(value)),
        core::Expr::Var { name, .. } => out.push_str(name),
        core::Expr::Unary { op, operand, .. } => {
            let _ = write!(out, "({}", unop(*op));
            if *op == UnOp::Not {
                out.push(' ');
            }
            write_expr(out, operand);
            out.push(')');
        }
        core::Expr::Binary { op, lhs, rhs, .. } => {
            out.push('(');
            write_expr(out, lhs);
            let _ = write!(out, " {} ", binop(*op));
            write_expr(out, rhs);
            out.push(')');
        }
        core::Expr::Call { callee, args, .. } => {
            let _ = write!(out, "{}(", callee.as_str());
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(out, a);
            }
            out.push(')');
        }
        core::Expr::ArrayLit { elems, .. } => {
            out.push('[');
            for (i, a) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(out, a);
            }
            out.push(']');
        }
        core::Expr::DictLit { entries, .. } => {
            // Entries keep their written order: a dict literal's keys need not
            // be literals, so there is nothing to sort them by that is not
            // itself this rendering — and two entries with one key is a runtime
            // question, not a structural one.
            //
            // **Known problem:** `semantics.md` says a literal's entries are
            // "sorted by key and deduplicated, so two literals naming the same
            // entries in different orders build one value". They do build one
            // value — grasp-dbsp sorts them — but they key differently here and
            // so emit different text, which is the property `equivalent_to`
            // exists to hold. A pattern's fields are sorted in `desugar` for
            // exactly this reason; a literal's cannot be, while its keys may be
            // computed.
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(out, k);
                out.push_str(" => ");
                write_expr(out, v);
            }
            out.push('}');
        }
        core::Expr::RecordLit { fields, .. } => {
            // Sorted: `types.md` says a record's fields are a set, so
            // `record(a: 1, b: 2)` and `record(b: 2, a: 1)` are one value and
            // must be one key.
            let mut sorted: Vec<&(String, core::Expr)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push_str("record(");
            for (i, (name, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{name}: ");
                write_expr(out, v);
            }
            out.push(')');
        }
    }
}

pub fn unop(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "not",
    }
}

pub fn binop(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "=",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Concat => "++",
    }
}

/// An atom's key: `r(c₁: a₁, …)` with **columns sorted by name**, each argument
/// a variable name, `_`, or a literal's canonical text.
pub fn atom(relation: &str, args: &[(String, core::Arg)]) -> String {
    let mut sorted: Vec<&(String, core::Arg)> = args.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = format!("{relation}(");
    for (i, (column, arg)) in sorted.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{column}: ");
        match arg {
            core::Arg::Expr(e) => write_expr(&mut out, e),
            core::Arg::Wildcard(_) => out.push('_'),
        }
    }
    out.push(')');
    out
}

/// A pattern's key. An unnest keeps its variables in written order — they are
/// positional, so `(k, v)` and `(v, k)` bind differently and are not one thing.
///
/// A destructure's fields are *named*, so they are not positional and two
/// spellings of the same fields are one pattern. `desugar` has already sorted
/// them, so rendering them in order is rendering them canonically.
pub fn pattern(p: &core::Pattern) -> String {
    match p {
        core::Pattern::Var { name, .. } => name.clone(),
        core::Pattern::Unnest { vars, kind, .. } => {
            let star = match kind {
                core::UnnestKind::Array => "*",
                core::UnnestKind::Dict => "**",
            };
            format!("({}) {star}", vars.join(", "))
        }
        core::Pattern::Dict { fields, rest, .. } => {
            format!("{{{}}}", destructure(fields, rest))
        }
        core::Pattern::Record { fields, rest, .. } => {
            format!("record({})", destructure(fields, rest))
        }
    }
}

/// The shared inside of the two destructure keys: `a: x, b: y, **r`.
fn destructure(fields: &[(String, String)], rest: &core::Rest) -> String {
    let mut parts: Vec<String> = fields.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    match rest {
        core::Rest::None => {}
        core::Rest::Ignore => parts.push("**".to_string()),
        core::Rest::Bind(v) => parts.push(format!("**{v}")),
    }
    parts.join(", ")
}

/// A right-hand side's key — an expression, or an aggregate's spelling.
pub fn rhs(r: &core::Rhs) -> String {
    match r {
        core::Rhs::Expr(e) => expr(e),
        core::Rhs::Aggregate { function, arg, .. } => match arg {
            Some(a) => format!("{}<{}>", aggregator(*function), expr(a)),
            None => format!("{}<>", aggregator(*function)),
        },
    }
}

pub fn aggregator(a: Aggregator) -> &'static str {
    match a {
        Aggregator::Sum => "sum",
        Aggregator::Count => "count",
        Aggregator::Min => "min",
        Aggregator::Max => "max",
        Aggregator::Avg => "avg",
    }
}

/// One body statement's key, per `compilation.md`'s table.
///
/// `Input` has no key because it is not a body statement in any rule that
/// reaches planning — `r(…) <- input` is a declaration, and `infer` has already
/// turned it into [`crate::infer::Kind::Input`].
pub fn stmt(s: &core::Stmt) -> String {
    match s {
        core::Stmt::Atom {
            relation,
            args,
            negated,
            ..
        } => {
            let a = atom(relation, args);
            if *negated { format!("not {a}") } else { a }
        }
        core::Stmt::Filter { expr: e, .. } => expr(e),
        core::Stmt::Match { lhs, rhs: r, .. } => format!("{} := {}", pattern(lhs), rhs(r)),
        core::Stmt::Assert { variable, ty, .. } => format!("{variable} :: {ty}"),
        core::Stmt::Input { .. } => "input".to_string(),
    }
}

/// A whole rule's key: its body's, sorted, then its head keyed as an atom is.
///
/// Sorted rather than written order, because a rule body is a set — that is the
/// premise the whole normalization fixture file rests on. The head is in the key
/// because two rules can share a body and differ only in what they produce, and
/// the union that sums them has to order those two.
pub fn rule(r: &core::Rule) -> String {
    let mut body: Vec<String> = r.body.iter().map(stmt).collect();
    body.sort();
    let head: Vec<(String, core::Arg)> = r
        .head
        .args
        .iter()
        .map(|(c, e)| (c.clone(), core::Arg::Expr(e.clone())))
        .collect();
    format!("{} <- {}", atom(&r.head.relation, &head), body.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Span;

    fn sp() -> Span {
        Span::new(1, 1, 0)
    }

    fn var(name: &str) -> core::Expr {
        core::Expr::Var {
            name: name.to_string(),
            span: sp(),
        }
    }

    fn int(n: i64) -> core::Expr {
        core::Expr::Lit {
            value: Lit::Int(n),
            span: sp(),
        }
    }

    #[test]
    fn a_float_reads_back_as_a_float() {
        // The bug this guards: `{}` renders 1.0 as `1`, which both languages
        // lex as an integer.
        assert_eq!(float(1.0), "1.0");
        assert_eq!(lit(&Lit::Float(1.0)), "1.0");
        assert_eq!(lit(&Lit::Float(0.5)), "0.5");
    }

    #[test]
    fn a_string_cannot_collide_with_a_bare_name() {
        assert_eq!(lit(&Lit::Str("a".into())), "\"a\"");
        assert_ne!(lit(&Lit::Str("a".into())), expr(&var("a")));
    }

    #[test]
    fn an_atom_sorts_its_columns() {
        let args = |order: [&str; 2]| {
            order
                .iter()
                .map(|c| (c.to_string(), core::Arg::Expr(var(c))))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            atom("r", &args(["a", "b"])),
            atom("r", &args(["b", "a"])),
            "columns are a set, so their written order is not information"
        );
    }

    #[test]
    fn a_record_sorts_its_fields_but_an_array_does_not() {
        let rec = |order: [&str; 2]| core::Expr::RecordLit {
            fields: order.iter().map(|f| (f.to_string(), int(1))).collect(),
            span: sp(),
        };
        assert_eq!(expr(&rec(["a", "b"])), expr(&rec(["b", "a"])));

        let arr = |elems: [i64; 2]| core::Expr::ArrayLit {
            elems: elems.iter().map(|n| int(*n)).collect(),
            span: sp(),
        };
        assert_ne!(
            expr(&arr([1, 2])),
            expr(&arr([2, 1])),
            "an array is ordered; its elements are not a set"
        );
    }

    #[test]
    fn an_expression_is_fully_parenthesised() {
        let e = core::Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(var("a")),
            rhs: Box::new(core::Expr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(var("b")),
                rhs: Box::new(int(2)),
                span: sp(),
            }),
            span: sp(),
        };
        assert_eq!(expr(&e), "(a + (b * 2))");
    }

    #[test]
    fn a_rule_body_is_a_set() {
        let stmts = |order: [i64; 2]| {
            order
                .iter()
                .map(|n| core::Stmt::Match {
                    lhs: core::Pattern::Var {
                        name: format!("v{n}"),
                        span: sp(),
                    },
                    rhs: core::Rhs::Expr(int(*n)),
                    span: sp(),
                })
                .collect::<Vec<_>>()
        };
        let rule_with = |order| core::Rule {
            head: core::Head {
                relation: "r".into(),
                args: vec![("x".into(), var("v0"))],
                span: sp(),
            },
            body: stmts(order),
            span: sp(),
        };
        assert_eq!(
            rule(&rule_with([0, 1])),
            rule(&rule_with([1, 0])),
            "statement order is not information; normalization.yaml asserts this"
        );
    }

    #[test]
    fn an_unnest_keeps_its_variable_order() {
        let unnest = |vars: [&str; 2]| core::Pattern::Unnest {
            vars: vars.iter().map(|v| v.to_string()).collect(),
            kind: core::UnnestKind::Dict,
            span: sp(),
        };
        assert_ne!(
            pattern(&unnest(["k", "v"])),
            pattern(&unnest(["v", "k"])),
            "an unnest binds positionally, so its variables are a sequence"
        );
    }
}
