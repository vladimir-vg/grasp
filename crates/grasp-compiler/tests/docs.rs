//! Every grasp example in `docs/grasp/` is grasp.
//!
//! The documents are the specification, and an example that does not compile
//! teaches a program that does not compile. This is the same guarantee
//! `grasp-dbsp-runner`'s `tests/docs.rs` gives its half of the workspace.
//!
//! Some blocks are whole programs and some are fragments — a couple of body
//! statements shown on their own. Rather than curating a list that would drift,
//! the test tries a block as a program and, failing that, as a rule body. A
//! block has to be one or the other.
//!
//! A block read as a **program** is typechecked, not merely parsed, and the one
//! thing forgiven is a relation the excerpt does not declare: an example about
//! a rule would be buried by the schema of every relation it reads. Everything
//! else fails — a callable that does not exist, a call of the wrong shape, a
//! type that cannot compose — and the first two are checked before inference,
//! so they are caught in an excerpt too.
//!
//! A block read as a **rule body** is only parsed. A fragment has no context:
//! `d := {}` has no element type until something uses it, which is what makes
//! it a fragment rather than a program.

use grasp_compiler::diag::{Diagnostic, Pass};
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

/// A diagnostic an excerpt earns by being one.
///
/// An example about a rule does not declare the relations it reads, and giving
/// each one a schema would bury the two lines the section is about. This is the
/// only thing forgiven, and the message is pinned by
/// `tests/cases/programs/relations.yaml`, so the wording read here cannot
/// change without a case saying so.
fn is_excerpt(d: &Diagnostic) -> bool {
    d.message.contains("is not defined and has no typespec")
}

#[test]
fn every_grasp_block_in_the_docs_is_grasp() {
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
            match grasp_compiler::check(block) {
                Ok(_) => continue,
                Err(diags) if diags.iter().all(is_excerpt) => continue,
                Err(diags) => {
                    failures.push(report(file, &grasp_compiler::diag::render(&diags), block));
                    continue;
                }
            }
        }
        let body = as_rule_body(block);
        if grasp_compiler::parse::parse(&body).is_ok() {
            // A fragment is parsed and *desugared*, not typed. Desugaring is
            // the pass that resolves a name to a callable and a call to one of
            // its variants, so `string:len(s)` and a call of the wrong shape
            // fail here — where inference could say nothing, a fragment having
            // no schema to read and no context to settle an empty container.
            match grasp_compiler::check(&body) {
                Ok(_) => continue,
                Err(diags) => {
                    let named: Vec<&Diagnostic> =
                        diags.iter().filter(|d| d.pass == Pass::Desugar).collect();
                    if named.is_empty() {
                        continue;
                    }
                    failures.push(report(
                        file,
                        &named
                            .iter()
                            .map(|d| d.to_string())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        block,
                    ));
                    continue;
                }
            }
        }
        // Report the program-level diagnostic: for a genuine program that is
        // the real error, and for a fragment the body reading has already been
        // tried and also failed.
        failures.push(report(file, &as_program.unwrap_err().to_string(), block));
    }

    assert!(
        failures.is_empty(),
        "{} grasp block(s) in docs/grasp/ are not grasp:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn report(file: &str, why: &str, block: &str) -> String {
    let indent = |text: &str| {
        text.lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("{file}:\n{}\n{}", indent(why), indent(block))
}
