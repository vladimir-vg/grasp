//! The YAML fixture harness.
//!
//! Every file under `tests/cases/` holds a list of cases; each becomes one test
//! named `<file stem>::<index>`, so `cargo test --test yaml joins` filters and
//! a failure names itself.
//!
//! The format is documented in `tests/cases/README.md`.
//!
//! Fixture values are converted to `serde_json::Value` and then go through the
//! runner's own codec in `src/json.rs` — there is deliberately no second
//! decoder here, so a fixture cannot drift from the real wire format.

use dbsp_runner::diag::Diagnostic;
use dbsp_runner::json::{Format, decode_value, encode_delta_insert_delete, encode_value};
use dbsp_runner::lower::Runner;
use dbsp_runner::value::{BatchType, TypeDesc};
use libtest_mimic::{Arguments, Failed, Trial};
use serde::Deserialize;
use serde_json::Value as J;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    let args = Arguments::from_args();
    let trials = match collect_trials() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to load fixtures: {e}");
            std::process::exit(1);
        }
    };
    libtest_mimic::run(&args, trials).exit();
}

// ---------------------------------------------------------------------------
// Fixture types
// ---------------------------------------------------------------------------

/// In the default `weighted` format a row is `[weight, row]` for a flat stream
/// and `[weight, key, value]` for an indexed one, the arity distinguishing them.
/// Under `insert_delete` a row is instead a `{insert: …}` / `{delete: …}` map.
type Row = serde_yaml::Value;
/// One transaction: output (or table) name to its rows.
type Epoch = BTreeMap<String, Vec<Row>>;

/// `deny_unknown_fields` matters here: without it a misspelled key in a fixture
/// is silently ignored and the case quietly asserts nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: Option<String>,
    source: String,
    #[serde(default)]
    input: Vec<Epoch>,
    #[serde(default)]
    expected_output: Option<Vec<Epoch>>,
    #[serde(default)]
    expected_exact_output: Option<Vec<Epoch>>,
    #[serde(default)]
    expected_diagnostics: Option<Vec<ExpectedDiagnostic>>,
    /// `weighted` (the default) or `insert_delete`.
    #[serde(default)]
    output_format: Option<String>,
}

/// Only the fields a fixture actually writes are checked, so new diagnostic
/// fields can be added without touching existing fixtures.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedDiagnostic {
    severity: Option<String>,
    pass: Option<String>,
    /// Substring match.
    message: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
}

