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
    Fact(Fact),
    Rule(Rule),
}

impl Decl {
    pub fn span(&self) -> Span {
        match self {
            Decl::Spec(s) => s.span,
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
}
