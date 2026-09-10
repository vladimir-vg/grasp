//! Desugaring — `docs/grasp/semantics.md`, "Desugaring".
//!
//! Several surface forms stand for others. This rewrites them, so everything
//! below sees the smaller [`crate::core`] language and no pass has to remember
//! that `a ++ b` and `concat(a, b)` are the same thing.
//!
//! Two rows of that table are performed by the parser and must not be done
//! again here: `x:` becomes `x: x` as the argument list is read — in an atom
//! and in a pattern both — and both dict spellings arrive as `(key, value)`
//! pairs because the AST has only one dict form. What is left is `++`, field
//! access and `not` in expression position.
//!
//! **The destructures are not expanded here.** They expand into a binding plus
//! the checks that make the pattern exact, and two of those need types: the
//! narrowing after a dict `get` has to name the value type, and a bound record
//! remainder is a literal over the fields left. So they survive into the core
//! as [`core::Pattern::Dict`] and [`core::Pattern::Record`], are typed in
//! `infer`, and are expanded in `plan` — the same route an unnest takes, for
//! the same reason. What this pass does to them is put their fields in a
//! canonical order and refuse the two that grasp-dbsp cannot express.

use crate::ast;
use crate::ast::Shape;
use crate::core;
use crate::diag::{Diagnostic, Pass, Span};

pub fn desugar(program: &ast::Program) -> Result<core::Program, Vec<Diagnostic>> {
    let mut out = Vec::with_capacity(program.len());
    for decl in program {
        if let Some(decl) = decl_of(decl).map_err(|d| vec![d])? {
            out.push(decl);
        }
    }
    Ok(out)
}

/// `None` for a function typespec, which is the one declaration with nothing
/// below it: what the compiler knows about a callable is [`core::Builtin`], and
/// a spec restates that rather than supplying it.
fn decl_of(decl: &ast::Decl) -> Result<Option<core::Decl>, Diagnostic> {
    Ok(Some(match decl {
        ast::Decl::Function(_) => return Ok(None),
        ast::Decl::Spec(s) => core::Decl::Spec(core::Spec {
            relation: s.relation.clone(),
            columns: s.columns.clone(),
            span: s.span,
        }),
        ast::Decl::Fact(f) => core::Decl::Fact(core::Fact {
            relation: f.relation.clone(),
            args: closed_args(&f.args)?,
            span: f.span,
        }),
        ast::Decl::Rule(r) => {
            let mut body: Vec<core::Stmt> = r.body.iter().map(stmt_of).collect::<Result<_, _>>()?;
            body.extend(head_aggregates(&r.head.args)?);
            core::Decl::Rule(core::Rule {
                head: core::Head {
                    relation: r.head.relation.clone(),
                    args: closed_args(&r.head.args)?,
                    span: r.head.span,
                },
                body,
                span: r.span,
            })
        }
    }))
}

/// A head's or a fact's arguments, which carry no wildcard.
///
/// The parser rejects one there, so the arm below is unreachable — but it is
/// written as a diagnostic rather than a panic, because a reachable internal
/// error is worse than a redundant check.
///
/// An aggregate argument becomes the variable its column names; the match that
/// binds it is [`head_aggregates`].
fn closed_args(args: &[ast::KvArg]) -> Result<Vec<(String, core::Expr)>, Diagnostic> {
    args.iter()
        .map(|a| match &a.value {
            ast::Arg::Expr(e) => Ok((a.column.clone(), expr_of(e)?)),
            ast::Arg::Aggregate { span, .. } => Ok((
                a.column.clone(),
                core::Expr::Var {
                    name: a.column.clone(),
                    span: *span,
                },
            )),
            ast::Arg::Wildcard(span) => Err(Diagnostic::error(
                Pass::Desugar,
                *span,
                "the wildcard `_` produces no value, so it cannot stand here",
            )),
        })
        .collect()
}

