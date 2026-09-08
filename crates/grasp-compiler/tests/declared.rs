//! Every relation a fixture mentions is declared in that fixture.
//!
//! `docs/grasp/inference.md` already requires this — ``relation `r` is not
//! defined and has no typespec`` is in its Phase 2 diagnostics table — but
//! `infer` does not exist yet, so nothing enforced it and 124 of 185 cases
//! drifted into violating it. They passed because `check_ok` tolerates
//! diagnostics from passes the pipeline has not reached, and they would all
//! have broken in one step the day `IMPLEMENTED` became `Infer`.
//!
//! **This test is scaffolding with an end.** When `infer` implements the check
//! and `IMPLEMENTED` reaches it, every `expected_ok` case enforces the rule on
//! its own and this file is redundant — delete it then rather than maintaining
//! two copies of one rule.

use grasp_compiler::ast::{Arg, Decl, Expr, Lit, Stmt};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    source: String,
    #[serde(default)]
    equivalent_to: Option<String>,
    #[serde(default)]
    expected_diagnostics: Option<Vec<Diag>>,
    #[serde(default)]
    skip: Option<String>,
    // Everything else in a case is an assertion this test does not read.
    #[serde(flatten)]
    _rest: serde_yaml::Value,
}

#[derive(Debug, Deserialize)]
struct Diag {
    pass: String,
    #[serde(default)]
    message: Option<String>,
}

/// A case asserting one of these exists precisely to break a rule this file
/// checks, so it is not evidence of drift. Matching on the message rather than
/// on a marker key keeps the exemption tied to what the case actually claims.
const ASSERTS_A_RULE: &[&str] = &[
    "is not defined and has no typespec",
    "has no non-recursive rule and no typespec",
    "has no type here",
];

impl Case {
    /// A case whose only assertion is a parse rejection never reaches
    /// inference — `compile` is `parse(source)?` and short-circuits — so the
    /// rule cannot apply to it, and its source is usually malformed anyway.
    fn exempt(&self) -> bool {
        if self.skip.is_some() {
            return true;
        }
        let Some(ds) = &self.expected_diagnostics else {
            return false;
        };
        if ds.iter().any(|d| {
            d.message
                .as_deref()
                .is_some_and(|m| ASSERTS_A_RULE.iter().any(|r| m.contains(r)))
        }) {
            return true;
        }
        !ds.is_empty() && ds.iter().all(|d| d.pass == "parse")
    }
}

/// The relations a program declares, and the ones its rule bodies mention.
fn declared_and_mentioned(source: &str) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    let program = grasp_compiler::parse::parse(source).map_err(|d| d.to_string())?;
    let mut declared = BTreeSet::new();
    let mut mentioned = BTreeSet::new();
    for decl in &program {
        match decl {
            // A spec, a fact and a rule head are each a declaration.
            Decl::Spec(s) => {
                declared.insert(s.relation.clone());
            }
            Decl::Fact(f) => {
                declared.insert(f.relation.clone());
            }
            Decl::Rule(r) => {
                declared.insert(r.head.relation.clone());
                for stmt in &r.body {
                    // Negated atoms count: an undefined relation under `not`
                    // makes a rule *more* permissive, so it fails open.
                    if let Stmt::Atom { relation, .. } = stmt {
                        mentioned.insert(relation.clone());
                    }
                }
            }
        }
    }
    Ok((declared, mentioned))
}

/// Relations defined only by facts, whose arguments carry a literal that has no
/// type of its own, and which have no typespec.
///
/// A fact's arguments are closed, so an untyped literal in one can only take a
/// type from the relation's spec or from another rule for the same relation. A
/// relation with neither is reported by inference's phase 3 — the same rule
/// that makes `n := 6 * 7` need a declared head.
fn fact_only_untyped(source: &str) -> Vec<String> {
    let Ok(program) = grasp_compiler::parse::parse(source) else {
        return Vec::new();
    };
    let mut specced = BTreeSet::new();
    let mut ruled = BTreeSet::new();
    let mut facts: Vec<(&str, bool)> = Vec::new();
    for decl in &program {
        match decl {
            Decl::Spec(s) => {
                specced.insert(s.relation.as_str());
            }
            Decl::Rule(r) => {
                ruled.insert(r.head.relation.as_str());
            }
            Decl::Fact(f) => {
                let untyped = f.args.iter().any(|a| match &a.value {
                    // A bare integer and `NONE` are the forms with no type of
                    // their own. A string, a float or a boolean carries one.
                    Arg::Expr(Expr::Lit { value, .. }) => {
                        matches!(value, Lit::Int(_) | Lit::None)
                    }
                    _ => false,
                });
                facts.push((f.relation.as_str(), untyped));
            }
        }
    }
    let mut out = BTreeSet::new();
    for (relation, untyped) in facts {
        if untyped && !specced.contains(relation) && !ruled.contains(relation) {
            out.insert(relation.to_string());
        }
    }
    out.into_iter().collect()
}

#[test]
fn a_fact_with_an_untyped_literal_has_somewhere_to_take_a_type_from() {
    let mut problems = Vec::new();
    for (stem, case) in all_cases() {
        if case.exempt() {
            continue;
        }
        for source in std::iter::once(&case.source).chain(case.equivalent_to.iter()) {
            for relation in fact_only_untyped(source) {
                problems.push(format!(
                    "{stem}::{}: `{relation}` is defined only by facts whose literals \
                     have no type of their own, and has no typespec",
                    case.name
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{} fixture program(s) leave a fact's literal with no type to take.\n\
         `docs/grasp/inference.md` reports this in phase 3.\n\n{}",
        problems.len(),
        problems.join("\n")
    );
}

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases")
}

/// Every case in every fixture file, with the file it came from.
fn all_cases() -> Vec<(String, Case)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(cases_dir())
        .expect("tests/cases")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(path).expect("a fixture file");
        let cases: Vec<Case> =
            serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        out.extend(cases.into_iter().map(|c| (stem.clone(), c)));
    }
    out
}

#[test]
fn every_relation_a_fixture_mentions_is_declared() {
    let mut problems = Vec::new();
    let mut checked = 0usize;
    for (stem, case) in all_cases() {
        if case.exempt() {
            continue;
        }
        // `equivalent_to` is a second program, held to the same rule.
        for source in std::iter::once(&case.source).chain(case.equivalent_to.iter()) {
            checked += 1;
            let (declared, mentioned) = match declared_and_mentioned(source) {
                Ok(v) => v,
                // A parse failure is the yaml harness's business, not this
                // test's — it reports it far better.
                Err(_) => continue,
            };
            let missing: Vec<&String> = mentioned.difference(&declared).collect();
            if !missing.is_empty() {
                problems.push(format!(
                    "{stem}::{}: {} has neither a typespec nor a definition",
                    case.name,
                    missing
                        .iter()
                        .map(|r| format!("`{r}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    assert!(
        checked > 100,
        "only {checked} programs checked; the discovery is probably broken"
    );
    assert!(
        problems.is_empty(),
        "{} fixture program(s) mention a relation that is never declared.\n\
         `docs/grasp/inference.md` reports this as \
         \"relation `r` is not defined and has no typespec\".\n\n{}",
        problems.len(),
        problems.join("\n")
    );
}
