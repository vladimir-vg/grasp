//! The reserved names in the code are the reserved names in the specification.
//!
//! `docs/grasp/syntax.md` lists the keywords, type names and aggregators that
//! may not name a relation, a variable or a column, and the namespace prefixes
//! the language holds; `docs/grasp/semantics.md` lists the builtins. Those
//! lists are also in `src/parse.rs`, and two copies of a list drift. This reads
//! the documents and compares.
//!
//! It is the same guarantee `grasp-dbsp-runner`'s `tests/reserved.rs` gives
//! `docs/grasp-dbsp/language.md`.

use grasp_compiler::parse::{
    AGGREGATORS, BUILTINS, KEYWORDS, RESERVED_NAMESPACES, TYPE_NAMES, is_reserved,
};
use std::path::{Path, PathBuf};

fn doc(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/grasp")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every `` `backticked` `` token in a stretch of text.
fn backticked(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else { break };
        out.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    out
}

/// The text of one `- **label** — …` bullet, which may wrap onto later lines.
fn bullet<'a>(section: &'a str, label: &str) -> &'a str {
    let marker = format!("- **{label}**");
    let start = section
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` bullet in the section"));
    let rest = &section[start + marker.len()..];
    // A bullet ends at the next one or at a blank line — the prose after the
    // last bullet in a list is not part of it.
    let end = [rest.find("\n- "), rest.find("\n\n")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

fn section<'a>(text: &'a str, heading: &str, next: &str) -> &'a str {
    let start = text
        .find(heading)
        .unwrap_or_else(|| panic!("no `{heading}` heading"));
    let rest = &text[start..];
    let end = rest.find(next).unwrap_or(rest.len());
    &rest[..end]
}

fn assert_same(what: &str, doc_list: &[String], code: &[&str]) {
    let mut doc_sorted: Vec<&str> = doc_list.iter().map(String::as_str).collect();
    let mut code_sorted: Vec<&str> = code.to_vec();
    doc_sorted.sort_unstable();
    code_sorted.sort_unstable();
    assert_eq!(
        doc_sorted, code_sorted,
        "the {what} in the docs and in src/parse.rs disagree"
    );
}

#[test]
fn the_reserved_words_match_syntax_md() {
    let text = doc("syntax.md");
    let words = section(
        &text,
        "### Reserved words",
        "### Reserved namespace prefixes",
    );

    assert_same("keywords", &backticked(bullet(words, "keywords")), KEYWORDS);
    assert_same(
        "type names",
        &backticked(bullet(words, "type names")),
        TYPE_NAMES,
    );
    assert_same(
        "aggregators",
        &backticked(bullet(words, "aggregators")),
        AGGREGATORS,
    );
}

#[test]
fn the_reserved_namespaces_match_syntax_md() {
    let text = doc("syntax.md");
    let prefixes = section(&text, "### Reserved namespace prefixes", "## AST");
    // The prose spells them with their colon: "`string:`, `array:`, …".
    let listed: Vec<String> = backticked(prefixes)
        .into_iter()
        .filter_map(|s| s.strip_suffix(':').map(str::to_string))
        .collect();
    assert_same("reserved namespaces", &listed, RESERVED_NAMESPACES);
}

#[test]
fn the_builtins_match_semantics_md() {
    let text = doc("semantics.md");
    let builtins = section(&text, "## Builtins", "## Example");
    // The first column of each table row holds the names.
    let listed: Vec<String> = builtins
        .lines()
        .filter(|l| l.starts_with("| `"))
        .flat_map(|l| backticked(l.split('|').nth(1).expect("a first column")))
        .collect();
    assert_same("builtins", &listed, BUILTINS);
}

/// The one-name rule is a separate check from reservation, and the difference
/// is visible: a column may be called `length`, but a relation may not.
#[test]
fn builtins_are_not_reserved_words() {
    for b in BUILTINS {
        assert!(
            !is_reserved(b),
            "`{b}` is a builtin, which does not make it a reserved word — \
             `docs/grasp/syntax.md` says a column may be called `length`"
        );
    }
}

#[test]
fn every_reserved_word_is_rejected_as_a_relation_name() {
    for word in KEYWORDS.iter().chain(TYPE_NAMES).chain(AGGREGATORS) {
        let source = format!("{word}(x: 1)\n");
        assert!(
            grasp_compiler::parse::parse(&source).is_err(),
            "`{word}` is reserved but was accepted as a relation name"
        );
    }
}