impl ExpectedDiagnostic {
    fn matches(&self, d: &Diagnostic) -> bool {
        let sev_ok = self.severity.as_ref().is_none_or(|s| s == d.severity.as_str());
        let pass_ok = self.pass.as_ref().is_none_or(|s| s == d.pass.as_str());
        let msg_ok = self.message.as_ref().is_none_or(|m| d.message.contains(m.as_str()));
        let line_ok = self.line.is_none_or(|l| d.span.is_some_and(|s| s.line == l));
        let col_ok = self.column.is_none_or(|c| d.span.is_some_and(|s| s.column == c));
        sev_ok && pass_ok && msg_ok && line_ok && col_ok
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.severity {
            parts.push(format!("severity={v}"));
        }
        if let Some(v) = &self.pass {
            parts.push(format!("pass={v}"));
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
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let cases: Vec<Case> = serde_yaml::from_str(&text)
            .map_err(|e| format!("{}: {e}", path.display()))?;

        for (i, case) in cases.into_iter().enumerate() {
            let name = match &case.name {
                Some(n) => format!("{stem}::{i}_{}", slug(n)),
                None => format!("{stem}::{i}"),
            };
            let where_ = format!("{}, case {i}", path.display());
            trials.push(Trial::test(name, move || {
                run_case(&case, &where_).map_err(Failed::from)
            }));
        }
    }
    Ok(trials)
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

// ---------------------------------------------------------------------------
// Running a case
// ---------------------------------------------------------------------------

fn run_case(case: &Case, where_: &str) -> Result<(), String> {
    let modes = [
        case.expected_output.is_some(),
        case.expected_exact_output.is_some(),
        case.expected_diagnostics.is_some(),
    ];
    match modes.iter().filter(|x| **x).count() {
        1 => {}
        0 => {
            return Err(format!(
                "{where_}: a case needs exactly one `expected_*` key \
                 (expected_output, expected_exact_output or expected_diagnostics)"
            ));
        }
        _ => return Err(format!("{where_}: a case may have only one `expected_*` key")),
    }

    if let Some(expected) = &case.expected_diagnostics {
        return check_diagnostics(case, expected, where_);
    }

    let exact = case.expected_exact_output.is_some();
    let expected = case
        .expected_exact_output
        .as_ref()
        .or(case.expected_output.as_ref())
        .expect("checked above");
    check_output(case, expected, exact, where_)
}

fn check_diagnostics(
    case: &Case,
    expected: &[ExpectedDiagnostic],
    where_: &str,
) -> Result<(), String> {
    let actual = match dbsp_runner::compile(&case.source) {
        Ok(_) => {
            return Err(format!("{where_}: expected compilation to fail, but it succeeded"));
        }
        Err(diags) => diags,
    };

    // Exact set, order-independent: pair each expectation with a distinct
    // diagnostic, then require nothing is left over on either side.
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
                    indent(&render_diags(&actual))
                ));
            }
        }
    }
    if !unmatched.is_empty() {
        let extra: Vec<String> = unmatched.iter().map(|d| d.to_string()).collect();
        return Err(format!(
            "{where_}: {} unexpected diagnostic(s):\n{}",
            extra.len(),
            indent(&extra.join("\n"))
        ));
    }
    Ok(())
}

