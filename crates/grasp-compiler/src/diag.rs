//! Diagnostics: one type for every problem any compilation pass reports.
//!
//! This is deliberately a near-copy of `grasp-dbsp-runner`'s `src/diag.rs`, and
//! the two are kept in step by hand rather than shared. The reason is [`Pass`]:
//! the two crates compile different languages through different stages, and a
//! shared enum would be the union of both vocabularies, giving every `match` in
//! either crate arms it can never reach. That is coupling, not sharing, and the
//! rest of the file is a hundred lines of trivial data.
//!
//! There is a second reason not to take this type *from* the runner: it would
//! put `dbsp`, `rkyv` and `feldera-sqllib` into the dependency graph of a crate
//! that only manipulates strings. A consumer should not link a DBSP runtime to
//! read an error message.
//!
//! So: if you came here to remove the duplication, this is why it is here.
//! Revisit if a third frontend appears.

use std::fmt;

/// A half-open region of source text, resolved to a line and column.
///
/// Lines and columns are 1-based, which is what editors and every compiler's
/// output use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub line: usize,
    pub column: usize,
    /// Length in bytes. Zero means a point rather than a region.
    pub len: usize,
}

impl Span {
    pub fn new(line: usize, column: usize, len: usize) -> Span {
        Span { line, column, len }
    }

    /// A span covering `self` through `end`, for reporting against a whole
    /// construct given the spans of its first and last tokens.
    pub fn to(self, end: Span) -> Span {
        if end.line != self.line || end.column < self.column {
            // Multi-line: keep the start, and do not invent a length.
            return Span { len: 0, ..self };
        }
        Span {
            len: end.column - self.column + end.len,
            ..self
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The compilation pass a diagnostic came from — the stages of the architecture
/// table in `docs/grasp/overview.md`.
///
/// The variants are **ordered**, and the ordering is the pipeline's. Nothing
/// depends on that today — how far the compiler has got is something it reports
/// per construct, through [`Diagnostic::unimplemented`], rather than something a
/// reader derives from a pass — but the order is the architecture's and costs
/// nothing to keep true.
///
/// Where the doc's eight stages collapse: `Infer` carries the safety check,
/// which `docs/grasp/inference.md` performs in phase 1; `Plan` carries the join
/// graph, the optimizer, the computation DAG and SCC analysis, which are one
/// stage as far as a reader of an error message is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    Parse,
    Desugar,
    Infer,
    Plan,
    Emit,
}

impl Pass {
    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Parse => "parse",
            Pass::Desugar => "desugar",
            Pass::Infer => "infer",
            Pass::Plan => "plan",
            Pass::Emit => "emit",
        }
    }

    /// The inverse of [`Pass::as_str`], for reading a pass out of a fixture.
    pub fn from_name(s: &str) -> Option<Pass> {
        match s {
            "parse" => Some(Pass::Parse),
            "desugar" => Some(Pass::Desugar),
            "infer" => Some(Pass::Infer),
            "plan" => Some(Pass::Plan),
            "emit" => Some(Pass::Emit),
            _ => None,
        }
    }

    /// Every pass, in pipeline order. The harness reports its pending fixtures
    /// broken down by pass, and a new variant should appear there without
    /// anyone having to remember to add it.
    pub const ALL: &'static [Pass] = &[
        Pass::Parse,
        Pass::Desugar,
        Pass::Infer,
        Pass::Plan,
        Pass::Emit,
    ];
}

impl fmt::Display for Pass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub pass: Pass,
    pub message: String,
    pub span: Option<Span>,
    /// The construct this compiler recognised and has not implemented, if that
    /// is what happened.
    ///
    /// This is the difference between *"grasp does not allow that"* and *"this
    /// compiler cannot do that yet"*, and it is worth a field rather than a
    /// turn of phrase because the test suite runs on it: a fixture that fails
    /// because of one of these is pending, and a fixture that fails any other
    /// way is a failure. Nothing else distinguishes the two, and reading it out
    /// of the message would make the distinction a matter of wording.
    ///
    /// The string is the construct, not a sentence — `"aggregates"`, not
    /// `"aggregates are not implemented"` — because the burn-down groups by it
    /// and a per-site phrasing would fragment the count.
    pub unimplemented: Option<String>,
}

impl Diagnostic {
    pub fn error(
        pass: Pass,
        span: impl Into<Option<Span>>,
        message: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            pass,
            message: message.into(),
            span: span.into(),
            unimplemented: None,
        }
    }

    /// Every construct the pipeline can report as unimplemented.
    ///
    /// The burn-down in the test suite groups by this string, so the vocabulary
    /// is pinned here rather than left to each call site: two places blocked by
    /// one feature must count as one thing, and a second spelling would quietly
    /// split the row in half.
    ///
    /// Each entry is a noun phrase that reads after `not implemented: `. The
    /// list is scaffolding — an entry appears when a stage starts reporting it
    /// and goes when that stage implements it, and when the list is empty this
    /// and [`Diagnostic::unimplemented`] go with it.
    /// There is deliberately **no catch-all**. A pass that could report "the
    /// stages after X" would never have to enumerate its gaps, and the burn-down
    /// would stay one number instead of a work queue; without one, a construct
    /// nobody named trips the `debug_assert` below rather than landing silently
    /// in a bucket.
    ///
    /// The plan and emit stages have no entry here at all: everything the
    /// pipeline reaches, it finishes. What is left is three constructs the
    /// front of the pipeline refuses, and the list goes when they do.
    pub const UNIMPLEMENTED: &[&str] = &[
        "narrowing that keeps its wrapper",
        "array destructuring",
        "binding a dict's remaining entries",
        "indexed unnest",
        "keyword arguments",
    ];

    /// A construct this compiler has not implemented yet.
    ///
    /// `construct` names the thing, in the words the language uses for it, and
    /// is what the test suite's burn-down counts — so two sites blocked by the
    /// same feature must pass the same string.
    pub fn unimplemented(
        pass: Pass,
        span: impl Into<Option<Span>>,
        construct: impl Into<String>,
    ) -> Diagnostic {
        let construct = construct.into();
        debug_assert!(
            Diagnostic::UNIMPLEMENTED.contains(&construct.as_str()),
            "`{construct}` is not in `Diagnostic::UNIMPLEMENTED`; the burn-down \
             groups by this string, so a new spelling of an existing gap would \
             split its count"
        );
        Diagnostic {
            severity: Severity::Error,
            pass,
            message: format!("not implemented: {construct}"),
            span: span.into(),
            unimplemented: Some(construct),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.span {
            Some(span) => write!(
                f,
                "{}[{}] at {}: {}",
                self.severity, self.pass, span, self.message
            ),
            None => write!(f, "{}[{}]: {}", self.severity, self.pass, self.message),
        }
    }
}

impl std::error::Error for Diagnostic {}

/// Renders a list of diagnostics one per line, for a panic message or a CLI.
pub fn render(diags: &[Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
