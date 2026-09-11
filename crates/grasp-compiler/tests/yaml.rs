//! The YAML fixture harness.
//!
//! Every file under `tests/cases/` holds a list of cases; each becomes one test
//! named `<path>::<index>_<name>` — the path being the file's place under
//! `tests/cases` without its extension, so `cargo test --test yaml syntax/`
//! runs a directory, `… operators` a file, and a failure names itself.
//!
//! The format is documented in `tests/cases/README.md`.
//!
//! Two things distinguish this from `grasp-dbsp-runner`'s otherwise identical
//! harness, and both come from grasp-compiler being a pipeline under
//! construction:
//!
//! - **Cases outrun the compiler.** A fixture asserting something only a later
//!   pass can produce is expected to fail, and is reported as *pending* rather
//!   than as a failure. The compiler is what says so, through
//!   `Diagnostic::unimplemented`; see [`first_unimplemented`].
//! - **What it emits must be executable.** Any case that reaches emission hands
//!   the text to `grasp_dbsp_runner::compile`, whatever it asserts, so a
//!   program the target rejects is caught without a fixture having to ask.

use common::{assert_every_directory_was_walked, fixture_files, label};
use grasp_compiler::diag::{Diagnostic, Pass};
use grasp_dbsp_runner::json::{decode_value, encode_value};
use grasp_dbsp_runner::lower::Runner;
use grasp_dbsp_runner::typecheck::Plan;
use grasp_dbsp_runner::value::{BatchType, TypeDesc};
use libtest_mimic::{Arguments, Failed, Trial};
use serde::Deserialize;
use serde_json::Value as J;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Pending cases, recorded as they are skipped over, for the burn-down line —
/// each the construct the compiler said it had not implemented.
/// A `Mutex` because `libtest-mimic` runs trials on several threads.
static PENDING: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

mod common;

fn main() {
    let args = Arguments::from_args();
    let trials = match collect_trials() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to load fixtures: {e}");
            std::process::exit(1);
        }
    };
    let conclusion = libtest_mimic::run(&args, trials);
    report_pending();
    conclusion.exit();
}

