//! Fixture discovery, shared by the harness and the declaration guard.
//!
//! Both walk `tests/cases/`, and both must walk it the same way: a case one of
//! them cannot see is a case that silently stops being checked. That is the
//! whole reason this is one module rather than two copies.

use std::path::{Path, PathBuf};

/// Every fixture file under `dir`, at any depth, sorted.
///
/// Recursion is what lets the corpus be filed in directories. A subdirectory
/// would otherwise vanish *silently* — it survives `read_dir` and is then
/// dropped for not having a `.yaml` extension — which is why `declared.rs`
/// asserts that every directory contributed something.
pub fn fixture_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let path = entry.map_err(|e| format!("{}: {e}", d.display()))?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "yaml" || x == "yml") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// A file's label in a trial name: its path under `tests/cases`, without the
/// extension.
///
/// The directory is part of it, so two files sharing a basename stay
/// distinguishable and `--test yaml syntax/` selects a directory.
pub fn label(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir)
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .into_owned()
}
