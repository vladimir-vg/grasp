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
/// **Every one is namespaced, and the namespace is the type family it belongs
/// to.** That is what makes each name monomorphic — `string:length` and
/// `array:length` are two functions, not one overloaded on its argument — and it
/// is why `record:get`, `dict:get` and `boolean:not` stopped being three
/// exceptions beside a dozen bare names.
///
/// `integer:` and `float:` do not merge into a `number:`. Float semantics are
/// not integer semantics, and `numeric` — arbitrary-precision decimal, when it
/// arrives — is a third thing again. `float:floor` exists and `integer:floor`
/// does not, being the identity on an `i64`.
///
/// A few are what desugaring writes rather than what a person does; each says
/// so. The rest are `docs/grasp/stdlib.grasp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `boolean:not(e)` — what `not e` desugars to in expression position.
    BooleanNot,

    IntegerAbs,

    FloatAbs,
    FloatFloor,
    FloatCeil,
    FloatRound,

    StringLength,
    /// What `a ++ b` desugars to.
    StringConcat,
    StringLower,
    StringUpper,
    StringTrim,

    ArrayLength,
    /// `array:at(a, index: i)` — an element by 0-based position.
    ///
    /// Access **by position** is `at` and access by key is `get`, throughout.
    ArrayAt,
    /// `array:drop(a, n)` — the elements from position `n` on.
    ///
    /// What an array pattern's `*r` binds, and not a name a program writes: the
    /// grasp-dbsp expression it becomes binds a name — `filter_array(a,
    /// function((e, i) -> i >= n))` — and grasp's expressions do not, so the
    /// binder is introduced at emission rather than carried through the middle
    /// of the compiler.
    ArrayDrop,

    DictLength,
    /// `dict:get(d, key: k)` — what `d[k]` and a dict pattern desugar to.
    DictGet,
    DictKeys,
    DictEntries,
    /// `dict:without_keys(d, ["a"])` — the entries a pattern did not name.
    ///
    /// An emission-time binder like [`Builtin::ArrayDrop`], for the same reason.
    DictWithoutKeys,

    /// `record:get(r, field: "f")` — what `r.f` desugars to.
    RecordGet,
}

impl Builtin {
    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "boolean:not" => Builtin::BooleanNot,
            "integer:abs" => Builtin::IntegerAbs,
            "float:abs" => Builtin::FloatAbs,
            "float:floor" => Builtin::FloatFloor,
            "float:ceil" => Builtin::FloatCeil,
            "float:round" => Builtin::FloatRound,
            "string:length" => Builtin::StringLength,
            "string:concat" => Builtin::StringConcat,
            "string:lower" => Builtin::StringLower,
            "string:upper" => Builtin::StringUpper,
            "string:trim" => Builtin::StringTrim,
            "array:length" => Builtin::ArrayLength,
            "array:at" => Builtin::ArrayAt,
            "array:drop" => Builtin::ArrayDrop,
            "dict:length" => Builtin::DictLength,
            "dict:get" => Builtin::DictGet,
            "dict:keys" => Builtin::DictKeys,
            "dict:entries" => Builtin::DictEntries,
            "dict:without_keys" => Builtin::DictWithoutKeys,
            "record:get" => Builtin::RecordGet,
            _ => return None,
        })
    }

    /// Every callable, for the test that pins this list against
    /// `docs/grasp/stdlib.grasp`.
    pub const ALL: &'static [Builtin] = &[
        Builtin::BooleanNot,
        Builtin::IntegerAbs,
        Builtin::FloatAbs,
        Builtin::FloatFloor,
        Builtin::FloatCeil,
        Builtin::FloatRound,
        Builtin::StringLength,
        Builtin::StringConcat,
        Builtin::StringLower,
        Builtin::StringUpper,
        Builtin::StringTrim,
        Builtin::ArrayLength,
        Builtin::ArrayAt,
        Builtin::ArrayDrop,
        Builtin::DictLength,
        Builtin::DictGet,
        Builtin::DictKeys,
        Builtin::DictEntries,
        Builtin::DictWithoutKeys,
        Builtin::RecordGet,
    ];

    /// Whether it can fail to have an answer.
    ///
    /// `dict:get` on a key that is not there and `array:at` past the end have
    /// none, and [a rule derives a row only where every step of its body has
    /// one][semantics] — so a call to one becomes a node, a filter and a
    /// rebinding, exactly as division does.
    ///
    /// [semantics]: ../../../docs/grasp/semantics.md
    pub fn partial(self) -> bool {
        matches!(self, Builtin::DictGet | Builtin::ArrayAt)
    }

    /// What grasp calls it.
    pub fn as_str(self) -> &'static str {
        match self {
            Builtin::BooleanNot => "boolean:not",
            Builtin::IntegerAbs => "integer:abs",
            Builtin::FloatAbs => "float:abs",
            Builtin::FloatFloor => "float:floor",
            Builtin::FloatCeil => "float:ceil",
            Builtin::FloatRound => "float:round",
            Builtin::StringLength => "string:length",
            Builtin::StringConcat => "string:concat",
            Builtin::StringLower => "string:lower",
            Builtin::StringUpper => "string:upper",
            Builtin::StringTrim => "string:trim",
            Builtin::ArrayLength => "array:length",
            Builtin::ArrayAt => "array:at",
            Builtin::ArrayDrop => "array:drop",
            Builtin::DictLength => "dict:length",
            Builtin::DictGet => "dict:get",
            Builtin::DictKeys => "dict:keys",
            Builtin::DictEntries => "dict:entries",
            Builtin::DictWithoutKeys => "dict:without_keys",
            Builtin::RecordGet => "record:get",
        }
    }

    /// What grasp-dbsp calls it, where the two are one call.
    ///
    /// `None` where emission is not a rename: `array:drop` and
    /// `dict:without_keys` become expressions that bind a name, and
    /// `record:get` becomes a field access. Those have their own arms in
    /// [`crate::emit`] and never reach the one that reads this.
    ///
    /// The two names differ because grasp chose its library rather than
    /// inheriting one: `float:floor` and `integer:abs` are grasp's names for
    /// things the target spells `floor` and `abs`, and `string:length`,
    /// `array:length` and `dict:length` are three functions over one.
    pub fn target(self) -> Option<&'static str> {
        match self {
            Builtin::BooleanNot => Some("not"),
            Builtin::IntegerAbs => Some("abs"),
            Builtin::FloatAbs => Some("abs"),
            Builtin::FloatFloor => Some("floor"),
            Builtin::FloatCeil => Some("ceil"),
            Builtin::FloatRound => Some("round"),
            Builtin::StringLength => Some("length"),
            Builtin::StringConcat => Some("concat"),
            Builtin::StringLower => Some("lower"),
            Builtin::StringUpper => Some("upper"),
            Builtin::StringTrim => Some("trim"),
            Builtin::ArrayLength => Some("length"),
            Builtin::ArrayAt => Some("get"),
            Builtin::ArrayDrop => None,
            Builtin::DictLength => Some("length"),
            Builtin::DictGet => Some("get"),
            Builtin::DictKeys => Some("keys"),
            Builtin::DictEntries => Some("entries"),
            Builtin::DictWithoutKeys => None,
            Builtin::RecordGet => None,
        }
    }
}
