//! The reserved-word list is assembled from the operator, aggregator and
//! builtin names. A list that drifts from the real ones is worse than none, so
//! these tests pin the correspondence.

use dbsp_runner::expr::Builtin;
use dbsp_runner::lang::is_reserved;
use dbsp_runner::typecheck::{AGGREGATORS, OPERATORS};

/// Every name in `OPERATORS` must be one `check_op` actually knows.
///
/// A name is "known" when compiling a call to it fails with something other
/// than "unknown operator" — the arguments here are deliberately wrong, so
/// every case fails; what matters is *how*.
#[test]
fn every_listed_operator_is_recognised() {
    for op in OPERATORS {
        let src = format!("x := {op}()");
        let diags = dbsp_runner::compile(&src).expect_err("no operator accepts zero arguments");
        let text = diags.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ");
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
        assert!(!OPERATORS.contains(&name), "test needs a name that is not an operator");
        let diags = dbsp_runner::compile(&format!("x := {name}()")).expect_err("not an operator");
        assert!(
            diags.iter().any(|d| d.message.contains("unknown operator")),
            "`{name}` should be an unknown operator"
        );
    }
}

#[test]
fn every_operator_aggregator_and_builtin_is_reserved() {
    for name in OPERATORS.iter().chain(AGGREGATORS).chain(Builtin::ALL) {
        assert!(is_reserved(name), "`{name}` is a language name but is not reserved");
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
    for name in ["zset", "indexed_zset", "optional", "record", "sql", "ABSENT", "null", "fun", "if"]
    {
        assert!(is_reserved(name), "`{name}` should be reserved");
    }
    for name in ["emp", "dept", "by_dept", "row", "r", "k", "v", "total", "pos"] {
        assert!(!is_reserved(name), "`{name}` should not be reserved");
    }
}