/// The matches a head's aggregate arguments stand for.
///
/// `q(total: sum<sal>) <- …` is `q(total: total) <- …, total := sum<sal>` — the
/// `total:` shorthand applied one step further, with the column naming the
/// variable. No fresh name is invented, so nothing here depends on the order
/// the columns were written in, and a diagnostic about the variable says
/// `total` rather than something the program never wrote.
///
/// A column whose name a variable may not have is refused by the parser, and a
/// body that already binds this name is refused by the aggregate scope check —
/// "bound by the body and again from the aggregate result" is exactly what that
/// is, so it needs no second rule here.
fn head_aggregates(args: &[ast::KvArg]) -> Result<Vec<core::Stmt>, Diagnostic> {
    args.iter()
        .filter_map(|a| match &a.value {
            ast::Arg::Aggregate {
                function,
                arg,
                span,
            } => Some((a, function, arg, span)),
            _ => None,
        })
        .map(|(a, function, arg, span)| {
            Ok(core::Stmt::Match {
                lhs: core::Pattern::Var {
                    name: a.column.clone(),
                    span: *span,
                },
                rhs: core::Rhs::Aggregate {
                    function: *function,
                    arg: arg.as_ref().map(expr_of).transpose()?,
                    span: *span,
                },
                span: *span,
            })
        })
        .collect()
}

fn stmt_of(stmt: &ast::Stmt) -> Result<core::Stmt, Diagnostic> {
    Ok(match stmt {
        ast::Stmt::Atom {
            relation,
            args,
            negated,
            span,
        } => core::Stmt::Atom {
            relation: relation.clone(),
            args: args
                .iter()
                .map(|a| {
                    Ok((
                        a.column.clone(),
                        match &a.value {
                            ast::Arg::Expr(e) => core::Arg::Expr(expr_of(e)?),
                            ast::Arg::Wildcard(s) => core::Arg::Wildcard(*s),
                            // The parser refuses one in an atom, where it would
                            // be folding the very thing it stands in.
                            ast::Arg::Aggregate { span, .. } => {
                                return Err(Diagnostic::error(
                                    Pass::Desugar,
                                    *span,
                                    "an aggregate cannot stand in an atom's argument",
                                ));
                            }
                        },
                    ))
                })
                .collect::<Result<_, Diagnostic>>()?,
            negated: *negated,
            span: *span,
        },
        ast::Stmt::Match { lhs, rhs, span } => core::Stmt::Match {
            lhs: pattern_of(lhs)?,
            rhs: match rhs {
                ast::Rhs::Expr(e) => core::Rhs::Expr(expr_of(e)?),
                ast::Rhs::Aggregate {
                    function,
                    arg,
                    span,
                } => core::Rhs::Aggregate {
                    function: *function,
                    arg: arg.as_ref().map(expr_of).transpose()?,
                    span: *span,
                },
            },
            span: *span,
        },
        ast::Stmt::Filter { expr, span } => core::Stmt::Filter {
            expr: expr_of(expr)?,
            span: *span,
        },
        ast::Stmt::Assert { variable, ty, span } => core::Stmt::Assert {
            variable: variable.clone(),
            ty: ty.clone(),
            span: *span,
        },
        ast::Stmt::Input { span } => core::Stmt::Input { span: *span },
    })
}

