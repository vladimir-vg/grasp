//! What a configuration file may say, and what it is told when it says
//! something else.
//!
//! The configuration is the artifact a person is most likely to get wrong: it
//! is hand-written, it is often copied from a Feldera deployment that has keys
//! this server does not implement, and — unlike a program — nothing about it is
//! checked by a compiler. So the rejections are the subject here, and the
//! acceptances are the smaller half.
//!
//! **Fixtures rather than hand-written cases**, in the shape
//! `grasp-dbsp-runner/tests/cases/` established, because the interesting axis
//! is *many inputs, one assertion*: each case is a file and either the
//! diagnostic it must produce or the settings it must become. A new rejected
//! key is then a fixture, not a function.
//!
//! `deny_unknown_fields` on the case struct for the reason the runner's harness
//! gives: without it a misspelled key in a fixture is silently ignored and the
//! case quietly asserts nothing.

use grasp_dbsp_server::config::PipelineConfig;
use libtest_mimic::{Arguments, Failed, Trial};
use serde::Deserialize;
use std::path::{Path, PathBuf};

fn main() {
    let args = Arguments::from_args();
    let trials = match collect() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to load fixtures: {e}");
            std::process::exit(1);
        }
    };
    libtest_mimic::run(&args, trials).exit();
}

/// One configuration file and what should become of it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    /// The file's text, verbatim.
    config: String,
    /// A program to check the configuration against, for the keys that name
    /// something in one. Optional, because most cases are about the file alone.
    #[serde(default)]
    program: Option<String>,
    /// One substring per diagnostic, matched against the rendering as a whole
    /// and counted exactly — so a case cannot pass on one diagnostic while a
    /// second, unnoticed one is also firing. Absent means the file must be
    /// accepted.
    #[serde(default)]
    expected_diagnostics: Option<Vec<String>>,
    /// Settings the file must produce, checked when it is accepted.
    #[serde(default)]
    expected_name: Option<String>,
    #[serde(default)]
    expected_workers: Option<usize>,
    #[serde(default)]
    expected_storage: Option<bool>,
}

fn collect() -> Result<Vec<Trial>, Box<dyn std::error::Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    files.sort();

    let mut trials = Vec::new();
    for file in files {
        let stem = file.file_stem().unwrap().to_string_lossy().to_string();
        let cases: Vec<Case> = serde_yaml::from_str(&std::fs::read_to_string(&file)?)?;
        for (i, case) in cases.into_iter().enumerate() {
            let name = format!("{stem}::{i}_{}", slug(&case.name));
            trials.push(Trial::test(name, move || run(case).map_err(Failed::from)));
        }
    }
    Ok(trials)
}

fn slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

fn run(case: Case) -> Result<(), String> {
    let parsed = PipelineConfig::parse(&case.config);

    // A program, where the case has one, so that the keys naming a view can be
    // checked. Its diagnostics join the file's.
    let parsed = match (parsed, &case.program) {
        (Ok(config), Some(source)) => {
            let plan = grasp_dbsp_runner::compile(source)
                .map_err(|d| format!("the case's own program does not compile: {d:?}"))?;
            match config.check_against(&plan) {
                Ok(()) => Ok(config),
                Err(d) => Err(d),
            }
        }
        (other, _) => other,
    };

    // Opening the backend is part of being a usable configuration, so an
    // accepted file is taken all the way to a `RunnerConfig`.
    let parsed = parsed.and_then(|c| c.runner().map(|r| (c, r)));

    match (parsed, &case.expected_diagnostics) {
        (Ok((config, runner)), None) => {
            if let Some(want) = &case.expected_name
                && &config.name != want
            {
                return Err(format!("name: wanted `{want}`, got `{}`", config.name));
            }
            if let Some(want) = case.expected_workers
                && runner.workers.get() != want
            {
                return Err(format!("workers: wanted {want}, got {}", runner.workers));
            }
            if let Some(want) = case.expected_storage
                && runner.storage.is_some() != want
            {
                return Err(format!(
                    "storage: wanted {want}, got {}",
                    runner.storage.is_some()
                ));
            }
            Ok(())
        }
        (Ok(_), Some(want)) => Err(format!("accepted, but should have said: {want:?}")),
        (Err(diags), None) => Err(format!(
            "rejected: {}",
            grasp_dbsp_runner::diag::render(&diags)
        )),
        (Err(diags), Some(want)) => {
            let rendered = grasp_dbsp_runner::diag::render(&diags);
            for substring in want {
                if !rendered.contains(substring.as_str()) {
                    return Err(format!("wanted `{substring}` in:\n{rendered}"));
                }
            }
            if diags.len() != want.len() {
                return Err(format!(
                    "wanted {} diagnostics, got {}:\n{rendered}",
                    want.len(),
                    diags.len()
                ));
            }
            Ok(())
        }
    }
}
