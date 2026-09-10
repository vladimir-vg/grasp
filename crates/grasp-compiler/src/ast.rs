//! The AST — `docs/grasp/syntax.md`, "AST".
//!
//! The shape the rest of the pipeline consumes, and the last point at which
//! source order matters.
//!
//! Every node carries the source span it came from. **Nothing downstream is
//! allowed to key on source position**: spans exist for diagnostics and nothing
//! else, so that reformatting a program cannot change what it compiles to. The
//! test suite enforces that from the far end — `equivalent_to` compares the
//! emitted text of two programs that differ only in layout.
//!
//! The AST is untyped: a `Field` still carries a name rather than an index, and
//! a `Lit` an unresolved number. `docs/grasp/inference.md` resolves both.

use crate::diag::Span;
use std::fmt;

pub type Program = Vec<Decl>;

#[derive(Debug, Clone, PartialEq)]
pub enum Decl {
    Spec(Spec),
    Function(FnSpec),
    Fact(Fact),
    Rule(Rule),
}

impl Decl {
    pub fn span(&self) -> Span {
        match self {
            Decl::Spec(s) => s.span,
            Decl::Function(f) => f.span,
            Decl::Fact(f) => f.span,
            Decl::Rule(r) => r.span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub relation: String,
    pub columns: Vec<(String, Type)>,
    pub span: Span,
}

/// `array:slice :: function` and the variants indented under it — one typespec
/// per name, written in bulk.
///
/// grasp has **no user-defined functions**: this declares a name in the
/// standard library, which is why the name must sit under a reserved
/// namespace. `docs/grasp/stdlib.grasp` is the library written in this form.
///
/// It does not reach the core. What the compiler knows about a callable is
/// [`crate::core::Builtin`], and a spec is a second statement of it rather than
/// the source of it — so a spec in a program is inert, and the two are held
/// together by a test rather than by the pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct FnSpec {
    pub name: String,
    /// At least one, and no two of one [`Shape`].
    pub variants: Vec<Variant>,
    pub span: Span,
}

/// One way of calling a function: `(array(T), start: i64) -> array(T)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub positional: Vec<Type>,
    pub keyword: Vec<(String, Type)>,
    pub result: Type,
    pub span: Span,
}

impl Variant {
    /// What a call must match for this variant to be the one: its
    /// [`Shape`], and the types under it.
    ///
    /// Two variants of one shape are only a conflict when this agrees too —
    /// which is what lets `temporal:date` take a `string` and a `timestamp`
    /// under one name. Keywords are sorted, so the pair compares as the set a
    /// shape already is.
    pub fn signature(&self) -> (Shape, Vec<Type>, Vec<(String, Type)>) {
        let mut keyword = self.keyword.clone();
        keyword.sort_by(|a, b| a.0.cmp(&b.0));
        (self.shape(), self.positional.clone(), keyword)
    }

    pub fn shape(&self) -> Shape {
        Shape::new(
            self.positional.len(),
            self.keyword.iter().map(|(name, _)| name.clone()),
        )
    }
}

/// What identifies one variant of a function: how many arguments come by
/// position, and which keywords follow.
///
/// **Overloads are by shape, not by type** — Erlang's dispatch rather than
/// C++'s. Resolution picks the one variant whose shape matches the call and
/// only then checks the types, which is what lets `array:slice` carry eight
/// variants over `start:` `stop:` `step:` without a defaulting mechanism, and
/// what makes two variants of one shape a conflicting typespec rather than an
/// ambiguity to resolve by preferring one.
///
/// Keywords are a **set**: `f(a, x: 1, y: 2)` and `f(a, y: 2, x: 1)` are one
/// call. The order a variant writes them in is not part of its identity, and is
/// kept only by [`crate::core::Signature`], which uses it to put the arguments
/// in the order the core call holds them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Shape {
    pub positional: usize,
    /// Sorted, so that two shapes compare as the sets they are.
    pub keyword: Vec<String>,
}

