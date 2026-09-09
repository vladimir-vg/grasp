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
    /// `[x, y, …] := arr` — the variables it binds, by position, and what it
    /// says about the elements past them.
    ///
    /// Positional, so unlike a destructure over names these are *not* sorted:
    /// `[x, y]` and `[y, x]` bind differently and are two patterns.
    Array {
        elems: Vec<String>,
        rest: Rest,
        span: Span,
    },
    /// `{a: x, …} := d` — the keys it names, paired with the variables they
    /// bind, and what it says about the ones it does not name.
    ///
    /// Kept rather than desugared, for the reason `Unnest` is: the expansion
    /// needs types. `{a: x}` means "get the entry and drop the row when it is
    /// absent", and the narrowing that drops it has to name the dict's value
    /// type, which desugaring does not know. See `plan`, which expands it once
    /// inference has.
    Dict {
        fields: Vec<(String, String)>,
        rest: Rest,
        span: Span,
    },
    /// `record(a: x, …) := r` — the same, over a record.
    ///
    /// A record's fields are its *type*, so this asks nothing at runtime: with
    /// no rest it is the claim that `r` has exactly these fields, and with one
    /// it is the claim that it has at least them. Both are checked in `infer`,
    /// and a row is never dropped for failing them.
    Record {
        fields: Vec<(String, String)>,
        rest: Rest,
        span: Span,
    },
}

pub use crate::ast::{Rest, UnnestKind};

/// The variable a `*r` or `**r` binds, where there is one.
fn rest_binding(rest: &Rest) -> Option<&str> {
    match rest {
        Rest::Bind(v) => Some(v.as_str()),
        _ => None,
    }
}

impl Pattern {
    /// The variables this pattern binds.
    ///
    /// An unnest's are positional and a destructure's are not, but for *what is
    /// bound* the distinction does not matter, and three passes wanted the same
    /// answer.
    pub fn binds(&self) -> Vec<&str> {
        match self {
            Pattern::Var { name, .. } => vec![name],
            Pattern::Unnest { vars, .. } => vars.iter().map(String::as_str).collect(),
            Pattern::Dict { fields, rest, .. } | Pattern::Record { fields, rest, .. } => fields
                .iter()
                .map(|(_, v)| v.as_str())
                .chain(rest_binding(rest))
                .collect(),
            Pattern::Array { elems, rest, .. } => elems
                .iter()
                .map(String::as_str)
                .chain(rest_binding(rest))
                .collect(),
        }
    }
}

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
    /// `dict:get(d, k)` — what `d[k]` and a dict pattern desugar to.
    ///
    /// The one of the three with no name a program can write: `d[k]` is the
    /// spelling, and giving it a second one would be giving grasp the computed
    /// dict lookup [`semantics.md`] holds back, unchecked, under a name nothing
    /// documents. `record:get` and `boolean:not` keep theirs, being the written
    /// halves of `.` and `not`.
    ///
    /// [`semantics.md`]: ../../../docs/grasp/semantics.md
    DictGet,
    /// `boolean:not(e)` — what `not e` desugars to in expression position.
    BooleanNot,
    /// `array:get(a, i)` — an element by 0-based position, `optional(E)`.
    ///
    /// What an array pattern reads each of its variables with. Not a name a
    /// program can write, for `dict:get`'s reason: grasp has no element-access
    /// syntax yet, and a namespaced spelling would be one by the back door.
    ArrayGet,
    /// `array:drop(a, n)` — the elements from position `n` on.
    ///
    /// What an array pattern's `*r` binds. It exists because the grasp-dbsp
    /// expression it becomes binds a name — `filter_array(a, function((e, i) ->
    /// i >= n))` — and grasp's expressions do not, so the binder is introduced
    /// at emission rather than carried through the middle of the compiler.
    ArrayDrop,
    /// `dict:without_keys(d, ["a"])` — the entries the pattern did not name.
    ///
    /// What a dict pattern's `**e` binds, and an emission-time binder like
    /// [`Builtin::ArrayDrop`]: it becomes a `filter_array` over the dict's
    /// entries, rebuilt with `dict`.
    DictWithoutKeys,
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
        Builtin::ArrayGet,
        Builtin::ArrayDrop,
        Builtin::DictWithoutKeys,
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
            Builtin::ArrayGet => "array:get",
            Builtin::ArrayDrop => "array:drop",
            Builtin::DictWithoutKeys => "dict:without_keys",
            Builtin::BooleanNot => "boolean:not",
        }
    }
}