/// The debt, broken down by the pass each case is waiting for.
///
/// Printed rather than asserted: a pending case is not a failure, it is a test
/// written before the code. Making it visible after every run is what keeps it
/// from becoming a pile nobody looks at.
fn report_pending() {
    let pending = PENDING.lock().expect("pending");
    if pending.is_empty() {
        return;
    }
    // Grouped by what blocked them, which is a work queue rather than a census:
    // the largest number is the feature that would free the most fixtures.
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for reason in pending.iter() {
        assert!(
            grasp_compiler::diag::Diagnostic::UNIMPLEMENTED.contains(&reason.as_str())
                || grasp_compiler::core::DESIGNED.contains(&reason.as_str()),
            "the compiler reported `{reason}` as unimplemented, which is neither in \
             `Diagnostic::UNIMPLEMENTED` nor a function `core::DESIGNED` names — the \
             burn-down would count it as a row of its own rather than with the gap \
             it belongs to"
        );
        *counts.entry(reason.as_str()).or_default() += 1;
    }
    let mut parts: Vec<(usize, &str)> = counts.into_iter().map(|(r, n)| (n, r)).collect();
    parts.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    eprintln!(
        "\n{} pending: {}",
        pending.len(),
        parts
            .iter()
            .map(|(n, r)| format!("{r} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

// ---------------------------------------------------------------------------
// Fixture types
// ---------------------------------------------------------------------------

/// A row is `[weight, value]`. A grasp relation is always a flat
/// `zset(record(...))` — indexed streams never correspond to one — so unlike
/// the runner's fixtures there is no three-element indexed form.
type Row = serde_yaml::Value;
/// One transaction: relation name to its rows.
type Epoch = BTreeMap<String, Vec<Row>>;

/// `deny_unknown_fields` matters here: without it a misspelled key in a fixture
/// is silently ignored and the case quietly asserts nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    /// Required. At three hundred cases `errors::47` tells you nothing.
    name: String,
    source: String,
    #[serde(default)]
    input: Vec<Epoch>,

    // Exactly one of these six.
    #[serde(default)]
    expected_ok: Option<bool>,
    #[serde(default)]
    expected_diagnostics: Option<Vec<ExpectedDiagnostic>>,
    #[serde(default)]
    equivalent_to: Option<String>,
    #[serde(default)]
    expected_output: Option<Vec<Epoch>>,
    #[serde(default)]
    expected_exact_output: Option<Vec<Epoch>>,
    /// Relation and rule names to the types inference gave them. See
    /// [`check_types`] for the key grammar.
    #[serde(default)]
    expected_types: Option<BTreeMap<String, BTreeMap<String, String>>>,

    /// Only for constructs blocked on named future work; the reason must name a
    /// section of `docs/grasp/overview.md#future-work`. A case blocked merely on
    /// time is pending, not skipped — see `tests/cases/README.md`.
    #[serde(default)]
    skip: Option<String>,
}

/// Only the fields a fixture actually writes are checked, so new diagnostic
/// fields can be added without touching existing fixtures. `pass` is the
/// exception: it is required, because it also decides when the case goes live.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedDiagnostic {
    pass: String,
    severity: Option<String>,
    /// Substring match.
    message: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
}

impl ExpectedDiagnostic {
    fn pass(&self) -> Result<Pass, String> {
        Pass::from_name(&self.pass).ok_or_else(|| {
            format!(
                "unknown pass `{}`; expected one of {}",
                self.pass,
                Pass::ALL
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }

    fn matches(&self, d: &Diagnostic) -> bool {
        let pass_ok = self.pass == d.pass.as_str();
        let sev_ok = self
            .severity
            .as_ref()
            .is_none_or(|s| s == d.severity.as_str());
        let msg_ok = self
            .message
            .as_ref()
            .is_none_or(|m| d.message.contains(m.as_str()));
        let line_ok = self
            .line
            .is_none_or(|l| d.span.is_some_and(|s| s.line == l));
        let col_ok = self
            .column
            .is_none_or(|c| d.span.is_some_and(|s| s.column == c));
        pass_ok && sev_ok && msg_ok && line_ok && col_ok
    }

    fn describe(&self) -> String {
        let mut parts = vec![format!("pass={}", self.pass)];
        if let Some(v) = &self.severity {
            parts.push(format!("severity={v}"));
        }
        if let Some(v) = &self.message {
            parts.push(format!("message~{v:?}"));
        }
        if let Some(v) = self.line {
            parts.push(format!("line={v}"));
        }
        if let Some(v) = self.column {
            parts.push(format!("column={v}"));
        }
        parts.join(" ")
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases")
}

fn collect_trials() -> Result<Vec<Trial>, String> {
    let dir = cases_dir();
    let files = fixture_files(&dir)?;
    assert_every_directory_was_walked(&dir, &files);

    let mut trials = Vec::new();
    // The sources of the cases that actually run something, for the coverage
    // trial below.
    let mut executed: Vec<String> = Vec::new();
    for path in files {
        let stem = label(&dir, &path);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cases: Vec<Case> =
            serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

        for (i, case) in cases.into_iter().enumerate() {
            let name = format!("{stem}::{i}_{}", slug(&case.name));
            let where_ = format!("{}, case {i} ({})", path.display(), case.name);
            let skipped = case.skip.is_some();
            if !skipped && (case.expected_output.is_some() || case.expected_exact_output.is_some())
            {
                executed.push(case.source.clone());
            }
            trials.push(
                Trial::test(name.clone(), move || {
                    run_trial(&case, &name, &where_).map_err(Failed::from)
                })
                // A skipped case is not run at all: it is blocked on a language
                // feature, so its source may not even parse.
                .with_ignored_flag(skipped),
            );
        }
    }
    trials.push(Trial::test(
        "coverage::every_callable_is_exercised",
        move || every_callable_is_exercised(&executed).map_err(Failed::from),
    ));
    Ok(trials)
}

/// Every callable the compiler has is *run* by some case.
///
/// `reserved.rs` already holds the library to `stdlib.grasp`, so a function
/// without an entry fails there. This is the other half, and it is the half
/// that was missing: fourteen functions were implemented, declared, and never
/// called by a case that runs — so `float:ceil` emitting a name the target does
/// not have would have compiled, and nothing would have said so.
///
/// **Only a case that compiles counts.** A pending one never reaches the
/// builtin it names, so counting it would report coverage the run does not
/// have — which is the same mistake as counting a mention in a comment.
fn every_callable_is_exercised(sources: &[String]) -> Result<(), String> {
    use grasp_compiler::core::Builtin;

    let live: Vec<&String> = sources
        .iter()
        .filter(|s| grasp_compiler::compile(s).is_ok())
        .collect();

    let mut missing: Vec<&'static str> = Vec::new();
    for b in Builtin::ALL {
        // Written by desugaring and never by a program — `r.f`, `*r` and `**r`
        // are the syntax, and the fixtures for those cover them. A family's
        // members are not callable either: the head's name is what a program
        // writes, and it is checked here.
        if !b.callable() {
            continue;
        }
        let name = b.as_str();
        if !missing.contains(&name) && !live.iter().any(|s| s.contains(&format!("{name}("))) {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "not called by any case that runs: {}\n  \
         a function nothing calls is a function nothing checks — give it a case \
         with an `expected_exact_output`, beside its family in \
         `tests/cases/programs/`",
        missing.join(", ")
    ))
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

// ---------------------------------------------------------------------------
// Running a case
// ---------------------------------------------------------------------------

/// The construct a program hit that the compiler has not implemented, if any.
///
/// This is the whole of the pending mechanism. A case blocked by unwritten code
/// is pending; a case that fails any other way is a failure; and the compiler
/// is the authority on which is which, because it is the only thing that knows.
///
/// It replaces a high-water mark that had to be bumped by hand, and with it the
/// check that caught people forgetting: a case goes live here the moment the
/// compiler stops saying it cannot, so there is no marker to remove and nothing
/// to forget.
fn unimplemented_reason(source: &str) -> Option<String> {
    first_unimplemented(&grasp_compiler::compile(source))
}

/// What pending means, defined once: the first thing a result says the compiler
/// cannot do.
///
/// Which result is handed to it is the caller's question, and it is not the same
/// question for every mode. `grasp_compiler::compile` always ends in "the stages
/// after inference", so a mode that only needs the front half must ask
/// `grasp_compiler::check` instead — otherwise it is pending on a stage it never
/// wanted, forever, and passes while asserting nothing.
fn first_unimplemented<T>(result: &Result<T, Vec<Diagnostic>>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(diags) => diags.iter().find_map(|d| d.unimplemented.clone()),
    }
}

fn run_trial(case: &Case, _name: &str, where_: &str) -> Result<(), String> {
    check_shape(case, where_)?;

    // What the compiler has not got to yet, asked of the half of the pipeline
    // this mode reads. `equivalent_to` is a second program and blocks the case
    // just as its own source would.
    let blocked = if case.expected_types.is_some() {
        // This mode reads inference's output and stops there, so the stages
        // after it cannot block it — and must not, since they block everything.
        first_unimplemented(&grasp_compiler::check(&case.source))
    } else {
        std::iter::once(&case.source)
            .chain(case.equivalent_to.iter())
            .find_map(|source| unimplemented_reason(source))
    };

    if let Some(reason) = blocked {
        // Recorded whatever the mode does with it. Counting and asserting are
        // two different things, and a mode that tolerates a gap without
        // counting it hides the gap: `expected_ok` alone covered destructuring
        // for as long as destructuring did not exist, and the burn-down said
        // one feature was missing when three were.
        PENDING.lock().expect("pending").push(reason);

        // `expected_ok` claims only that nothing *rejects* the program, and an
        // unimplemented construct does not reject it — so that mode stays live
        // and runs anyway, which is what makes it the floor it is meant to be.
        // Every other mode needs the compiler to produce something in
        // particular, and cannot be judged until it can.
        if case.expected_ok.is_none() {
            return Ok(());
        }
    }
    run_case(case, where_)
}

/// A case asserts exactly one thing, and `input` belongs to the output modes.
fn check_shape(case: &Case, where_: &str) -> Result<(), String> {
    let modes = [
        ("expected_ok", case.expected_ok.is_some()),
        ("expected_diagnostics", case.expected_diagnostics.is_some()),
        ("equivalent_to", case.equivalent_to.is_some()),
        ("expected_output", case.expected_output.is_some()),
        (
            "expected_exact_output",
            case.expected_exact_output.is_some(),
        ),
        ("expected_types", case.expected_types.is_some()),
    ];
    let present: Vec<&str> = modes.iter().filter(|(_, p)| *p).map(|(n, _)| *n).collect();
    match present.len() {
        1 => {}
        0 => {
            return Err(format!(
                "{where_}: a case needs exactly one of {}",
                modes.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            ));
        }
        _ => {
            return Err(format!(
                "{where_}: a case may have only one assertion; found {}",
                present.join(" and ")
            ));
        }
    }
    // `pass` is validated here rather than where it is matched, so a typo in
    // it is "unknown pass `infr`" and not a silent failure to match anything.
    // Deciding when a case goes live was its other job and is now the
    // compiler's, but this one is why it is still required.
    if let Some(diags) = &case.expected_diagnostics {
        if diags.is_empty() {
            return Err(format!(
                "{where_}: `expected_diagnostics` is empty; a case that expects no \
                 diagnostic wants `expected_ok: true`"
            ));
        }
        for d in diags {
            d.pass().map_err(|e| format!("{where_}: {e}"))?;
        }
    }
    if case.expected_ok == Some(false) {
        return Err(format!(
            "{where_}: `expected_ok: false` asserts nothing in particular; \
             write the diagnostics you expect instead"
        ));
    }
    // `rel: {}` is a legitimate claim — a relation may have no columns — so the
    // emptiness that means nothing is the outer map.
    if let Some(types) = &case.expected_types {
        if types.is_empty() {
            return Err(format!(
                "{where_}: `expected_types` is empty; a case that asserts nothing \
                 about types wants `expected_ok: true`"
            ));
        }
        // Spelling is checked here, not at comparison time, because a case that
        // goes pending never reaches the comparison — and a typo'd expectation
        // sitting unnoticed inside one until its construct lands is exactly the
        // silence this mode was built to remove.
        for map in types.values() {
            for (name, ty) in map {
                if let Some(bad) = uncanonical(ty) {
                    return Err(format!(
                        "{where_}: `expected_types` gives `{name}` the type `{ty}`, which \
                         is not how a type is spelled: {bad}. This mode pins the spelling \
                         a diagnostic would quote, so it has to be exact"
                    ));
                }
            }
        }
    }
    let has_output = case.expected_output.is_some() || case.expected_exact_output.is_some();
    if !case.input.is_empty() && !has_output {
        return Err(format!(
            "{where_}: `input` is only meaningful with `expected_output` or \
             `expected_exact_output`"
        ));
    }
    Ok(())
}

fn run_case(case: &Case, where_: &str) -> Result<(), String> {
    if case.expected_ok.is_some() {
        return check_ok(case, where_);
    }
    if let Some(expected) = &case.expected_diagnostics {
        return check_diagnostics(case, expected, where_);
    }
    if let Some(other) = &case.equivalent_to {
        return check_equivalent(case, other, where_);
    }
    if let Some(types) = &case.expected_types {
        return check_types(case, types, where_);
    }
    let exact = case.expected_exact_output.is_some();
    let expected = case
        .expected_exact_output
        .as_ref()
        .or(case.expected_output.as_ref())
        .expect("checked by check_shape");
    check_output(case, expected, exact, where_)
}

/// `expected_types` — the types inference gave a relation's columns and a rule's
/// variables.
///
/// The only mode that can see inference's output. Every other one reads what the
/// compiler *emitted* or *rejected*, and both erase types: two programs that
/// differ only in the type a variable was given are the same program to them.
///
/// **Keys.** A bare name is a relation, and its map is column name to type.
/// `rel:N` is the Nth rule, counting from zero in source order, whose head names
/// `rel`, and its map is variable name to type. A key is split at its last `:`
/// when what follows is all digits, which is total: `lex.rs` takes a namespace
/// segment only when a letter follows the colon, so `rel:0` is not a name grasp
/// can spell and no relation can be called that.
///
/// **Partial.** A column or variable the case does not name is not checked, and
/// neither is a relation it does not name — a case may pin one variable of one
/// rule and say nothing else. Naming something that does not exist is still a
/// failure: leaving a key out is a choice, getting one wrong is a typo.
///
/// **Types are compared as strings**, against `ast::Type`'s `Display`, with no
/// normalisation. That pins the canonical spelling as well as the type, which is
/// the vocabulary every diagnostic quotes.
fn check_types(
    case: &Case,
    expected: &BTreeMap<String, BTreeMap<String, String>>,
    where_: &str,
) -> Result<(), String> {
    let typed = grasp_compiler::check(&case.source).map_err(|d| {
        format!(
            "{where_}: expected the program to typecheck, but:\n{}",
            indent(&render(&d))
        )
    })?;

    for (key, wanted) in expected {
        match split_rule_key(key) {
            Some((relation, index)) => {
                let rules: Vec<&grasp_compiler::infer::TypedRule> = typed
                    .decls
                    .iter()
                    .filter_map(|d| match d {
                        grasp_compiler::infer::Decl::Rule(r)
                            if r.rule.head.relation == relation =>
                        {
                            Some(r)
                        }
                        _ => None,
                    })
                    .collect();
                // A bad relation name must say so, rather than blaming the
                // ordinal it happens to carry.
                if !typed.relations.contains_key(relation) {
                    return Err(unknown_relation(&typed, relation, where_));
                }
                let Some(rule) = rules.get(index) else {
                    return Err(format!(
                        "{where_}: {}",
                        no_such_rule(&rules, relation, index)
                    ));
                };
                let at = format!("rule `{key}` (line {})", rule.rule.span.line);
                compare(&rule.vars, wanted, &at, "variable", "binds no", where_)?;
            }
            None => {
                let Some(relation) = typed.relations.get(key) else {
                    return Err(unknown_relation(&typed, key, where_));
                };
                let columns: BTreeMap<String, grasp_compiler::ast::Type> =
                    relation.columns.iter().cloned().collect();
                let at = format!("relation `{key}`");
                compare(&columns, wanted, &at, "column", "has no", where_)?;
            }
        }
    }
    Ok(())
}

/// What is wrong with how a type is written, spelled as the end of a sentence.
///
/// A partial check, and honestly so: it has no parser, so it catches the
/// separators and not the shape. `compare` catches everything else — including a
/// record whose fields are out of order — for any case that actually runs, and
/// this exists for the ones that do not.
fn uncanonical(ty: &str) -> Option<String> {
    let bytes = ty.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if (*b == b',' || *b == b':') && bytes.get(i + 1) != Some(&b' ') {
            return Some(format!(
                "a type writes a space after every `,` and `:`, and this one does not at \
                 column {}",
                i + 1
            ));
        }
        if *b == b' ' && bytes.get(i + 1) == Some(&b' ') {
            return Some(format!("it has two spaces at column {}", i + 1));
        }
    }
    if ty.trim() != ty {
        return Some("it has leading or trailing space".to_string());
    }
    None
}

/// Two type spellings side by side, with a caret under the first byte that
/// differs.
///
/// Only worth drawing when they nearly match: `i64` against `f64` needs no
/// diagram, but `dict(string, i64)` against `dict(string,i64)` is invisible
/// without one, and that is the mistake this mode invites.
fn near_miss(got: &str, want: &str) -> Option<String> {
    let at = got
        .bytes()
        .zip(want.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or(got.len().min(want.len()));
    if at < 2 {
        return None;
    }
    let squeeze = |s: &str| s.split_whitespace().collect::<String>();
    let note = if squeeze(got) == squeeze(want) {
        " — they differ only in spacing"
    } else {
        ""
    };
    Some(format!(
        "they differ at column {}{note}:\n{}",
        at + 1,
        indent(&format!(
            "inferred  {got}\nexpected  {want}\n          {}^",
            " ".repeat(at)
        ))
    ))
}

/// `rel:N` into `(rel, N)`, or `None` for a plain relation name.
///
/// Splits at the *last* colon so a qualified relation name keeps its namespace:
/// `a:b:0` is rule 0 of `a:b`.
fn split_rule_key(key: &str) -> Option<(&str, usize)> {
    let (relation, index) = key.rsplit_once(':')?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((relation, index.parse().ok()?))
}

/// One map of settled types against what the case named, reporting the first
/// disagreement — with the whole inferred map beneath it, because a table
/// assertion is not debuggable from a single key.
fn compare(
    actual: &BTreeMap<String, grasp_compiler::ast::Type>,
    wanted: &BTreeMap<String, String>,
    at: &str,
    // `column` or `variable`, and `has no` or `binds no` — a relation has
    // columns, a rule binds variables, and the message should say so.
    noun: &str,
    absent: &str,
    where_: &str,
) -> Result<(), String> {
    let inferred = || {
        let rows: Vec<String> = actual
            .iter()
            .map(|(n, t)| format!("{n}: {t}"))
            .collect::<Vec<_>>();
        let body = if rows.is_empty() {
            "(nothing)".to_string()
        } else {
            rows.join("\n")
        };
        format!("\n{}", indent(&format!("inferred:\n{}", indent(&body))))
    };
    for (name, want) in wanted {
        let Some(got) = actual.get(name) else {
            return Err(format!(
                "{where_}: {at} {absent} {noun} `{name}`{}",
                inferred()
            ));
        };
        let got = got.to_string();
        if &got != want {
            let marker = match near_miss(&got, want) {
                Some(m) => format!("\n{}", indent(&m)),
                None => String::new(),
            };
            return Err(format!(
                "{where_}: {at} {noun} `{name}` is `{got}`, expected `{want}`{marker}{}",
                inferred()
            ));
        }
    }
    Ok(())
}

fn unknown_relation(typed: &grasp_compiler::infer::Typed, name: &str, where_: &str) -> String {
    format!(
        "{where_}: `expected_types` names relation `{name}`, which the program \
         does not define; it defines: {}",
        typed
            .relations
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn no_such_rule(
    rules: &[&grasp_compiler::infer::TypedRule],
    relation: &str,
    index: usize,
) -> String {
    if rules.is_empty() {
        return format!(
            "`expected_types` names rule `{relation}:{index}`, but `{relation}` has no rules"
        );
    }
    let keys: Vec<String> = rules
        .iter()
        .enumerate()
        .map(|(i, r)| format!("`{relation}:{i}` (line {})", r.rule.span.line))
        .collect();
    format!(
        "`expected_types` names rule `{relation}:{index}`, but `{relation}` has {} rule{}: {}",
        rules.len(),
        if rules.len() == 1 { "" } else { "s" },
        keys.join(", ")
    )
}

/// Compile, and hold the result to the always-on invariant: whatever grasp-dbsp
/// this produced, `grasp-dbsp-runner` must accept it. That catches the whole
/// class of "emitted something the target rejects" without any fixture asking.
fn emit(source: &str, what: &str, where_: &str) -> Result<(String, Plan), String> {
    let text = grasp_compiler::compile(source).map_err(|d| {
        format!(
            "{where_}: compiling {what} failed:\n{}",
            indent(&render(&d))
        )
    })?;
    let plan = grasp_dbsp_runner::compile(&text).map_err(|d| {
        format!(
            "{where_}: the emitted grasp-dbsp was rejected by grasp-dbsp-runner:\n{}\n  \
             emitted:\n{}",
            indent(&grasp_dbsp_runner::diag::render(&d)),
            indent(&capped(&text)),
        )
    })?;
    Ok((text, plan))
}

/// `expected_ok` — the program is accepted by every stage that exists.
///
/// Diagnostics from a pass the pipeline has not reached are tolerated, which is
/// what lets this mode be written before the pipeline is finished. As stages
/// land the same fixture silently demands more, and once emission works it also
/// carries the runner-accepts invariant.
fn check_ok(case: &Case, where_: &str) -> Result<(), String> {
    match grasp_compiler::compile(&case.source) {
        Ok(text) => {
            grasp_dbsp_runner::compile(&text).map_err(|d| {
                format!(
                    "{where_}: the emitted grasp-dbsp was rejected by \
                     grasp-dbsp-runner:\n{}\n  emitted:\n{}",
                    indent(&grasp_dbsp_runner::diag::render(&d)),
                    indent(&capped(&text)),
                )
            })?;
            Ok(())
        }
        Err(diags) => {
            let rejections: Vec<&Diagnostic> =
                diags.iter().filter(|d| d.unimplemented.is_none()).collect();
            if rejections.is_empty() {
                // Only what the compiler has not got to yet, which is not a
                // rejection and not this mode's business.
                return Ok(());
            }
            Err(format!(
                "{where_}: expected the program to be accepted, but:\n{}",
                indent(
                    &rejections
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            ))
        }
    }
}

fn check_diagnostics(
    case: &Case,
    expected: &[ExpectedDiagnostic],
    where_: &str,
) -> Result<(), String> {
    let actual = match grasp_compiler::compile(&case.source) {
        Ok(text) => {
            return Err(format!(
                "{where_}: expected compilation to fail, but it succeeded:\n{}",
                indent(&capped(&text))
            ));
        }
        Err(diags) => diags,
    };

    // Exact set, order-independent: pair each expectation with a distinct
    // diagnostic, then require nothing is left over on either side. A spurious
    // extra error is itself a bug.
    let mut unmatched: Vec<&Diagnostic> = actual.iter().collect();
    for want in expected {
        match unmatched.iter().position(|d| want.matches(d)) {
            Some(i) => {
                unmatched.remove(i);
            }
            None => {
                return Err(format!(
                    "{where_}: no diagnostic matched [{}]\n  actual diagnostics:\n{}",
                    want.describe(),
                    indent(&render(&actual))
                ));
            }
        }
    }
    if !unmatched.is_empty() {
        return Err(format!(
            "{where_}: {} unexpected diagnostic(s):\n{}",
            unmatched.len(),
            indent(
                &unmatched
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        ));
    }
    Ok(())
}

/// `equivalent_to` — two grasp programs must emit byte-identical grasp-dbsp.
///
/// Byte equality is the assertion, not a weakening of one: `compilation.md`
/// requires the optimizer to be deterministic and `syntax.md` requires that
/// nothing downstream keys on source position, so two programs that mean the
/// same thing must emit the same text. One mode, and it holds both claims.
fn check_equivalent(case: &Case, other: &str, where_: &str) -> Result<(), String> {
    let (left, _) = emit(&case.source, "`source`", where_)?;
    let (right, _) = emit(other, "`equivalent_to`", where_)?;
    if left == right {
        return Ok(());
    }
    Err(format!(
        "{where_}: the two programs emit different grasp-dbsp\n  {}\n  \
         from `source`:\n{}\n  from `equivalent_to`:\n{}",
        first_difference(&left, &right),
        indent(&capped(&left)),
        indent(&capped(&right)),
    ))
}

fn check_output(case: &Case, expected: &[Epoch], exact: bool, where_: &str) -> Result<(), String> {
    if expected.len() != case.input.len() {
        return Err(format!(
            "{where_}: {} input epoch(s) but {} expected epoch(s); \
             use `{{}}` for an epoch that produces nothing",
            case.input.len(),
            expected.len()
        ));
    }

    let (text, plan) = emit(&case.source, "`source`", where_)?;
    // Every failure past this point is undebuggable without the emitted
    // program, so it goes into each message below.
    let emitted = || indent(&capped(&text));

    // Outputs are whatever the expectations mention, keyed by grasp relation
    // name — `mapping.md` names the emitted node after the relation.
    let outputs: Vec<String> = expected
        .iter()
        .flat_map(|e| e.keys().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    // Only a named relation is observed, so a case whose every epoch is `{}`
    // observes nothing and asserts nothing — which is how a "derives no row"
    // fixture passed against a program that derived a row of nulls. A case
    // says which relation it is about, and writes the empty epoch beside one
    // that has rows.
    if outputs.is_empty() {
        return Err(format!(
            "{where_}: no epoch names a relation, so this case observes nothing and \
             asserts nothing; name the relation it is about — a relation expected to \
             derive nothing is written with an empty row list (`out: []`)"
        ));
    }
    // In subset mode an empty list is vacuous — every row of none is present in
    // anything — so the claim has to be written as `expected_exact_output`.
    if !exact
        && let Some(name) = expected
            .iter()
            .flat_map(|e| e.iter())
            .find_map(|(n, rows)| rows.is_empty().then_some(n))
    {
        return Err(format!(
            "{where_}: `{name}` is expected to be empty, which `expected_output` \
             cannot assert — a subset of no rows is any output at all; write \
             `expected_exact_output`"
        ));
    }

    let mut runner = Runner::build(&plan, &outputs).map_err(|d| {
        format!(
            "{where_}: building the circuit failed:\n{}\n  emitted:\n{}",
            indent(&grasp_dbsp_runner::diag::render(&d)),
            emitted(),
        )
    })?;

    for (epoch_idx, (inputs, want)) in case.input.iter().zip(expected).enumerate() {
        for (relation, rows) in inputs {
            // An external grasp relation emits as `r := input("r")`, so the
            // table name is the relation name.
            let node_idx = plan
                .inputs()
                .into_iter()
                .find(|(_, t)| *t == relation.as_str())
                .map(|(i, _)| i)
                .ok_or_else(|| {
                    format!(
                        "{where_}: the emitted program has no input table `{relation}`\n  \
                         emitted:\n{}",
                        emitted()
                    )
                })?;
            let row_type = match &plan.nodes[node_idx].ty {
                BatchType::ZSet(t) => t.clone(),
                BatchType::IndexedZSet(..) => {
                    return Err(format!(
                        "{where_}: input `{relation}` emitted as an indexed stream; \
                         a grasp relation is always a zset\n  emitted:\n{}",
                        emitted()
                    ));
                }
            };
            for row in rows {
                let (value, weight) = decode_input_row(row, &row_type).map_err(|e| {
                    format!("{where_}, epoch {epoch_idx}, relation `{relation}`: {e}")
                })?;
                runner
                    .push(relation, value, weight)
                    .map_err(|e| format!("{where_}: {e}\n  emitted:\n{}", emitted()))?;
            }
        }

        let produced = runner
            .step()
            .map_err(|e| format!("{where_}: {e}\n  emitted:\n{}", emitted()))?;

        for (name, deltas) in &produced {
            let ty = &plan.node(name).expect("output exists").ty;
            let BatchType::ZSet(row_type) = ty else {
                return Err(format!(
                    "{where_}: output `{name}` emitted as an indexed stream; \
                     a grasp relation is always a zset\n  emitted:\n{}",
                    emitted()
                ));
            };
            let mut actual: Vec<J> = deltas
                .iter()
                .map(|d| {
                    let row = encode_value(&d.key, row_type).map_err(|e| e.to_string())?;
                    Ok(canon(&J::Array(vec![J::from(d.weight), row])))
                })
                .collect::<Result<_, String>>()
                .map_err(|e: String| {
                    format!(
                        "{where_}: encoding output `{name}`: {e}\n  emitted:\n{}",
                        emitted()
                    )
                })?;

            let mut wanted: Vec<J> = want
                .get(name)
                .map(|rows| rows.iter().map(|r| canon(&yaml_to_json(r))).collect())
                .unwrap_or_default();

            actual.sort_by_key(|v| v.to_string());
            wanted.sort_by_key(|v| v.to_string());

            let ok = if exact {
                actual == wanted
            } else {
                wanted.iter().all(|w| actual.contains(w))
            };
            if !ok {
                return Err(format!(
                    "{where_}, epoch {epoch_idx}, relation `{name}`: {} mismatch\n  \
                     expected:\n{}\n  actual:\n{}\n  emitted:\n{}",
                    if exact { "exact" } else { "subset" },
                    indent(&rows_to_string(&wanted)),
                    indent(&rows_to_string(&actual)),
                    emitted(),
                ));
            }
        }

        // A tripwire rather than a check a fixture can trip: `step` yields an
        // entry for every selected output, empty deltas included, so a named
        // relation is always compared above — including against the empty list
        // that says it derives nothing. If that contract ever changed, the
        // comparison would silently skip the relation instead, and every
        // expectation about it would become vacuous.
        for name in want.keys() {
            if !produced.iter().any(|(n, _)| n == name) {
                return Err(format!(
                    "{where_}, epoch {epoch_idx}: relation `{name}` was selected as an \
                     output but not reported, so nothing about it was checked\n  \
                     emitted:\n{}",
                    emitted()
                ));
            }
        }
    }
    Ok(())
}

/// `[weight, value]` for a grasp relation, which is always flat.
fn decode_input_row(
    row: &Row,
    ty: &TypeDesc,
) -> Result<(grasp_dbsp_runner::value::DynValue, i64), String> {
    let row = row
        .as_sequence()
        .ok_or_else(|| "an input row is [weight, value]".to_string())?;
    if row.len() != 2 {
        return Err(format!(
            "an input row is [weight, value]; found {} element(s)",
            row.len()
        ));
    }
    let weight = yaml_to_json(&row[0])
        .as_i64()
        .ok_or_else(|| "the first element of a row must be an integer weight".to_string())?;
    let value = decode_value(&yaml_to_json(&row[1]), ty).map_err(|e| e.to_string())?;
    Ok((value, weight))
}

// ---------------------------------------------------------------------------
// YAML/JSON plumbing
// ---------------------------------------------------------------------------

fn yaml_to_json(y: &serde_yaml::Value) -> J {
    match y {
        serde_yaml::Value::Null => J::Null,
        serde_yaml::Value::Bool(b) => J::Bool(*b),
        serde_yaml::Value::Number(n) => n
            .as_i64()
            .map(J::from)
            .or_else(|| {
                n.as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map(J::Number)
            })
            .unwrap_or(J::Null),
        serde_yaml::Value::String(s) => J::String(s.clone()),
        serde_yaml::Value::Sequence(items) => J::Array(items.iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(map) => J::Object(
            map.iter()
                .map(|(k, v)| {
                    let key = k
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{k:?}"));
                    (key, yaml_to_json(v))
                })
                .collect(),
        ),
        serde_yaml::Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

/// Normalises numbers so an integer-valued float compares equal to the integer.
/// A fixture writing `salary: 100` for an `f64` column should not fail against
/// an encoded `100.0`.
fn canon(v: &J) -> J {
    match v {
        J::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9e15 => J::from(f as i64),
            _ => v.clone(),
        },
        J::Array(items) => J::Array(items.iter().map(canon).collect()),
        J::Object(map) => J::Object(map.iter().map(|(k, x)| (k.clone(), canon(x))).collect()),
        _ => v.clone(),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Emitted programs go into failure messages, and a big one would bury the
/// message it is attached to.
const MAX_EMITTED_LINES: usize = 120;

fn capped(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_EMITTED_LINES {
        return text.to_string();
    }
    format!(
        "{}\n… {} more line(s)",
        lines[..MAX_EMITTED_LINES].join("\n"),
        lines.len() - MAX_EMITTED_LINES
    )
}

/// Points at the first line where two emitted programs part company, so a
/// whole-program diff does not have to be read by eye.
fn first_difference(left: &str, right: &str) -> String {
    let (mut l, mut r) = (left.lines(), right.lines());
    let mut n = 1;
    loop {
        match (l.next(), r.next()) {
            (Some(a), Some(b)) if a == b => n += 1,
            (Some(a), Some(b)) => {
                return format!(
                    "first differing line {n}:\n    source:        {a}\n    equivalent_to: {b}"
                );
            }
            (Some(a), None) => return format!("`equivalent_to` ends at line {n}; source has: {a}"),
            (None, Some(b)) => return format!("`source` ends at line {n}; equivalent_to has: {b}"),
            (None, None) => return "the texts are equal (a comparison bug)".to_string(),
        }
    }
}

fn rows_to_string(rows: &[J]) -> String {
    if rows.is_empty() {
        return "(nothing)".to_string();
    }
    rows.iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(diags: &[Diagnostic]) -> String {
    if diags.is_empty() {
        return "(none)".to_string();
    }
    grasp_compiler::diag::render(diags)
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