impl Shape {
    pub fn new(positional: usize, keyword: impl IntoIterator<Item = String>) -> Shape {
        let mut keyword: Vec<String> = keyword.into_iter().collect();
        keyword.sort();
        Shape {
            positional,
            keyword,
        }
    }
}

impl fmt::Display for Shape {
    /// `(_, start:, stop:)` — a `_` for each argument by position, and each
    /// keyword by name. It is what a diagnostic quotes to say how a function is
    /// called, so it shows the call rather than the types.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        for i in 0..self.positional {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str("_")?;
        }
        for (i, name) in self.keyword.iter().enumerate() {
            if i > 0 || self.positional > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name}:")?;
        }
        f.write_str(")")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub relation: String,
    pub args: Vec<KvArg>,
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
    pub args: Vec<KvArg>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KvArg {
    pub column: String,
    pub value: Arg,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Expr(Expr),
    /// `_` — matches any value and binds nothing. Two in one atom are
    /// unrelated, so this carries no identity.
    Wildcard(Span),
    /// `total: sum<sal>` — an aggregate written where its result goes.
    ///
    /// Legal in a rule head and nowhere else, and gone by the end of
    /// desugaring: it means `total := sum<sal>` with the column naming the
    /// variable, which is the `total:` shorthand applied one step further.
    ///
    /// Held here rather than in [`Expr`] because an aggregate is still not an
    /// expression — nothing may write one in a filter, and this variant cannot
    /// reach one.
    Aggregate {
        function: Aggregator,
        /// `None` for `count<>`, which takes no argument.
        arg: Option<Expr>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Atom {
        relation: String,
        args: Vec<KvArg>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnnestKind {
    /// `(v) := *arr`
    Array,
    /// `(k, v) := **d`
    Dict,
}

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
    Array {
        elems: Vec<String>,
        rest: Rest,
        span: Span,
    },
    Dict {
        fields: Vec<(String, String)>,
        rest: Rest,
        span: Span,
    },
    Record {
        fields: Vec<(String, String)>,
        rest: Rest,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rest {
    None,
    /// `*` or `**` with no variable — ignore the remainder.
    Ignore,
    /// `*r` or `**r` — bind it.
    Bind(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rhs {
    Expr(Expr),
    /// `sum<e>`; `count<>` has no argument.
    Aggregate {
        function: Aggregator,
        arg: Option<Expr>,
        span: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregator {
    Sum,
    Count,
    Min,
    Max,
    Avg,
}

impl Aggregator {
    pub fn from_name(name: &str) -> Option<Aggregator> {
        match name {
            "sum" => Some(Aggregator::Sum),
            "count" => Some(Aggregator::Count),
            "min" => Some(Aggregator::Min),
            "max" => Some(Aggregator::Max),
            "avg" => Some(Aggregator::Avg),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    /// `NONE` — a value, not SQL NULL.
    None,
}

/// The operator groups of `docs/grasp/syntax.md`, "Operator groups".
///
/// This is not decoration: precedence is defined *within* a group, and between
/// groups only three relations hold. The parser carries the group of the
/// operator it just consumed so it can reject an incomparable neighbour, which
/// a plain binding-power table cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Arithmetic,
    Concatenation,
    Comparison,
    Boolean,
}

impl Group {
    pub fn as_str(self) -> &'static str {
        match self {
            Group::Arithmetic => "arithmetic",
            Group::Concatenation => "concatenation",
            Group::Comparison => "comparison",
            Group::Boolean => "boolean",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

impl UnOp {
    pub fn group(self) -> Group {
        match self {
            UnOp::Neg => Group::Arithmetic,
            UnOp::Not => Group::Boolean,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "not",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn group(self) -> Group {
        match self {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => Group::Arithmetic,
            BinOp::Concat => Group::Concatenation,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                Group::Comparison
            }
            BinOp::And | BinOp::Or => Group::Boolean,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Concat => "++",
            BinOp::Eq => "=",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }
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
    Field {
        base: Box<Expr>,
        name: String,
        span: Span,
    },
    /// `d[k]` and `arr[i]` — a read by key or by position.
    ///
    /// The postfix twin of [`Expr::Field`], but unlike it **not** sugar: which
    /// of `dict:get` and `array:at` it means is decided by the subject's type,
    /// so it survives desugaring and is settled in `infer`.
    Index {
        base: Box<Expr>,
        key: Box<Expr>,
        span: Span,
    },
    /// `arr[1:5:2]` — a slice, and the one subscript that is not a lookup.
    ///
    /// Each part may be left out: `arr[:5]`, `arr[2:]`, `arr[::2]`, `arr[:]`.
    /// Unlike [`Expr::Index`] it needs no type to be understood — a dict cannot
    /// be sliced — so this *is* sugar, and desugaring rewrites it to
    /// `array:slice` with the keywords the parts were written under.
    Slice {
        base: Box<Expr>,
        start: Option<Box<Expr>>,
        stop: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
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
        name: String,
        positional: Vec<Expr>,
        keyword: Vec<(String, Expr)>,
        span: Span,
    },
    ArrayLit {
        elems: Vec<Expr>,
        span: Span,
    },
    /// Entries are `(key, value)`. Both dict spellings arrive here; the `:`
    /// form's key is a string literal, which is what makes `{a: v}` *be*
    /// `{"a" => v}` once `docs/grasp/semantics.md`'s desugaring has run.
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
            | Expr::Field { span, .. }
            | Expr::Index { span, .. }
            | Expr::Slice { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::ArrayLit { span, .. }
            | Expr::DictLit { span, .. }
            | Expr::RecordLit { span, .. } => *span,
        }
    }

    /// The operator group this expression sits in, for the mixing check.
    /// A primary belongs to no group and can neighbour anything.
    pub fn group(&self) -> Option<Group> {
        match self {
            Expr::Unary { op, .. } => Some(op.group()),
            Expr::Binary { op, .. } => Some(op.group()),
            _ => None,
        }
    }
}

impl fmt::Display for Type {
    /// The canonical spelling of a type, which is what a diagnostic quotes.
    ///
    /// Record fields are sorted by name, because `types.md` says they are a set
    /// — `record(a: i64, b: string)` and `record(b: string, a: i64)` are one
    /// type, and a message that spelled them differently would be reporting the
    /// order rather than the type.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Boolean => f.write_str("boolean"),
            Type::I64 => f.write_str("i64"),
            Type::F64 => f.write_str("f64"),
            Type::String => f.write_str("string"),
            Type::Json => f.write_str("json"),
            Type::Date => f.write_str("date"),
            Type::Time => f.write_str("time"),
            Type::Timestamp => f.write_str("timestamp"),
            Type::Var(name) => f.write_str(name),
            Type::Optional(t) => write!(f, "optional({t})"),
            Type::Array(t) => write!(f, "array({t})"),
            Type::Dict(k, v) => write!(f, "dict({k}, {v})"),
            Type::Record(fields) => {
                let mut sorted: Vec<&(String, Type)> = fields.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                f.write_str("record(")?;
                for (i, (name, ty)) in sorted.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                f.write_str(")")
            }
        }
    }
}

/// `docs/grasp/types.md`. `relation(...)` is not here: it is not a value type,
/// and it appears only in a spec.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Boolean,
    I64,
    F64,
    String,
    Json,
    Optional(Box<Type>),
    Record(Vec<(String, Type)>),
    Array(Box<Type>),
    Dict(Box<Type>, Box<Type>),
    /// A calendar date — no time, no zone.
    Date,
    /// A time of day — no date, no zone.
    Time,
    /// An instant, always UTC. There is no zone-carrying form; see
    /// `overview.md`.
    Timestamp,
    /// `T` — a type variable, and the one thing here that is not a type.
    ///
    /// Legal in a [function typespec][`FnSpec`] and nowhere else: it says that
    /// `array:at`'s result is the array's element type without naming one.
    /// Nothing below the parser meets it, because a typespec does not reach the
    /// core.
    Var(String),
}
