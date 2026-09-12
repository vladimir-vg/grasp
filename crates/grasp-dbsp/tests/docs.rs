//! The grasp-dbsp in the design documents is grasp-dbsp.
//!
//! Two directories are read, because two documents make claims about this
//! language. `docs/grasp-dbsp/` specifies it. `docs/grasp/mapping.md` shows what
//! `grasp-compiler` emits into it — a claim only this crate can check, which is
//! why the test lives here rather than beside the compiler.
//!
//! Every ```` ```grasp-dbsp ```` block is one of two things:
//!
//! - **A program**, which must *compile*. An excerpt may name streams it does
//!   not declare, and that is the only thing forgiven: a diagnostic about
//!   anything else fails, so a renamed operator, a changed argument order or a
//!   type that stopped being legal is caught in a block that could never be run
//!   whole.
//! - **A fragment** — an expression, a lambda, or the `name := expr` binding an
//!   expression example is written as — which must *parse* where such a thing
//!   may appear. A fragment cannot be typechecked: it names a row variable
//!   nothing here binds, which is what makes it a fragment.
//!
//! A block that is neither fails, and the message says which readings were
//! tried. What is deliberately *not* here is a third reading for "prose that
//! looks like code": a block either carries the tag or it does not, and the
//! grammar blocks beside these carry none.

use grasp_dbsp::diag::Diagnostic;
use grasp_dbsp::expr::Builtin;
use grasp_dbsp::lang::{self, Arg, Decl, DictLit, Expr, ExprKind, OpCall, Rhs};
use grasp_dbsp::typecheck::OPERATORS;
use std::path::{Path, PathBuf};

/// Every ```` ```grasp-dbsp ```` block, with the document it came from.
fn blocks() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for dir in [root.join("docs/grasp"), root.join("docs/grasp-dbsp")] {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path).expect("a doc");
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // Splitting on the fence gives prose at even indices and code at
            // odd ones, the info string being the code block's first line.
            for (i, block) in text.split("```").enumerate() {
                if i % 2 == 1
                    && let Some(body) = block.strip_prefix("grasp-dbsp\n")
                {
                    out.push((name.clone(), body.to_string()));
                }
            }
        }
    }
    out
}

/// A diagnostic an excerpt earns by being one.
///
/// A block showing what a `join` looks like does not declare the two streams it
/// joins, and saying so would bury the line the section is about. Every other
/// diagnostic is a document that disagrees with the language.
///
/// Both messages are pinned by a case in `tests/cases/errors.yaml`, so the
/// wording this reads cannot change without a test saying so.
fn is_excerpt(d: &Diagnostic) -> bool {
    d.message.starts_with("unknown stream ") || d.message.contains("which is not declared")
}

/// The text before a `#` that is not inside a string.
fn without_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Whether `text` parses somewhere a fragment may appear.
///
/// Three places, because this language has three: the right-hand side of a
/// definition, a value inside a function body, and the function literal an
/// operator takes. A fragment written as `d := …` is an expression with a name
/// on it — the form an expression example takes in a function body — so the
/// right-hand side is tried alone as well.
fn parses_as_fragment(text: &str, elsewhere: &[String]) -> bool {
    let clean = |source: String| {
        lang::parse(&source).is_ok_and(|p| unknown_names(&p, elsewhere).is_empty())
    };
    let anywhere = |t: &str| {
        clean(format!("probe := {t}"))
            || clean(format!("probe := map(s, function((row) -> {t}))"))
            || clean(format!("probe := map(s, {t})"))
    };
    let text = text.trim();
    if text.is_empty() {
        return true;
    }
    anywhere(text) || matches!(text.split_once(":="), Some((_, rhs)) if anywhere(rhs.trim()))
}

