//! The reserved-word list is assembled from the operator, aggregator and
//! builtin names. A list that drifts from the real ones is worse than none, so
//! these tests pin the correspondence.

use grasp_dbsp_runner::expr::Builtin;
use grasp_dbsp_runner::lang::is_reserved;
use grasp_dbsp_runner::typecheck::{AGGREGATORS, OPERATORS};

/// Every name in `OPERATORS` must be one `check_op` actually knows.
///
/// A name is "known" when compiling a call to it fails with something other
/// than "unknown operator" — the arguments here are deliberately wrong, so
/// every case fails; what matters is *how*.
#[test]
fn every_listed_operator_is_recognised() {
    for op in OPERATORS {
        let src = format!("x := {op}()");
        let diags =
            grasp_dbsp_runner::compile(&src).expect_err("no operator accepts zero arguments");
        let text = diags
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        assert!(
            !text.contains("unknown operator"),
            "`{op}` is in OPERATORS but check_op does not know it: {text}"
        );
    }
}

/// And the converse: a name that is not listed must be rejected as unknown, so
/// an operator added to `check_op` without being listed is caught.
#[test]
fn unlisted_names_are_unknown_operators() {
    for name in ["frobnicate", "zip", "explode"] {
        assert!(
            !OPERATORS.contains(&name),
            "test needs a name that is not an operator"
        );
        let diags =
            grasp_dbsp_runner::compile(&format!("x := {name}()")).expect_err("not an operator");
        assert!(
            diags.iter().any(|d| d.message.contains("unknown operator")),
            "`{name}` should be an unknown operator"
        );
    }
}

#[test]
fn every_operator_aggregator_and_builtin_is_reserved() {
    for name in OPERATORS.iter().chain(AGGREGATORS).chain(Builtin::ALL) {
        assert!(
            is_reserved(name),
            "`{name}` is a language name but is not reserved"
        );
    }
}

#[test]
fn every_builtin_name_resolves() {
    for name in Builtin::ALL {
        assert!(
            Builtin::from_name(name).is_some(),
            "`{name}` is in Builtin::ALL but from_name does not know it"
        );
    }
}

/// Type constructors and keywords are reserved too, and ordinary identifiers
/// are not — the list should not have grown to swallow everything.
#[test]
fn the_reserved_set_has_the_right_shape() {
    for name in [
        "zset",
        "indexed_zset",
        "optional",
        "record",
        "array",
        "string",
        "sql",
        "NONE",
        "null",
        "function",
        "return",
        "if",
        "cast",
    ] {
        assert!(is_reserved(name), "`{name}` should be reserved");
    }
    for name in [
        "emp", "dept", "by_dept", "row", "r", "k", "v", "total", "pos",
    ] {
        assert!(!is_reserved(name), "`{name}` should not be reserved");
    }
}

/// Every operator, aggregator and builtin is exercised by at least one fixture.
///
/// The design documents are the specification — they are what a compiler
/// frontend or an emitting agent is given — so a rule stated there and not
/// pinned by a case is a rule free to drift out of agreement with the code.
/// This is the coverage half of that: it does not check that a fixture asserts
/// the *right* thing, but it does stop a construct from being documented and
/// then never run.
#[test]
fn every_construct_appears_in_a_fixture() {
    let mut corpus = String::new();
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    for entry in std::fs::read_dir(&cases).expect("tests/cases") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "yaml") {
            corpus.push_str(&std::fs::read_to_string(&path).expect("read"));
        }
    }
    assert!(!corpus.is_empty(), "no fixtures found");

    let mut missing: Vec<String> = Vec::new();
    for op in OPERATORS {
        // Operators are called, so look for the name followed by `(`.
        if !corpus.contains(&format!("{op}(")) {
            missing.push(format!("operator `{op}`"));
        }
    }
    for agg in AGGREGATORS {
        // Aggregators appear as a bare second argument to `aggregate`.
        if !corpus.contains(&format!(", {agg},")) {
            missing.push(format!("aggregator `{agg}`"));
        }
    }
    for b in Builtin::ALL {
        if !corpus.contains(&format!("{b}(")) {
            missing.push(format!("builtin `{b}`"));
        }
    }
    // `cast` is not a `Builtin` — its second argument is a type, not an
    // expression — so it has to be named here rather than coming from a list.
    for other in ["cast", "record", "array", "dict", "select"] {
        if !corpus.contains(&format!("{other}(")) {
            missing.push(format!("`{other}`"));
        }
    }
    assert!(
        missing.is_empty(),
        "not exercised by any fixture: {}",
        missing.join(", ")
    );
}

/// The worked example in `language.md` compiles.
///
/// The documents are the specification an emitter is given, so an example that
/// does not compile teaches a program that does not compile. This extracts the
/// one whole program in `language.md` rather than duplicating it, so the two
/// cannot drift.
#[test]
fn the_language_example_compiles() {
    // The design documents live at the workspace root, not in this crate: the
    // language is the contract between the runner and the compiler frontend.
    let doc =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/grasp-dbsp/language.md");
    let doc = std::fs::read_to_string(&doc).expect("language.md");
    // Splitting on the fence gives prose at even indices and code at odd ones;
    // the prose around the example mentions the same names, so parity is what
    // distinguishes them.
    let block = doc
        .split("```")
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, b)| b)
        .find(|b| b.contains("input(\"emp\")") && b.contains("aggregate("))
        .expect("the worked example");
    if let Err(diags) = grasp_dbsp_runner::compile(block.trim_start_matches('\n')) {
        panic!(
            "the example in language.md does not compile: {}",
            diags
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}
