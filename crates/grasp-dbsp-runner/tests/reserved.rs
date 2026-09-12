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

/// Every operator, aggregator and builtin is exercised by a fixture that runs.
///
/// The design documents are the specification — they are what a compiler
/// frontend or an emitting agent is given — so a rule stated there and not
/// pinned by a case is a rule free to drift out of agreement with the code.
/// This is the coverage half of that: it does not check that a fixture asserts
/// the *right* thing, but it does stop a construct from being documented and
/// then never run.
///
/// **The corpus is the cases, not the files.** Searching the text would count a
/// name that appears only in a comment, or only in a case that is expected not
/// to compile — coverage claimed by a line that never executes. So the YAML is
/// parsed, only cases asserting output are kept, and only those that compile.
#[test]
fn every_construct_appears_in_a_fixture() {
    #[derive(serde::Deserialize)]
    struct Case {
        source: String,
        #[serde(default)]
        expected_output: Option<serde_yaml::Value>,
        #[serde(default)]
        expected_exact_output: Option<serde_yaml::Value>,
    }

    let mut corpus = String::new();
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    for entry in std::fs::read_dir(&cases).expect("tests/cases") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "yaml") {
            let text = std::fs::read_to_string(&path).expect("read");
            let parsed: Vec<Case> =
                serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for case in parsed {
                let runs = case.expected_output.is_some() || case.expected_exact_output.is_some();
                if runs && grasp_dbsp_runner::compile(&case.source).is_ok() {
                    corpus.push_str(&case.source);
                    corpus.push('\n');
                }
            }
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
    for other in [
        "cast",
        "record",
        "array",
        "dict",
        "map_array",
        "filter_array",
    ] {
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

/// `language.md`'s "Reserved words" paragraph, pinned to the code — the way the
/// compiler's `tests/reserved.rs` pins `syntax.md`. It lists the type
/// constructors and keywords by name and the operators, aggregators and
/// builtins by count, so those are what is checked: every name it lists is
/// reserved, every type name and keyword the code reserves is listed, and the
/// counts are the real ones. Six reserved type names were once missing from
/// the list, and nothing noticed.
#[test]
fn the_reserved_words_match_language_md() {
    use grasp_dbsp_runner::lang::{KEYWORDS, TYPE_NAMES};
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/grasp-dbsp/language.md"),
    )
    .expect("language.md");
    let start = text.find("## Reserved words").expect("the section");
    let body = &text[start..];
    let body = &body[body.find('\n').unwrap() + 1..];
    let paragraph = body.trim_start().split("\n\n").next().expect("a paragraph");

    let listed: Vec<&str> = paragraph
        .split('`')
        .skip(1)
        .step_by(2)
        .collect();
    for name in &listed {
        assert!(is_reserved(name), "`{name}` is listed as reserved in language.md but is not");
    }
    for name in TYPE_NAMES.iter().chain(KEYWORDS) {
        assert!(
            listed.contains(name),
            "`{name}` is reserved but language.md's list does not name it"
        );
    }
    let count = |what: &str| -> usize {
        let i = paragraph.find(what).unwrap_or_else(|| panic!("`{what}` in the paragraph"));
        let before = paragraph[..i].trim_end();
        before
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("a count before `{what}`"))
    };
    assert_eq!(count(" operator names"), OPERATORS.len(), "operator count in language.md");
    assert_eq!(count(" aggregator names"), AGGREGATORS.len(), "aggregator count in language.md");
}
