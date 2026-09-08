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

/// Every directory holding fixtures contributed at least one file.
///
/// The corpus is filed in subdirectories, and a walk that missed one would hide
/// a third of it while leaving every count plausible — a case that is never
/// read is a case that never fails. Asserting coverage rather than a threshold
/// needs no number that goes stale as the corpus grows.
pub fn assert_every_directory_was_walked(dir: &Path, files: &[PathBuf]) {
    let walked: std::collections::BTreeSet<&Path> =
        files.iter().filter_map(|p| p.parent()).collect();
    for entry in std::fs::read_dir(dir).expect("tests/cases") {
        let path = entry.expect("a directory entry").path();
        // An empty directory holds nothing to miss, and faulting one would be a
        // false alarm rather than a caught bug.
        let empty = std::fs::read_dir(&path).is_ok_and(|mut e| e.next().is_none());
        if path.is_dir() && !empty {
            assert!(
                walked.contains(path.as_path()),
                "no fixture was read from `{}`; the walk is not reaching every \
                 directory, and cases nothing reads are cases nothing checks",
                path.display()
            );
        }
    }
}
