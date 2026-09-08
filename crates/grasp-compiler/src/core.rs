//! The core forms — what desugaring produces and everything below consumes.
//!
//! `docs/grasp/compilation.md` names this representation: the pipeline is
//! `text → AST → core → typed core → join graph → …`. It is a **smaller**
//! language than the AST, not an annotated one, and that is the point of giving
//! it its own types rather than documenting an invariant on `ast`.
//!
//! What it drops, and what each drop buys:
//!
//! - `Expr::Field` — `s.f` becomes `record:get(s, "f")`. This one is the reason
//!   the module exists. A `Field` that reached emission would come out as
//!   grasp-dbsp `s.f` and **work**, because `mapping.md` maps `s.field` to
//!   `s.field` — so a bug where field desugaring was skipped would be invisible
//!   to every test there is. `BinOp::Concat` surviving would emit `a ++ b` and
//!   be rejected loudly; `Field` would not. An invariant whose violation cannot
//!   be observed is the one worth making unrepresentable.
//! - `BinOp::Concat` and `UnOp::Not` — both become calls.
//! - `Pattern::{Array, Dict, Record}` and `Rest` — the destructures expand into
//!   a binding and the filters that make them exact, so [`Pattern`] here is
//!   `Var` or `Unnest` and nothing else. The unnests stay: they are generative
//!   rather than sugar, and become a `flat_map`.
//!
//! Callables are a [`Builtin`] rather than a `String`, so the three spellings
//! desugaring writes — `record:get`, `dict:get`, `boolean:not` — are variants
//! two later stages cannot disagree about.
//!
//! Everything else is the AST's, including [`crate::ast::Type`], which
//! desugaring does not touch.

use crate::ast::{Aggregator, BinOp, Lit, Type, UnOp};
use crate::diag::Span;

pub type Program = Vec<Decl>;

#[derive(Debug, Clone, PartialEq)]
pub enum Decl {
    Spec(Spec),
    Fact(Fact),
    Rule(Rule),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub relation: String,
    pub columns: Vec<(String, Type)>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub relation: String,
    /// Closed expressions — a fact's arguments have no body to read a variable
    /// from, which the safety check is what enforces.
    pub args: Vec<(String, Expr)>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub head: Head,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    pub relation: String,
    /// No wildcard: a head produces a value for every column it names.
    pub args: Vec<(String, Expr)>,
    pub span: Span,
}

/// One argument of a body atom. Unlike a head's, this may be a wildcard —
/// "there is a column here I do not care about".
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Expr(Expr),
    Wildcard(Span),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Atom {
        relation: String,
        args: Vec<(String, Arg)>,
        negated: bool,
        span: Span,
    },
    Match {
        lhs: Pattern,
        rhs: Rhs,
        span: Span,
    },
    Filter {
        expr: Expr,
        span: Span,
    },
    Assert {
        variable: String,
        ty: Type,
        span: Span,
    },
    Input {
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Atom { span, .. }
            | Stmt::Match { span, .. }
            | Stmt::Filter { span, .. }
            | Stmt::Assert { span, .. }
            | Stmt::Input { span } => *span,
        }
    }
}

/// `Var` or `Unnest`, and nothing else — the destructures are gone.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Var {
        name: String,
        span: Span,
    },
    Unnest {
        vars: Vec<String>,
        kind: UnnestKind,
        span: Span,
    },
}

pub use crate::ast::UnnestKind;

#[derive(Debug, Clone, PartialEq)]
pub enum Rhs {
    Expr(Expr),
    Aggregate {
        function: Aggregator,
        arg: Option<Expr>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit {
        value: Lit,
        span: Span,
    },
    Var {
        name: String,
        span: Span,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Call {
        callee: Builtin,
        args: Vec<Expr>,
        span: Span,
    },
    ArrayLit {
        elems: Vec<Expr>,
        span: Span,
    },
    DictLit {
        entries: Vec<(Expr, Expr)>,
        span: Span,
    },
    RecordLit {
        fields: Vec<(String, Expr)>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Lit { span, .. }
            | Expr::Var { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::ArrayLit { span, .. }
            | Expr::DictLit { span, .. }
            | Expr::RecordLit { span, .. } => *span,
        }
    }
}

/// Every callable there is.
///
/// The first group is `docs/grasp/semantics.md`'s builtin table — what a person
/// writes. The last three are what desugaring writes: they live in reserved
/// namespaces, and a program that spells one is writing the expansion by hand
/// rather than reaching for something new.
///
/// The reserved namespaces hold *no other* names. `string:length` parses as a
/// qualified identifier and resolves to nothing, because those namespaces are
/// held for a library that has not grown into them yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Abs,
    Floor,
    Ceil,
    Round,
    Length,
    Concat,
    Lower,
    Upper,
    Trim,
    /// `if(cond, a, b)` — the only branching construct.
    If,
    Keys,
    Entries,

    /// `record:get(s, "f")` — what `s.f` desugars to.
    RecordGet,
    /// `dict:get(d, "a")` — what a dict pattern desugars to.
    DictGet,
    /// `boolean:not(e)` — what `not e` desugars to in expression position.
    BooleanNot,
}

impl Builtin {
    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "abs" => Builtin::Abs,
            "floor" => Builtin::Floor,
            "ceil" => Builtin::Ceil,
            "round" => Builtin::Round,
            "length" => Builtin::Length,
            "concat" => Builtin::Concat,
            "lower" => Builtin::Lower,
            "upper" => Builtin::Upper,
            "trim" => Builtin::Trim,
            "if" => Builtin::If,
            "keys" => Builtin::Keys,
            "entries" => Builtin::Entries,
            "record:get" => Builtin::RecordGet,
            "dict:get" => Builtin::DictGet,
            "boolean:not" => Builtin::BooleanNot,
            _ => return None,
        })
    }

    /// Every callable, for the test that pins this list against the one
    /// `parse.rs` reserves and `semantics.md` documents.
    pub const ALL: &'static [Builtin] = &[
        Builtin::Abs,
        Builtin::Floor,
        Builtin::Ceil,
        Builtin::Round,
        Builtin::Length,
        Builtin::Concat,
        Builtin::Lower,
        Builtin::Upper,
        Builtin::Trim,
        Builtin::If,
        Builtin::Keys,
        Builtin::Entries,
        Builtin::RecordGet,
        Builtin::DictGet,
        Builtin::BooleanNot,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Builtin::Abs => "abs",
            Builtin::Floor => "floor",
            Builtin::Ceil => "ceil",
            Builtin::Round => "round",
            Builtin::Length => "length",
            Builtin::Concat => "concat",
            Builtin::Lower => "lower",
            Builtin::Upper => "upper",
            Builtin::Trim => "trim",
            Builtin::If => "if",
            Builtin::Keys => "keys",
            Builtin::Entries => "entries",
            Builtin::RecordGet => "record:get",
            Builtin::DictGet => "dict:get",
            Builtin::BooleanNot => "boolean:not",
        }
    }
}
