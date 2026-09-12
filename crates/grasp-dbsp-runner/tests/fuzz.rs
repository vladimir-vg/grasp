//! Nothing a program can say makes the front end panic.
//!
//! `overview.md`: "no reachable path may produce an internal error". A
//! diagnostic is the only acceptable answer to any text at all, and this holds
//! the front end to that with what a hand-written corpus cannot: text nobody
//! wrote on purpose. Every fixture source and every example in the documents
//! is a seed, and each case takes one and damages it — a character dropped, a
//! token inserted, a span cut out, two lines swapped, a line doubled — a few
//! times over, then compiles the result. Whatever comes back is fine as long as
//! it *comes back*.

use proptest::prelude::*;
use std::path::{Path, PathBuf};

const TOKENS: &[&str] = &[
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    ":",
    ",",
    ".",
    "_",
    "*",
    "**",
    ":=",
    "::",
    "<-",
    "->",
    "=>",
    "not",
    "and",
    "or",
    "input",
    "true",
    "false",
    "NONE",
    "null",
    "\n",
    "    ",
    " ",
    "\"",
    "\\",
    "-",
    "+",
    "/",
    "%",
    "++",
    "=",
    "==",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "#",
    "0",
    "1.5",
    "9999999999999999999999",
    "record",
    "array",
    "dict",
    "optional",
    "json",
    "dynamic",
    "bytes",
    "date",
    "time",
    "timestamp",
    "interval",
    "sum",
    "count",
    "min",
    "max",
    "avg",
    "function",
    "circuit",
    "fixpoint",
    "empty()",
    "constant([])",
    "map",
    "join",
    "filter",
    "cast",
    "coalesce",
    "if",
    "get",
    "slice",
    "map_array",
    "filter_array",
    "string:length",
    "array:at",
    "dict:get",
    "temporal:date",
    "dynamic:of",
    "\u{e9}",
    "\u{0}",
    "\t",
];

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, ext, out);
        } else if path.extension().is_some_and(|x| x == ext) {
            out.push(path);
        }
    }
}

/// Every ```` ```LANG ```` block in the documents and every `source:` in the
/// fixtures — read once, since every case draws from the same corpus.
fn seeds(docs: &[&str], fixtures: &str, fence: &str) -> &'static [String] {
    static SEEDS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    SEEDS.get_or_init(|| collect_seeds(docs, fixtures, fence))
}

fn collect_seeds(docs: &[&str], fixtures: &str, fence: &str) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for dir in docs {
        let mut files = Vec::new();
        walk(&root.join(dir), "md", &mut files);
        for file in files {
            let text = std::fs::read_to_string(file).expect("a document");
            for (i, block) in text.split("```").enumerate() {
                if i % 2 == 1
                    && let Some(body) = block.strip_prefix(fence)
                    && let Some(body) = body.strip_prefix('\n')
                {
                    out.push(body.to_string());
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(&root.join(fixtures), "yaml", &mut files);
    for file in files {
        let text = std::fs::read_to_string(file).expect("a fixture");
        let Ok(cases) = serde_yaml::from_str::<Vec<serde_yaml::Value>>(&text) else {
            continue;
        };
        for case in cases {
            for key in ["source", "equivalent_to"] {
                if let Some(serde_yaml::Value::String(s)) = case.get(key) {
                    out.push(s.clone());
                }
            }
        }
    }
    out.retain(|s| !s.trim().is_empty());
    out.sort();
    out.dedup();
    assert!(out.len() > 50, "only {} seeds", out.len());
    out
}

#[derive(Debug, Clone)]
enum Op {
    Drop(usize),
    Insert(usize, usize),
    Cut(usize, usize),
    Swap(usize, usize),
    Double(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<usize>().prop_map(Op::Drop),
        (any::<usize>(), any::<usize>()).prop_map(|(i, t)| Op::Insert(i, t)),
        (any::<usize>(), any::<usize>()).prop_map(|(i, j)| Op::Cut(i, j)),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Op::Swap(a, b)),
        any::<usize>().prop_map(Op::Double),
    ]
}

/// A character boundary at or after `i % (len + 1)`.
fn at(s: &str, i: usize) -> usize {
    let mut i = i % (s.len() + 1);
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn apply(mut s: String, ops: &[Op]) -> String {
    for op in ops {
        match *op {
            Op::Drop(i) => {
                let i = at(&s, i);
                if i < s.len() {
                    let next = s[i..].chars().next().map_or(1, char::len_utf8);
                    s.replace_range(i..i + next, "");
                }
            }
            Op::Insert(i, t) => {
                let i = at(&s, i);
                s.insert_str(i, TOKENS[t % TOKENS.len()]);
            }
            Op::Cut(i, j) => {
                let (i, j) = (at(&s, i), at(&s, j));
                let (i, j) = (i.min(j), i.max(j));
                s.replace_range(i..j, "");
            }
            Op::Swap(a, b) => {
                let mut lines: Vec<&str> = s.lines().collect();
                if !lines.is_empty() {
                    let (a, b) = (a % lines.len(), b % lines.len());
                    lines.swap(a, b);
                    s = lines.join("\n");
                }
            }
            Op::Double(a) => {
                let mut lines: Vec<&str> = s.lines().collect();
                if !lines.is_empty() {
                    let a = a % lines.len();
                    lines.insert(a, lines[a]);
                    s = lines.join("\n");
                }
            }
        }
    }
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn a_damaged_program_gets_a_diagnostic_and_never_a_panic(
        pick in any::<usize>(),
        ops in proptest::collection::vec(op(), 1..4),
    ) {
        // `docs/grasp/mapping.md` holds emitted examples in this language too.
        let seeds = seeds(&["../../docs/grasp-dbsp", "../../docs/grasp"], "tests/cases", "grasp-dbsp");
        let source = apply(seeds[pick % seeds.len()].clone(), &ops);
        // Either answer is fine; a panic is the one thing that is not.
        let _ = grasp_dbsp_runner::compile(&source);
    }
}