/// A destructure's fields, by key.
///
/// A pattern names its keys, so `{a: x, b: y}` and `{b: y, a: x}` are one
/// pattern — unlike an unnest, whose variables are positional. Sorting here is
/// what makes them one thing everywhere downstream: the structural key renders
/// this order and `plan` expands in it, so two spellings key alike *and* emit
/// alike, which is what `compilation.md` asks of a key. The parser has already
/// rejected a repeated key, so nothing is lost.
fn sorted_fields(fields: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = fields.to_vec();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn pattern_of(p: &ast::Pattern) -> Result<core::Pattern, Diagnostic> {
    Ok(match p {
        ast::Pattern::Var { name, span } => core::Pattern::Var {
            name: name.clone(),
            span: *span,
        },
        // Generative rather than sugar: an unnest becomes a `flat_map`, not a
        // binding and some filters, so it survives into the core unchanged.
        ast::Pattern::Unnest { vars, kind, span } => core::Pattern::Unnest {
            vars: vars.clone(),
            kind: *kind,
            span: *span,
        },
        // The dict and record patterns survive into the core for the reason an
        // unnest does: expanding them needs types, and this runs before there
        // are any.
        ast::Pattern::Dict { fields, rest, span } => core::Pattern::Dict {
            fields: sorted_fields(fields),
            rest: rest.clone(),
            span: *span,
        },
        ast::Pattern::Record { fields, rest, span } => core::Pattern::Record {
            fields: sorted_fields(fields),
            rest: rest.clone(),
            span: *span,
        },
        // Positional, so its variables keep the order they were written in —
        // the sorting above is for patterns that name their parts.
        ast::Pattern::Array { elems, rest, span } => core::Pattern::Array {
            elems: elems.clone(),
            rest: rest.clone(),
            span: *span,
        },
    })
}

fn expr_of(e: &ast::Expr) -> Result<core::Expr, Diagnostic> {
    Ok(match e {
        ast::Expr::Lit { value, span } => core::Expr::Lit {
            value: value.clone(),
            span: *span,
        },
        ast::Expr::Var { name, span } => core::Expr::Var {
            name: name.clone(),
            span: *span,
        },

        // `d[k]` → `dict:get(d, k)`. The key is an ordinary expression, which
        // is the whole difference from `s.f`: a record's field is part of its
        // type and has to be written, a dict's key is a value.
        // Not rewritten here: `d[k]` is `dict:get` and `arr[i]` is `array:at`,
        // and only the subject's type tells them apart. `infer` settles it.
        ast::Expr::Index { base, key, span } => core::Expr::Index {
            base: Box::new(expr_of(base)?),
            key: Box::new(expr_of(key)?),
            span: *span,
        },

        // A slice needs no type — a dict cannot be sliced — so this one really
        // is sugar. Written as the call it stands for and resolved by the
        // ordinary path, so the defaults live in `Builtin::signatures` and
        // nowhere else.
        ast::Expr::Slice {
            base,
            start,
            stop,
            step,
            span,
        } => {
            let named = |name: &str, part: &Option<Box<ast::Expr>>| {
                part.as_ref().map(|e| (name.to_string(), (**e).clone()))
            };
            return expr_of(&ast::Expr::Call {
                name: core::Builtin::ArraySlice.as_str().to_string(),
                positional: vec![(**base).clone()],
                keyword: [
                    named("start", start),
                    named("stop", stop),
                    named("step", step),
                ]
                .into_iter()
                .flatten()
                .collect(),
                span: *span,
            });
        }

        // `s.f` → `record:get(s, "f")`, and `s.a.b` by recursion.
        ast::Expr::Field { base, name, span } => core::Expr::Call {
            callee: core::Builtin::RecordGet,
            args: vec![
                expr_of(base)?,
                core::Expr::Lit {
                    value: ast::Lit::Str(name.clone()),
                    span: *span,
                },
            ],
            span: *span,
        },

        // `not e` → `boolean:not(e)`. Unary minus is not sugar.
        ast::Expr::Unary { op, operand, span } => match op {
            ast::UnOp::Not => core::Expr::Call {
                callee: core::Builtin::BooleanNot,
                args: vec![expr_of(operand)?],
                span: *span,
            },
            ast::UnOp::Neg => core::Expr::Unary {
                op: *op,
                operand: Box::new(expr_of(operand)?),
                span: *span,
            },
        },

        // `a ++ b` → `concat(a, b)`.
        ast::Expr::Binary { op, lhs, rhs, span } => match op {
            ast::BinOp::Concat => core::Expr::Call {
                callee: core::Builtin::StringConcat,
                args: vec![expr_of(lhs)?, expr_of(rhs)?],
                span: *span,
            },
            _ => core::Expr::Binary {
                op: *op,
                lhs: Box::new(expr_of(lhs)?),
                rhs: Box::new(expr_of(rhs)?),
                span: *span,
            },
        },

        ast::Expr::Call {
            name,
            positional,
            keyword,
            span,
        } => {
            // Resolving the name here is what the `Builtin` enum is for: below
            // this point a callable is a variant, not a string two passes have
            // to spell the same way.
            let Some(callee) = core::Builtin::from_name(name) else {
                // A name the *library* has and this compiler has not is a gap,
                // not a mistake — `stdlib.grasp` declares the whole library, so
                // the burn-down carries what is left of it.
                return Err(if core::DESIGNED.contains(&name.as_str()) {
                    Diagnostic::unimplemented(Pass::Desugar, *span, name.clone())
                } else {
                    Diagnostic::error(
                        Pass::Desugar,
                        *span,
                        format!("there is no callable `{name}`"),
                    )
                });
            };
            let sig = resolve(callee, positional, keyword, *span)?;
            // Resolution is what puts the arguments in order: below here a call
            // is positional, and every pass after indexes it.
            let mut args: Vec<core::Expr> =
                positional.iter().map(expr_of).collect::<Result<_, _>>()?;
            for param in sig.keyword {
                match keyword.iter().find(|(name, _)| name == param.name) {
                    Some((_, value)) => args.push(expr_of(value)?),
                    // Left out, and the signature says what that means — which
                    // is what makes `array:slice`'s eight variants eight ways to
                    // write one four-argument call.
                    None => args.push(core::Expr::Lit {
                        value: param
                            .default
                            .expect("the shape matched, so an absent keyword has a default")
                            .literal(),
                        span: *span,
                    }),
                }
            }
            core::Expr::Call {
                callee,
                args,
                span: *span,
            }
        }

        ast::Expr::ArrayLit { elems, span } => core::Expr::ArrayLit {
            elems: elems.iter().map(expr_of).collect::<Result<_, _>>()?,
            span: *span,
        },
        ast::Expr::DictLit { entries, span } => core::Expr::DictLit {
            entries: entries
                .iter()
                .map(|(k, v)| Ok((expr_of(k)?, expr_of(v)?)))
                .collect::<Result<_, Diagnostic>>()?,
            span: *span,
        },
        ast::Expr::RecordLit { fields, span } => core::Expr::RecordLit {
            fields: fields
                .iter()
                .map(|(n, v)| Ok((n.clone(), expr_of(v)?)))
                .collect::<Result<_, Diagnostic>>()?,
            span: *span,
        },
    })
}

/// Which variant of a callable a call means — `syntax.md`, "Name resolution".
///
/// **By shape, not by type**: the number of arguments given by position and the
/// *set* of keyword names given, which is Erlang's dispatch rather than C++'s.
/// Types are checked afterwards, in `infer`, against the variant this picked —
/// so a call that matches no shape is reported here, in the words of the call,
/// rather than as a type error about an argument the program did not write.
fn resolve(
    callee: core::Builtin,
    positional: &[ast::Expr],
    keyword: &[(String, ast::Expr)],
    span: Span,
) -> Result<&'static core::Signature, Diagnostic> {
    let shape = Shape::new(
        positional.len(),
        keyword.iter().map(|(name, _)| name.clone()),
    );
    callee.resolve(&shape).ok_or_else(|| {
        let name = callee.as_str();
        let taken: Vec<String> = callee
            .signatures()
            .iter()
            .flat_map(|s| s.shapes())
            .map(|s| format!("`{s}`"))
            .collect();
        Diagnostic::error(
            Pass::Desugar,
            span,
            format!(
                "`{name}` is called {}, and this call is `{shape}`",
                taken.join(" or ")
            ),
        )
    })
}