fn check_output(
    case: &Case,
    expected: &[Epoch],
    exact: bool,
    where_: &str,
) -> Result<(), String> {
    if expected.len() != case.input.len() {
        return Err(format!(
            "{where_}: {} input epoch(s) but {} expected epoch(s); \
             use `{{}}` for an epoch that produces nothing",
            case.input.len(),
            expected.len()
        ));
    }

    let plan = dbsp_runner::compile(&case.source)
        .map_err(|d| format!("{where_}: compilation failed:\n{}", indent(&render_diags(&d))))?;

    // Outputs are whatever the expectations mention, as grasp-dbsp does.
    let mut outputs: Vec<String> =
        expected.iter().flat_map(|e| e.keys().cloned()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    outputs.sort();

    let mut runner = Runner::build(&plan, &outputs)
        .map_err(|d| format!("{where_}: building the circuit failed:\n{}", indent(&render_diags(&d))))?;

    for (epoch_idx, (inputs, want)) in case.input.iter().zip(expected).enumerate() {
        // Feed this transaction.
        for (table, rows) in inputs {
            // The fixture keys inputs by *table* name — the string in
            // `input("...")` — which need not equal the node's name.
            let node_idx = plan
                .inputs()
                .into_iter()
                .find(|(_, t)| *t == table.as_str())
                .map(|(i, _)| i)
                .ok_or_else(|| format!("{where_}: no input table `{table}`"))?;
            let row_type = match &plan.nodes[node_idx].ty {
                BatchType::ZSet(t) => t.clone(),
                BatchType::IndexedZSet(..) => {
                    return Err(format!("{where_}: input `{table}` is indexed"));
                }
            };
            for row in rows {
                let (value, weight) = decode_input_row(row, &row_type)
                    .map_err(|e| format!("{where_}, epoch {epoch_idx}, table `{table}`: {e}"))?;
                runner
                    .push(table, value, weight)
                    .map_err(|e| format!("{where_}: {e}"))?;
            }
        }

        let format = match case.output_format.as_deref() {
            None | Some("weighted") => Format::Weighted,
            Some("insert_delete") => Format::InsertDelete,
            Some(other) => {
                return Err(format!(
                    "{where_}: unknown output_format `{other}`; \
                     expected `weighted` or `insert_delete`"
                ));
            }
        };

        let produced = runner.step().map_err(|e| format!("{where_}: {e}"))?;

        for (name, deltas) in &produced {
            let ty = &plan.node(name).expect("output exists").ty;
            let mut actual: Vec<J> = match format {
                Format::Weighted => deltas
                    .iter()
                    .map(|d| {
                        let mut row = vec![
                            J::from(d.weight),
                            encode_value(&d.key, key_type(ty)).map_err(|e| e.to_string())?,
                        ];
                        if let (Some(v), BatchType::IndexedZSet(_, vt)) = (&d.value, ty) {
                            row.push(encode_value(v, vt).map_err(|e| e.to_string())?);
                        }
                        Ok(J::Array(row))
                    })
                    .collect::<Result<_, String>>()
                    .map_err(|e| format!("{where_}: encoding output `{name}`: {e}"))?,
                // One record per unit of weight, so the count is the assertion.
                Format::InsertDelete => {
                    let mut out = Vec::new();
                    for d in deltas {
                        out.extend(
                            encode_delta_insert_delete(d, ty)
                                .map_err(|e| format!("{where_}: encoding `{name}`: {e}"))?,
                        );
                    }
                    out
                }
            };

            let mut wanted: Vec<J> = want
                .get(name)
                .map(|rows| rows.iter().map(|r| canon(&yaml_to_json(r))).collect())
                .unwrap_or_default();

            for v in &mut actual {
                *v = canon(v);
            }
            actual.sort_by_key(|v| v.to_string());
            wanted.sort_by_key(|v| v.to_string());

            let ok = if exact {
                actual == wanted
            } else {
                wanted.iter().all(|w| actual.contains(w))
            };
            if !ok {
                return Err(format!(
                    "{where_}, epoch {epoch_idx}, output `{name}`: {} mismatch\n  expected:\n{}\n  actual:\n{}",
                    if exact { "exact" } else { "subset" },
                    indent(&rows_to_string(&wanted)),
                    indent(&rows_to_string(&actual)),
                ));
            }
        }

        // An expectation naming an output that produced nothing this epoch.
        for name in want.keys() {
            if !produced.iter().any(|(n, _)| n == name) {
                return Err(format!(
                    "{where_}, epoch {epoch_idx}: expected output `{name}` was not produced"
                ));
            }
        }
    }
    Ok(())
}

fn key_type(ty: &BatchType) -> &TypeDesc {
    match ty {
        BatchType::ZSet(t) => t,
        BatchType::IndexedZSet(k, _) => k,
    }
}

/// `[weight, row]` for a flat input table.
fn decode_input_row(row: &Row, ty: &TypeDesc) -> Result<(dbsp_runner::value::DynValue, i64), String> {
    let row = row
        .as_sequence()
        .ok_or_else(|| "an input row is [weight, row]".to_string())?;
    if row.len() != 2 {
        return Err(format!("an input row is [weight, row]; found {} element(s)", row.len()));
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
            .or_else(|| n.as_f64().and_then(serde_json::Number::from_f64).map(J::Number))
            .unwrap_or(J::Null),
        serde_yaml::Value::String(s) => J::String(s.clone()),
        serde_yaml::Value::Sequence(items) => J::Array(items.iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(map) => J::Object(
            map.iter()
                .map(|(k, v)| {
                    let key = k.as_str().map(str::to_string).unwrap_or_else(|| format!("{k:?}"));
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

fn rows_to_string(rows: &[J]) -> String {
    if rows.is_empty() {
        return "(nothing)".to_string();
    }
    rows.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("\n")
}

fn render_diags(diags: &[Diagnostic]) -> String {
    if diags.is_empty() {
        return "(none)".to_string();
    }
    diags.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n")
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}
