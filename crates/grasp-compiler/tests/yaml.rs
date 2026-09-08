//! The YAML fixture harness.
//!
//! Every file under `tests/cases/` holds a list of cases; each becomes one test
//! named `<file stem>::<index>_<name>`, so `cargo test --test yaml operators`
//! filters and a failure names itself.
//!
//! The format is documented in `tests/cases/README.md`.
//!
//! Two things distinguish this from `grasp-dbsp-runner`'s otherwise identical
//! harness, and both come from grasp-compiler being a pipeline under
//! construction:
//!
//! - **Cases outrun the compiler.** A fixture asserting something only a later
//!   pass can produce is expected to fail, and is reported as *pending* rather
//!   than as a failure. See [`required_pass`] and the block in [`run_trial`].
//! - **What it emits must be executable.** Any case that reaches emission hands
//!   the text to `grasp_dbsp_runner::compile`, whatever it asserts, so a
//!   program the target rejects is caught without a fixture having to ask.

use grasp_compiler::IMPLEMENTED;
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

/// Pending cases, recorded as they are skipped over, for the burn-down line.
/// A `Mutex` because `libtest-mimic` runs trials on several threads.
static PENDING: LazyLock<Mutex<Vec<Pass>>> = LazyLock::new(|| Mutex::new(Vec::new()));

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
    let parts: Vec<String> = Pass::ALL
        .iter()
        .map(|p| (p, pending.iter().filter(|q| *q == p).count()))
        .filter(|(_, n)| *n > 0)
        .map(|(p, n)| format!("{p} {n}"))
        .collect();
    eprintln!(
        "\n{} pending (the pipeline reaches `{IMPLEMENTED}`): {}",
        pending.len(),
        parts.join(", ")
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

    // Exactly one of these five.
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
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
        .collect();
    files.sort();

    let mut trials = Vec::new();
    for path in files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cases: Vec<Case> =
            serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

        for (i, case) in cases.into_iter().enumerate() {
            let name = format!("{stem}::{i}_{}", slug(&case.name));
            let where_ = format!("{}, case {i} ({})", path.display(), case.name);
            let skipped = case.skip.is_some();
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
    Ok(trials)
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

/// The pass a case's assertion needs the pipeline to have reached.
///
/// Derived from what the case asserts rather than annotated, so there is no
/// bookkeeping to forget. This is the other half of why `pass` is required on
/// an expected diagnostic.
fn required_pass(case: &Case) -> Result<Pass, String> {
    if let Some(diags) = &case.expected_diagnostics {
        if diags.is_empty() {
            return Err("`expected_diagnostics` is empty; a case that expects no \
                        diagnostic wants `expected_ok: true`"
                .to_string());
        }
        let mut max = Pass::Parse;
        for d in diags {
            max = max.max(d.pass()?);
        }
        return Ok(max);
    }
    if case.expected_ok.is_some() {
        // The floor: "this program is legal", asserted against whatever the
        // pipeline currently checks. It demands more as stages land.
        return Ok(Pass::Parse);
    }
    // `equivalent_to` compares emitted text, and the output modes execute it.
    Ok(Pass::Emit)
}

fn run_trial(case: &Case, name: &str, where_: &str) -> Result<(), String> {
    check_shape(case, where_)?;
    let required = required_pass(case).map_err(|e| format!("{where_}: {e}"))?;

    if required > IMPLEMENTED {
        return match run_case(case, where_) {
            // Expected: the pipeline does not reach this pass yet.
            Err(_) => {
                PENDING.lock().expect("pending").push(required);
                Ok(())
            }
            // Unexpected. Either the stage landed and nobody bumped
            // `IMPLEMENTED`, or the case asserts less than it claims to — a
            // fixture that would otherwise sit here passing and proving
            // nothing.
            Ok(()) => Err(format!(
                "{name} passes today, but needs pass `{required}` and the \
                 pipeline reaches `{IMPLEMENTED}`.\n  \
                 Bump `IMPLEMENTED` in src/lib.rs if the stage landed, or fix \
                 the case if it asserts less than it should."
            )),
        };
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
    if case.expected_ok == Some(false) {
        return Err(format!(
            "{where_}: `expected_ok: false` asserts nothing in particular; \
             write the diagnostics you expect instead"
        ));
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
    let exact = case.expected_exact_output.is_some();
    let expected = case
        .expected_exact_output
        .as_ref()
        .or(case.expected_output.as_ref())
        .expect("checked by check_shape");
    check_output(case, expected, exact, where_)
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
            let real: Vec<&Diagnostic> = diags.iter().filter(|d| d.pass <= IMPLEMENTED).collect();
            if real.is_empty() {
                // Only complaints from passes that do not exist yet.
                return Ok(());
            }
            Err(format!(
                "{where_}: expected the program to be accepted, but:\n{}",
                indent(
                    &real
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

        // An expectation naming a relation that produced nothing this epoch.
        for name in want.keys() {
            if !produced.iter().any(|(n, _)| n == name) {
                return Err(format!(
                    "{where_}, epoch {epoch_idx}: expected relation `{name}` \
                     produced nothing\n  emitted:\n{}",
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
