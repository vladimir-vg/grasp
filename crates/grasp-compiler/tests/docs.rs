//! Every grasp example in `docs/grasp/` parses.
//!
//! The documents are the specification, and an example that does not parse
//! teaches a program that does not compile. This is the same guarantee
//! `grasp-dbsp-runner`'s `tests/docs.rs` gives its half of the workspace.
//!
//! Some blocks are whole programs and some are fragments — a couple of body
//! statements shown on their own. Rather than curating a list that would drift,
//! the test tries a block as a program and, failing that, as a rule body. A
//! block has to be one or the other.

use std::path::{Path, PathBuf};

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/grasp")
}

/// The contents of every ```` ```grasp ```` block, with the file it came from.
fn grasp_blocks() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(docs_dir())
        .expect("docs/grasp")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    for path in files {
        let text = std::fs::read_to_string(&path).expect("a doc");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // Splitting on the fence gives prose at even indices and code at odd
        // ones, the info string being the code block's first line.
        for (i, block) in text.split("```").enumerate() {
            if i % 2 == 1
                && let Some(body) = block.strip_prefix("grasp\n")
            {
                out.push((name.clone(), body.to_string()));
            }
        }
    }
    out
}

/// A fragment is a body: wrap it in a rule so the parser has somewhere to put
/// it. The head is deliberately trivial — this test is about the block.
fn as_rule_body(block: &str) -> String {
    let indented: String = block
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("    {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("doc_example(v: v) <-\n{indented}\n")
}

#[test]
fn every_grasp_block_in_the_docs_parses() {
    let blocks = grasp_blocks();
    assert!(
        blocks.len() >= 20,
        "found only {} grasp blocks; the extraction is probably broken",
        blocks.len()
    );

    let mut failures = Vec::new();
    for (file, block) in &blocks {
        let as_program = grasp_compiler::parse::parse(block);
        if as_program.is_ok() {
            continue;
        }
        if grasp_compiler::parse::parse(&as_rule_body(block)).is_ok() {
            continue;
        }
        // Report the program-level diagnostic: for a genuine program that is
        // the real error, and for a fragment the body reading has already been
        // tried and also failed.
        failures.push(format!(
            "{file}: {}\n{}",
            as_program.unwrap_err(),
            block
                .lines()
                .map(|l| format!("    {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    assert!(
        failures.is_empty(),
        "{} grasp block(s) in docs/grasp/ do not parse:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
