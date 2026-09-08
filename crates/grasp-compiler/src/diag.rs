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
/// The variants are **ordered**, and the ordering is the pipeline's. Two things
/// depend on it: [`crate::IMPLEMENTED`] names how far the pipeline reaches, and
/// the YAML harness compares a fixture's required pass against it to decide
/// whether the fixture is live yet. Keep them in pipeline order.
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