/// Every operator and builtin a parsed block names, that the language does not
/// have.
///
/// The reading below stops at the first unresolved stream, and an excerpt is
/// full of those — so `join` renamed to `zip` in a block that does not declare
/// its inputs would never be reached. That is the likeliest way a document
/// rots, so the names are checked against the language's own lists rather than
/// left to a pass that will not get there. It is also the only check a
/// *fragment* can carry: an expression naming a row nothing binds cannot be
/// typechecked, but the functions it calls still have to exist.
fn unknown_names(program: &lang::Program, elsewhere: &[String]) -> Vec<String> {
    let mut defined: Vec<&str> = elsewhere.iter().map(String::as_str).collect();
    for decl in &program.decls {
        match decl {
            Decl::Circuit(c) => defined.push(&c.name),
            Decl::Function(f) => defined.push(&f.name),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for decl in &program.decls {
        walk_decl(decl, &defined, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn walk_decl(decl: &Decl, defined: &[&str], out: &mut Vec<String>) {
    match decl {
        Decl::Node { rhs, .. } => match rhs {
            Rhs::Op(call) => walk_op(call, defined, out),
            Rhs::Instantiate(i) | Rhs::Fixpoint(i) => {
                if !defined.contains(&i.circuit.as_str()) {
                    out.push(format!("circuit `{}`", i.circuit));
                }
                for (_, arg) in &i.args {
                    walk_arg(arg, defined, out);
                }
            }
            Rhs::Ref(_) => {}
        },
        Decl::Circuit(c) => {
            for decl in &c.body {
                walk_decl(decl, defined, out);
            }
        }
        Decl::Function(f) => walk_expr(&f.body, defined, out),
        Decl::TypeSpec { .. } => {}
    }
}

fn walk_op(call: &OpCall, defined: &[&str], out: &mut Vec<String>) {
    if !OPERATORS.contains(&call.op.as_str()) && !defined.contains(&call.op.as_str()) {
        out.push(format!("operator `{}`", call.op));
    }
    for arg in &call.args {
        walk_arg(arg, defined, out);
    }
}

fn walk_arg(arg: &Arg, defined: &[&str], out: &mut Vec<String>) {
    match arg {
        Arg::Op(call) => walk_op(call, defined, out),
        Arg::Fun(f) => walk_expr(&f.body, defined, out),
        Arg::Expr(e) => walk_expr(e, defined, out),
        // A bare name is a stream here, or an aggregator — which operator it is
        // decides, and the type checker is what knows.
        Arg::Name(_) | Arg::Str(_) | Arg::Field(_) => {}
    }
}

/// Exhaustive on purpose: a new expression form has to be added here, rather
/// than quietly going unchecked wherever the documents use it.
fn walk_expr(e: &Expr, defined: &[&str], out: &mut Vec<String>) {
    let mut walk = |e| walk_expr(e, defined, out);
    match &e.kind {
        ExprKind::Call(name, args) => {
            if Builtin::from_name(name).is_none() && !defined.contains(&name.as_str()) {
                out.push(format!("`{name}`"));
            }
            for a in args {
                walk_expr(a, defined, out);
            }
        }
        ExprKind::Field(inner, _) | ExprKind::Cast(inner, _) | ExprKind::Unary(_, inner) => {
            walk(inner)
        }
        ExprKind::Binary(_, a, b) => {
            walk(a);
            walk(b);
        }
        ExprKind::Record(fields) => {
            for (_, v) in fields {
                walk_expr(v, defined, out);
            }
        }
        ExprKind::List(items) => {
            for i in items {
                walk_expr(i, defined, out);
            }
        }
        ExprKind::Dict(DictLit::Pairs(pairs)) => {
            for (k, v) in pairs {
                walk_expr(k, defined, out);
                walk_expr(v, defined, out);
            }
        }
        ExprKind::Dict(DictLit::From(inner)) => walk(inner),
        ExprKind::MapArray { array, body, .. } | ExprKind::FilterArray { array, body, .. } => {
            walk(array);
            walk(body);
        }
        ExprKind::None
        | ExprKind::Bool(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Var(_) => {}
    }
}

/// One reading of a whole block, or of one of its lines.
///
/// The program reading is tried first and does not win by parsing: `d :=
/// dict(row.pairs)` is a declaration to the grammar and an expression to a
/// reader, so a program that does not compile falls through to the fragment
/// ladder, and the program's own diagnostic is what a block that is neither
/// reports.
fn is_the_language(text: &str, elsewhere: &[String]) -> Result<(), String> {
    let mut why = None;
    if let Ok(program) = lang::parse(text) {
        let unknown = unknown_names(&program, elsewhere);
        if !unknown.is_empty() {
            // `dict(a)` is an operator to the grammar and a value constructor
            // to the language, so an unknown name is one more reason to read
            // the block as a fragment rather than a verdict on it.
            why = Some(format!(
                "the language has no {} — a name in a document that nothing answers to",
                unknown.join(", ")
            ));
        } else {
            match grasp_dbsp::compile(text) {
                Ok(_) => return Ok(()),
                Err(diags) if diags.iter().all(is_excerpt) => return Ok(()),
                Err(diags) => {
                    why = Some(
                        diags
                            .iter()
                            .map(|d| d.to_string())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
        }
    }
    if parses_as_fragment(text, elsewhere) {
        return Ok(());
    }
    Err(why.unwrap_or_else(|| lang::parse(text).unwrap_err().to_string()))
}

#[test]
fn every_grasp_dbsp_block_in_the_docs_is_the_language() {
    let blocks = blocks();
    assert!(
        blocks.len() >= 30,
        "found only {} grasp-dbsp blocks; the extraction is probably broken",
        blocks.len()
    );

    // A document is the unit a reader reads, so a helper defined in one block
    // and used in the next is defined: `function scale(x)` is written once and
    // called by the two blocks under it.
    let mut defined: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for (file, block) in &blocks {
        let Ok(program) = lang::parse(block) else {
            continue;
        };
        let names = defined.entry(file.as_str()).or_default();
        for decl in &program.decls {
            match decl {
                Decl::Circuit(c) => names.push(c.name.clone()),
                Decl::Function(f) => names.push(f.name.clone()),
                _ => {}
            }
        }
    }
    let none = Vec::new();

    let mut failures = Vec::new();
    for (file, block) in &blocks {
        let elsewhere = defined.get(file.as_str()).unwrap_or(&none);
        let whole = match is_the_language(block, elsewhere) {
            Ok(()) => continue,
            Err(why) => why,
        };
        // Several blocks are a column of alternatives — five ways to write a
        // function literal, two spellings of one cast — so a block that is not
        // one thing may still be a list of things.
        let lines: Vec<&str> = block
            .lines()
            .map(without_comment)
            .filter(|l| !l.trim().is_empty())
            .collect();
        if lines.len() > 1 && lines.iter().all(|l| is_the_language(l, elsewhere).is_ok()) {
            continue;
        }
        failures.push(format!(
            "{file}: neither a program that compiles nor a fragment that \
             parses:\n{}\n{}",
            indent(&whole),
            indent(block)
        ));
    }

    assert!(
        failures.is_empty(),
        "{} grasp-dbsp block(s) in the docs are not the language:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The worked example in `docs/grasp-dbsp/language.md` compiles.
///
/// Pinned on its own as well as by the sweep above, which forgives a stream an
/// excerpt does not declare. This one is a whole program, so nothing about it
/// is an excerpt, and a missing declaration in it is a document teaching a
/// program that does not run.
#[test]
fn the_language_example_compiles() {
    let (_, block) = blocks()
        .into_iter()
        .find(|(_, b)| b.contains("input(\"emp\")") && b.contains("aggregate("))
        .expect("the worked example");
    if let Err(diags) = grasp_dbsp::compile(&block) {
        panic!(
            "the example in language.md does not compile: {}",
            grasp_dbsp::diag::render(&diags)
        );
    }
}

/// And the one in `docs/grasp/mapping.md`, which is what the compiler says it
/// emits: `input`, `map`, `map_index`, `join`, `plus`, `circuit` and
/// `fixpoint`, in one program.
#[test]
fn the_mapping_example_compiles() {
    let (_, block) = blocks()
        .into_iter()
        .find(|(_, b)| b.contains("circuit path_scc") && b.contains("fixpoint("))
        .expect("the worked example");
    if let Err(diags) = grasp_dbsp::compile(&block) {
        panic!(
            "the worked example in docs/grasp/mapping.md does not compile: {}",
            grasp_dbsp::diag::render(&diags)
        );
    }
}
