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

// ---------------------------------------------------------------------------
// The documents hold together as documents
// ---------------------------------------------------------------------------
//
// Two checks over every `.md` in the workspace, not only `docs/grasp/`: a
// link's target has to exist, and a table has to be one table. Both fail
// silently in a rendered page — a dead anchor is a plain link, and prose
// dropped into the middle of a table splits it into two — and the last such
// break was found by a reader and fixed by hand.

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every markdown file in the workspace, skipping build output and the
/// vendored checkout.
fn markdown_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !matches!(name.as_ref(), ".git" | "target" | "vendor") {
                    walk(&path, out);
                }
            } else if name.ends_with(".md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&workspace_root(), &mut out);
    out.sort();
    assert!(
        out.len() >= 14,
        "expected the design documents and READMEs, found {} markdown files",
        out.len()
    );
    out
}

/// GitHub's anchor for a heading: lowercased, punctuation dropped, spaces to
/// hyphens, and a numeric suffix for a repeated heading.
fn anchors(text: &str) -> std::collections::BTreeSet<String> {
    let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
    let mut out = std::collections::BTreeSet::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let heading = line.trim_start_matches('#');
        if heading.len() == line.len() || !heading.starts_with(' ') {
            continue;
        }
        let mut slug = String::new();
        for c in heading.trim().chars() {
            match c {
                c if c.is_alphanumeric() || c == '_' || c == '-' => slug.extend(c.to_lowercase()),
                ' ' => slug.push('-'),
                _ => {}
            }
        }
        let n = seen.entry(slug.clone()).or_insert(0);
        out.insert(if *n == 0 { slug } else { format!("{slug}-{n}") });
        *n += 1;
    }
    out
}

/// The `(target)` of every `[text](target)` outside a code block, with its line.
fn links(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_code = false;
    for (i, line) in text.lines().enumerate() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let mut rest = line;
        while let Some(close) = rest.find("](") {
            let after = &rest[close + 2..];
            let Some(end) = after.find(')') else { break };
            out.push((i + 1, after[..end].to_string()));
            rest = &after[end + 1..];
        }
    }
    out
}

#[test]
fn every_link_in_the_documents_resolves() {
    let files = markdown_files();
    let texts: std::collections::BTreeMap<PathBuf, String> = files
        .iter()
        .map(|p| (p.clone(), std::fs::read_to_string(p).expect("a markdown file")))
        .collect();
    let mut broken = Vec::new();
    for (path, text) in &texts {
        for (line, target) in links(text) {
            if target.starts_with("http://") || target.starts_with("https://") {
                continue;
            }
            let (file, anchor) = match target.split_once('#') {
                Some((f, a)) => (f, Some(a)),
                None => (target.as_str(), None),
            };
            let resolved = if file.is_empty() {
                path.clone()
            } else {
                path.parent().unwrap().join(file)
            };
            let Ok(resolved) = resolved.canonicalize() else {
                broken.push(format!("{}:{line}: `{target}` names no file", path.display()));
                continue;
            };
            if let Some(anchor) = anchor
                && resolved.extension().is_some_and(|x| x == "md")
            {
                let doc = texts
                    .get(&resolved)
                    .cloned()
                    .or_else(|| std::fs::read_to_string(&resolved).ok())
                    .unwrap_or_default();
                if !anchors(&doc).contains(anchor) {
                    broken.push(format!(
                        "{}:{line}: `{target}` names no section",
                        path.display()
                    ));
                }
            }
        }
    }
    assert!(broken.is_empty(), "{} broken link(s):\n{}", broken.len(), broken.join("\n"));
}

#[test]
fn no_table_is_split_by_prose() {
    let mut broken = Vec::new();
    for path in markdown_files() {
        let text = std::fs::read_to_string(&path).expect("a markdown file");
        let lines: Vec<&str> = text.lines().collect();
        let mut in_code = false;
        let mut in_table = false;
        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("```") {
                in_code = !in_code;
                in_table = false;
                continue;
            }
            if in_code {
                continue;
            }
            let row = line.starts_with('|');
            if row && !in_table && i > 0 && !lines[i - 1].trim().is_empty() {
                broken.push(format!(
                    "{}:{}: a table begins without a blank line before it",
                    path.display(),
                    i + 1
                ));
            }
            if in_table && !row && !line.trim().is_empty() {
                broken.push(format!(
                    "{}:{}: prose directly after a table row splits the table",
                    path.display(),
                    i + 1
                ));
            }
            in_table = row;
        }
    }
    assert!(broken.is_empty(), "{} broken table(s):\n{}", broken.len(), broken.join("\n"));
}
