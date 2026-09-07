//! Diagnostics: one type for every problem any compilation pass reports.
//!
//! The passes currently stop at the first error, so a `Vec<Diagnostic>` usually
//! holds exactly one. They return a vector anyway, because error recovery —
//! reporting every problem in a pass rather than the first — is a change to the
//! passes and not to this type or to anything that consumes it.
//!
//! [`Diagnostic::span`] is optional for the same reason: a pass that does not
//! yet know where a problem is says so, instead of pointing somewhere plausible
//! and wrong.

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

/// The compilation pass a diagnostic came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    Parse,
    Typecheck,
    Lower,
}

impl Pass {
    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Parse => "parse",
            Pass::Typecheck => "typecheck",
            Pass::Lower => "lower",
        }
    }
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
