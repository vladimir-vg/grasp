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

use grasp_compiler::parse::{AGGREGATORS, DECL_KINDS, KEYWORDS, RESERVED_NAMESPACES, TYPE_NAMES};
use std::collections::{BTreeMap, BTreeSet};
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
/// Read as grasp, not as text: the file is parsed and its typespecs are compared
/// to `core::Builtin`'s signatures, so a builtin whose shape drifted from the
/// declared one is a failure here rather than a surprise at a call site.
///
/// Three claims, and each has a way of failing:
///
/// - every implemented callable is declared, with the same shapes — so a
///   `Builtin` added without an entry fails;
/// - every declared name the compiler has not got is in `core::DESIGNED` — so an
///   entry naming nothing fails, and a function implemented without leaving that
///   list fails too;
/// - nothing is declared that no program may write.
#[test]
fn the_library_is_stdlib_grasp() {
    use grasp_compiler::ast::{Decl, Shape};
    use grasp_compiler::core::{Builtin, DESIGNED};

    let text = doc("stdlib.grasp");
    let program = grasp_compiler::parse::parse(&text)
        .unwrap_or_else(|e| panic!("docs/grasp/stdlib.grasp does not parse: {e}"));

    let mut declared: BTreeMap<String, Vec<Shape>> = BTreeMap::new();
    for decl in &program {
        let Decl::Function(spec) = decl else {
            panic!("stdlib.grasp declares the library and nothing else, but it has a {decl:?}");
        };
        let mut shapes: Vec<Shape> = spec.variants.iter().flat_map(|v| v.shapes()).collect();
        shapes.sort();
        // A **set**: two variants of one shape are an overload, and which side
        // spells the alternatives is a matter of how each says "either type".
        // The file writes two lines; the compiler may write one signature whose
        // check accepts both, as the polymorphic component accessors do.
        shapes.dedup();
        declared.insert(spec.name.clone(), shapes);
    }

    // The names a program may write. A family's implementations answer to the
    // head's name and are not callable themselves, so this is a set of *names*
    // rather than of variants.
    let writable: BTreeSet<&str> = Builtin::ALL
        .iter()
        .filter(|b| b.callable())
        .map(|b| b.as_str())
        .collect();

    for b in Builtin::ALL {
        let name = b.as_str();
        if !b.callable() {
            assert!(
                writable.contains(name) || !declared.contains_key(name),
                "`{name}` is written by desugaring and is not a name a program \
                 may take, so declaring it in the library offers something \
                 nothing can call"
            );
            continue;
        }
        let Some(shapes) = declared.get(name) else {
            panic!(
                "`{name}` is a callable this compiler has, but the library does \
                 not declare it — a program could call something no document \
                 describes"
            );
        };
        // A signature covers every shape its optional parameters allow, so this
        // is where `array:slice`'s one signature meets the eight variants the
        // file writes out.
        let mut mine: Vec<Shape> = b.signatures().iter().flat_map(|s| s.shapes()).collect();
        mine.sort();
        mine.dedup();
        assert_eq!(
            shapes, &mine,
            "`{name}` is declared with different shapes than the compiler accepts, \
             so the document and the code disagree about how it is called"
        );
    }

    for name in declared.keys() {
        if Builtin::from_name(name).is_some() {
            continue;
        }
        assert!(
            DESIGNED.contains(&name.as_str()),
            "the library declares `{name}`, which this compiler has neither \
             implemented nor listed in `core::DESIGNED` — so calling it would be \
             reported as a name that does not exist"
        );
    }

    for name in DESIGNED {
        assert!(
            declared.contains_key(*name),
            "`core::DESIGNED` names `{name}`, which the library does not declare \
             — a gap the burn-down counts and no document describes"
        );
        assert!(
            Builtin::from_name(name).is_none(),
            "`{name}` is implemented but still in `core::DESIGNED`, so a call to \
             it would be reported unimplemented after it works"
        );
    }
}

/// Every callable lives under a held namespace, which is what makes the shape of
/// a name the answer to whether it is one.
///
/// It is also the whole of the one-name rule now. There is no separate list of
/// callable names to keep a relation off: `record:get` is unavailable because
/// `record:` is held, and `length` is available because nothing is called that.
#[test]
fn every_callable_is_namespaced() {
    use grasp_compiler::core::Builtin;

    for b in Builtin::ALL {
        let name = b.as_str();
        let ns = name.split_once(':').map(|(ns, _)| ns);
        assert!(
            ns.is_some_and(|ns| RESERVED_NAMESPACES.contains(&ns)),
            "`{name}` is a callable outside every held namespace, so nothing \
             stops a relation taking its name"
        );
    }
    for name in grasp_compiler::core::DESIGNED {
        let ns = name.split_once(':').map(|(ns, _)| ns);
        assert!(
            ns.is_some_and(|ns| RESERVED_NAMESPACES.contains(&ns)),
            "`{name}` is a designed callable outside every held namespace, so a \
             program could define a relation there and collide with it when it \
             lands"
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
