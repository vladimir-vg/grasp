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
    AGGREGATORS, BUILTINS, DECL_KINDS, KEYWORDS, RESERVED_NAMESPACES, TYPE_NAMES, is_reserved,
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
        "declaration kinds",
        &backticked(bullet(words, "declaration kinds")),
        DECL_KINDS,
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

/// The library the compiler has is the library `stdlib.grasp` declares.
///
/// Read as text here. Once the function typespec is part of the grammar this
/// becomes a parse, and the check grows from the names to the shapes.
#[test]
fn the_builtins_match_stdlib_grasp() {
    let text = doc("stdlib.grasp");
    let listed: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_suffix(" :: function"))
        .map(str::to_string)
        .collect();
    assert_same("builtins", &listed, BUILTINS);
}

/// Every callable lives under a held namespace, which is what makes the shape
/// of a name the answer to whether it is one.
#[test]
fn every_builtin_is_namespaced() {
    for b in BUILTINS {
        let ns = b.split_once(':').map(|(ns, _)| ns);
        assert!(
            ns.is_some_and(|ns| RESERVED_NAMESPACES.contains(&ns)),
            "`{b}` is a callable outside every held namespace, so nothing stops a \
             relation taking its name"
        );
    }
}

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
    for word in KEYWORDS
        .iter()
        .chain(TYPE_NAMES)
        .chain(DECL_KINDS)
        .chain(AGGREGATORS)
    {
        let source = format!("{word}(x: 1)\n");
        assert!(
            grasp_compiler::parse::parse(&source).is_err(),
            "`{word}` is reserved but was accepted as a relation name"
        );
    }
}

/// `core::Builtin` and `parse::BUILTINS` are two lists of one thing.
///
/// `parse::BUILTINS` is what the one-name rule reserves and what
/// `the_builtins_match_semantics_md` above pins against the specification;
/// `core::Builtin` is what desugaring resolves a call to. Nothing connected
/// them, so a builtin added to one and not the other would either be
/// unreservable or unresolvable, and the fixture that caught it would blame
/// something else.
#[test]
fn the_builtin_enum_and_the_reserved_list_agree() {
    use grasp_compiler::core::Builtin;

    for name in BUILTINS {
        assert!(
            Builtin::from_name(name).is_some(),
            "`{name}` is reserved as a callable but `core::Builtin` cannot \
             resolve it, so a program calling it would be told there is no \
             such callable"
        );
    }

    for b in Builtin::ALL {
        let name = b.as_str();
        // The namespaced three are what desugaring writes. They are not in
        // `BUILTINS` because that list is what the *one-name rule* reserves —
        // it stops a relation being called `length`, and no relation can be
        // called `record:get` anyway, since the namespace is reserved outright.
        if name.contains(':') {
            let namespace = name.split_once(':').expect("checked").0;
            assert!(
                RESERVED_NAMESPACES.contains(&namespace),
                "`{name}` lives in `{namespace}:`, which is not a reserved \
                 namespace — so a program could define a relation there and \
                 collide with what desugaring writes"
            );
            continue;
        }
        assert!(
            BUILTINS.contains(&name),
            "`{name}` is a callable but is not in `parse::BUILTINS`, so a \
             relation could be given its name and the one-name rule would not \
             notice"
        );
    }
}
