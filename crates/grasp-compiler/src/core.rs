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

use crate::ast::{Aggregator, BinOp, Lit, Shape, Type, UnOp};
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
    /// `d[k]` and `arr[i]` — a read by key or by position.
    ///
    /// Kept, for the reason [`Pattern::Dict`] is: the rewrite needs types.
    /// Which of `dict:get` and `array:at` a subscript means is decided by the
    /// subject's type, and desugaring has none — so this survives into the core
    /// and `infer` settles it, the way `plan` settles a destructure. Nothing
    /// below `infer` meets one.
    Index {
        base: Box<Expr>,
        key: Box<Expr>,
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
            | Expr::Index { span, .. }
            | Expr::ArrayLit { span, .. }
            | Expr::DictLit { span, .. }
            | Expr::RecordLit { span, .. } => *span,
        }
    }
}

/// The standard library grasp has designed and this compiler has not got.
///
/// `docs/grasp/stdlib.grasp` declares the **whole** library, so a call to one of
/// these is `not implemented: <name>` rather than "there is no callable": the
/// language has the function and the compiler has not caught up. The difference
/// then shows in the test suite's burn-down, as a work queue rather than as a
/// promise in a document nobody counts.
///
/// Sorted, and every entry is a name in that file — `tests/reserved.rs` checks
/// both halves of that, so a function implemented without leaving this list is
/// a failing test rather than a name reported unimplemented after it works.
pub const DESIGNED: &[&str] = &[
    "array:append",
    "array:concat",
    "array:distinct",
    "array:flatten",
    "array:index_of",
    "array:max",
    "array:min",
    "array:prepend",
    "array:reverse",
    "array:sort",
    "dict:merge",
    "float:max",
    "float:min",
    "float:pow",
    "float:sign",
    "float:sqrt",
    "integer:max",
    "integer:min",
    "integer:pow",
    "integer:sign",
    "string:at",
    "string:contains",
    "string:ends_with",
    "string:index_of",
    "string:ltrim",
    "string:replace",
    "string:rtrim",
    "string:slice",
    "string:split",
    "string:starts_with",
];

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
    /// `array:contains(a, element: x)` — whether the array holds it.
    ArrayContains,
    /// `array:slice(a, start:, stop:, step:)` — Python's slice.
    ///
    /// The one function in the library with several variants, and the reason
    /// [`Signature`] carries defaults: three optional keywords are eight ways
    /// to call it, and every one lowers to the same four-argument target call.
    ///
    /// **Total**, unlike the other ways of reading an array: the bounds clamp
    /// rather than fail, so `arr[0:99]` on a three-element array is those three
    /// and the row is derived.
    ArraySlice,

    DictLength,
    /// `dict:get(d, key: k)` — what `d[k]` and a dict pattern desugar to.
    DictGet,
    DictKeys,
    /// `dict:values(d)` — the other column of [`Builtin::DictEntries`].
    DictValues,
    DictEntries,
    /// `dict:has(d, key: k)` — whether the key is there.
    ///
    /// The way to ask about absence, since [`Builtin::DictGet`] answers by not
    /// deriving the row. "Absence is asked for, not unwrapped everywhere."
    DictHas,
    /// `dict:from_entries(a)` — the inverse of [`Builtin::DictEntries`].
    DictFromEntries,
    /// `dict:without(d, keys: ks)` — the entries whose keys are not in `ks`.
    ///
    /// [`Builtin::DictWithoutKeys`] is the same idea over the *literal* keys a
    /// pattern named. This one takes an array the data supplies, which is what
    /// the target's `contains` made expressible.
    DictWithout,
    /// `dict:without_keys(d, ["a"])` — the entries a pattern did not name.
    ///
    /// An emission-time binder like [`Builtin::ArrayDrop`], for the same reason.
    DictWithoutKeys,

    /// `temporal:date`, `temporal:time`, `temporal:timestamp` — the names a
    /// program writes, and **families**: several functions share each, so what
    /// a call means is settled by its shape and, where two shapes coincide, by
    /// its argument type.
    ///
    /// None survives `infer`, which rewrites each into the variant below that
    /// it selects.
    TemporalDate,
    TemporalTime,
    TemporalTimestamp,

    /// Parse. **Partial**: `2024-13-45` is not a date, so the row is not
    /// derived — the rule `dict:get` follows.
    TemporalParseDate,
    TemporalParseTime,
    TemporalParseTimestamp,
    /// The two halves of an instant. Total: a timestamp always has both.
    TemporalDateOf,
    TemporalTimeOf,
    /// From components. **Partial**: not every triple of integers is a date.
    TemporalMakeDate,
    TemporalMakeTime,
    /// A date and a time-of-day as one instant, and an instant from the epoch.
    /// Both total.
    TemporalMakeTimestamp,
    TemporalFromMicros,

    /// The components, each over **either type that holds it**: `years` a
    /// `date` or a `timestamp`, `hours` a `time` or one. Plural, because the
    /// answer is a count of them.
    ///
    /// The target's are polymorphic the way `length` is over string, array and
    /// dict, so this is one variant per component rather than a family.
    TemporalYear,
    TemporalMonth,
    TemporalDay,
    TemporalHour,
    TemporalMinute,
    TemporalSecond,
    TemporalMicrosecond,

    /// `temporal:epoch_micros(ts)` — the instant as an integer.
    TemporalEpochMicros,

    /// `temporal:interval(days:, hours:, minutes:, seconds:, microseconds:)` —
    /// a span of time. Every unit converts into microseconds exactly, which is
    /// what lets one type hold all five.
    TemporalInterval,
    /// The span in whole units of one size. Named apart from the components
    /// because they answer a different question: `temporal:minutes(14:30)` is
    /// 30, and `temporal:total_minutes` of a hundred hours is 6000.
    TemporalTotalDays,
    TemporalTotalHours,
    TemporalTotalMinutes,
    TemporalTotalSeconds,
    TemporalTotalMicroseconds,

    /// `record:get(r, field: "f")` — what `r.f` desugars to.
    ///
    /// Not a name a program may write, and the reason is its type: the result is
    /// the *named field's*, which depends on the value of an argument rather
    /// than on its type. No signature says that, so [`crate::infer`] reads the
    /// field literal — and a call given anything but a literal has no answer.
    /// `r.f` is the spelling, and the only one.
    RecordGet,
}

impl Builtin {
    /// The callable a program's `name(` means.
    ///
    /// [`Builtin::ArrayDrop`] and [`Builtin::DictWithoutKeys`] are not here:
    /// they are what desugaring writes and are not names a program may take.
    /// See [`Builtin::callable`].
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
            "array:contains" => Builtin::ArrayContains,
            "array:slice" => Builtin::ArraySlice,
            "dict:length" => Builtin::DictLength,
            "dict:get" => Builtin::DictGet,
            "dict:has" => Builtin::DictHas,
            "dict:keys" => Builtin::DictKeys,
            "dict:values" => Builtin::DictValues,
            "dict:entries" => Builtin::DictEntries,
            "dict:from_entries" => Builtin::DictFromEntries,
            "dict:without" => Builtin::DictWithout,
            "temporal:date" => Builtin::TemporalDate,
            "temporal:time" => Builtin::TemporalTime,
            "temporal:timestamp" => Builtin::TemporalTimestamp,
            "temporal:years" => Builtin::TemporalYear,
            "temporal:months" => Builtin::TemporalMonth,
            "temporal:days" => Builtin::TemporalDay,
            "temporal:hours" => Builtin::TemporalHour,
            "temporal:minutes" => Builtin::TemporalMinute,
            "temporal:seconds" => Builtin::TemporalSecond,
            "temporal:microseconds" => Builtin::TemporalMicrosecond,
            "temporal:epoch_micros" => Builtin::TemporalEpochMicros,
            "temporal:interval" => Builtin::TemporalInterval,
            "temporal:total_days" => Builtin::TemporalTotalDays,
            "temporal:total_hours" => Builtin::TemporalTotalHours,
            "temporal:total_minutes" => Builtin::TemporalTotalMinutes,
            "temporal:total_seconds" => Builtin::TemporalTotalSeconds,
            "temporal:total_microseconds" => Builtin::TemporalTotalMicroseconds,
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
        Builtin::ArrayContains,
        Builtin::ArraySlice,
        Builtin::DictLength,
        Builtin::DictGet,
        Builtin::DictHas,
        Builtin::DictKeys,
        Builtin::DictValues,
        Builtin::DictEntries,
        Builtin::DictFromEntries,
        Builtin::DictWithout,
        Builtin::DictWithoutKeys,
        Builtin::TemporalDate,
        Builtin::TemporalTime,
        Builtin::TemporalTimestamp,
        Builtin::TemporalParseDate,
        Builtin::TemporalParseTime,
        Builtin::TemporalParseTimestamp,
        Builtin::TemporalDateOf,
        Builtin::TemporalTimeOf,
        Builtin::TemporalMakeDate,
        Builtin::TemporalMakeTime,
        Builtin::TemporalMakeTimestamp,
        Builtin::TemporalFromMicros,
        Builtin::TemporalYear,
        Builtin::TemporalMonth,
        Builtin::TemporalDay,
        Builtin::TemporalHour,
        Builtin::TemporalMinute,
        Builtin::TemporalSecond,
        Builtin::TemporalMicrosecond,
        Builtin::TemporalEpochMicros,
        Builtin::TemporalInterval,
        Builtin::TemporalTotalDays,
        Builtin::TemporalTotalHours,
        Builtin::TemporalTotalMinutes,
        Builtin::TemporalTotalSeconds,
        Builtin::TemporalTotalMicroseconds,
        Builtin::RecordGet,
    ];

    /// Every way of calling it.
    ///
    /// **Subject positional, everything else keyword** — `array:at(a, index: 2)`
    /// rather than `array:at(a, 2)`, and `dict:without(d, keys: ks)`. A
    /// same-type binary operation like `string:concat` keeps both arguments
    /// positional, having no subject to distinguish.
    ///
    /// This is what a call is resolved against: desugaring matches the call's
    /// [`Shape`] to one of these and reorders its arguments into the order the
    /// signature writes, so that below the core a call is positional and every
    /// pass after can index it.
    ///
    /// One signature each, for now. The list is here rather than as an arity
    /// number because `array:slice` will have eight — the same name over
    /// `start:` `stop:` `step:`, which is what makes a defaulting mechanism
    /// unnecessary.
    pub fn signatures(self) -> &'static [Signature] {
        const UNARY: &[Signature] = &[Signature {
            positional: 1,
            keyword: &[],
            selects: None,
        }];
        const BINARY: &[Signature] = &[Signature {
            positional: 2,
            keyword: &[],
            selects: None,
        }];
        match self {
            Builtin::BooleanNot
            | Builtin::IntegerAbs
            | Builtin::FloatAbs
            | Builtin::FloatFloor
            | Builtin::FloatCeil
            | Builtin::FloatRound
            | Builtin::StringLength
            | Builtin::StringLower
            | Builtin::StringUpper
            | Builtin::StringTrim
            | Builtin::ArrayLength
            | Builtin::DictLength
            | Builtin::DictKeys
            | Builtin::DictValues
            | Builtin::DictEntries
            | Builtin::DictFromEntries => UNARY,
            Builtin::StringConcat => BINARY,
            // The two desugaring writes and a program does not. Their arguments
            // are positional because nothing spells them: emission builds the
            // core call directly.
            Builtin::ArrayDrop | Builtin::DictWithoutKeys => BINARY,
            Builtin::ArrayAt => &[Signature {
                positional: 1,
                keyword: &[Param {
                    name: "index",
                    default: None,
                }],
                selects: None,
            }],
            Builtin::ArrayContains => &[Signature {
                positional: 1,
                keyword: &[Param {
                    name: "element",
                    default: None,
                }],
                selects: None,
            }],
            // Three optional keywords, and so eight ways to call it — the eight
            // variants `stdlib.grasp` writes out.
            Builtin::ArraySlice => &[Signature {
                positional: 1,
                keyword: &[
                    Param {
                        name: "start",
                        default: Some(Omitted::Absent),
                    },
                    Param {
                        name: "stop",
                        default: Some(Omitted::Absent),
                    },
                    Param {
                        name: "step",
                        default: Some(Omitted::Int(1)),
                    },
                ],
                selects: None,
            }],
            Builtin::DictGet | Builtin::DictHas => &[Signature {
                positional: 1,
                keyword: &[Param {
                    name: "key",
                    default: None,
                }],
                selects: None,
            }],
            Builtin::DictWithout => &[Signature {
                positional: 1,
                keyword: &[Param {
                    name: "keys",
                    default: None,
                }],
                selects: None,
            }],
            Builtin::RecordGet => &[Signature {
                positional: 1,
                keyword: &[Param {
                    name: "field",
                    default: None,
                }],
                selects: None,
            }],

            // The three families. Each shape names the function it means, and
            // the two that coincide — `(_)` over a string and over a timestamp
            // — are what the argument types settle.
            Builtin::TemporalDate => &[
                Signature {
                    positional: 1,
                    keyword: &[],
                    selects: Some(Builtin::TemporalParseDate),
                },
                Signature {
                    positional: 1,
                    keyword: &[],
                    selects: Some(Builtin::TemporalDateOf),
                },
                Signature {
                    positional: 0,
                    keyword: &[
                        Param {
                            name: "year",
                            default: None,
                        },
                        Param {
                            name: "month",
                            default: None,
                        },
                        Param {
                            name: "day",
                            default: None,
                        },
                    ],
                    selects: Some(Builtin::TemporalMakeDate),
                },
            ],
            Builtin::TemporalTime => &[
                Signature {
                    positional: 1,
                    keyword: &[],
                    selects: Some(Builtin::TemporalParseTime),
                },
                Signature {
                    positional: 1,
                    keyword: &[],
                    selects: Some(Builtin::TemporalTimeOf),
                },
                Signature {
                    positional: 0,
                    keyword: &[
                        Param {
                            name: "hour",
                            default: None,
                        },
                        Param {
                            name: "minute",
                            default: None,
                        },
                        Param {
                            name: "second",
                            default: Some(Omitted::Int(0)),
                        },
                        Param {
                            name: "microsecond",
                            default: Some(Omitted::Int(0)),
                        },
                    ],
                    selects: Some(Builtin::TemporalMakeTime),
                },
            ],
            // No two shapes coincide here, so this family is settled by shape
            // alone and never reaches the type dispatch.
            Builtin::TemporalTimestamp => &[
                Signature {
                    positional: 1,
                    keyword: &[],
                    selects: Some(Builtin::TemporalParseTimestamp),
                },
                Signature {
                    positional: 0,
                    keyword: &[
                        Param {
                            name: "date",
                            default: None,
                        },
                        Param {
                            name: "time",
                            default: None,
                        },
                    ],
                    selects: Some(Builtin::TemporalMakeTimestamp),
                },
                Signature {
                    positional: 0,
                    keyword: &[Param {
                        name: "epoch_microseconds",
                        default: None,
                    }],
                    selects: Some(Builtin::TemporalFromMicros),
                },
            ],

            Builtin::TemporalYear
            | Builtin::TemporalMonth
            | Builtin::TemporalDay
            | Builtin::TemporalHour
            | Builtin::TemporalMinute
            | Builtin::TemporalSecond
            | Builtin::TemporalMicrosecond
            | Builtin::TemporalEpochMicros => UNARY,
            Builtin::TemporalTotalDays
            | Builtin::TemporalTotalHours
            | Builtin::TemporalTotalMinutes
            | Builtin::TemporalTotalSeconds
            | Builtin::TemporalTotalMicroseconds => UNARY,
            // Five optional keywords, so thirty-two ways to call it — one line
            // in `stdlib.grasp`, since a typespec can say a default.
            Builtin::TemporalInterval => &[Signature {
                positional: 0,
                keyword: &[
                    Param {
                        name: "days",
                        default: Some(Omitted::Int(0)),
                    },
                    Param {
                        name: "hours",
                        default: Some(Omitted::Int(0)),
                    },
                    Param {
                        name: "minutes",
                        default: Some(Omitted::Int(0)),
                    },
                    Param {
                        name: "seconds",
                        default: Some(Omitted::Int(0)),
                    },
                    Param {
                        name: "microseconds",
                        default: Some(Omitted::Int(0)),
                    },
                ],
                selects: None,
            }],

            // What a family resolves to. A program reaches these through the
            // head, so their own shape is the one the head's row already
            // matched — written out because `signatures` must answer for every
            // variant, and because emission reads the arity.
            Builtin::TemporalParseDate
            | Builtin::TemporalParseTime
            | Builtin::TemporalParseTimestamp
            | Builtin::TemporalDateOf
            | Builtin::TemporalTimeOf
            | Builtin::TemporalFromMicros => UNARY,
            Builtin::TemporalMakeDate => &[Signature {
                positional: 3,
                keyword: &[],
                selects: None,
            }],
            Builtin::TemporalMakeTime => &[Signature {
                positional: 4,
                keyword: &[],
                selects: None,
            }],
            Builtin::TemporalMakeTimestamp => BINARY,
        }
    }

    /// Every signature a call of this shape could mean.
    ///
    /// One for all but a family, and for a family one per candidate — which is
    /// where the argument types have to decide, in `infer`. Same-shape
    /// signatures agree on the order of their keywords, so the *arguments* are
    /// ordered by shape whether or not the callee is settled.
    pub fn resolve(self, shape: &Shape) -> Vec<&'static Signature> {
        self.signatures()
            .iter()
            .filter(|s| s.accepts(shape))
            .collect()
    }

    /// The candidates a family stands for, in the order `infer` should try them.
    ///
    /// Empty for a name that is its own implementation.
    pub fn family(self) -> Vec<Builtin> {
        self.signatures().iter().filter_map(|s| s.selects).collect()
    }

    /// Whether a program may write this name.
    ///
    /// Three are written by desugaring and never by a person: [`Builtin::ArrayDrop`]
    /// is what an array pattern's `*r` binds, [`Builtin::DictWithoutKeys`] what
    /// a dict pattern's `**r` does, and [`Builtin::RecordGet`] what `r.f` does.
    /// None is in the library, and [`Builtin::from_name`] does not resolve them.
    ///
    /// What they share is an argument **no source syntax can supply**: each
    /// takes a literal — an array of the keys a pattern named, a string naming
    /// a field — and is useless given anything else. A callable that accepts
    /// only a literal is syntax wearing a function's clothes, and grasp already
    /// has the syntax.
    pub fn callable(self) -> bool {
        !matches!(
            self,
            Builtin::ArrayDrop
                | Builtin::DictWithoutKeys
                | Builtin::RecordGet
                // What a family head resolves to. The *name* is writable; these
                // are which function it turned out to mean.
                | Builtin::TemporalParseDate
                | Builtin::TemporalParseTime
                | Builtin::TemporalParseTimestamp
                | Builtin::TemporalDateOf
                | Builtin::TemporalTimeOf
                | Builtin::TemporalMakeDate
                | Builtin::TemporalMakeTime
                | Builtin::TemporalMakeTimestamp
                | Builtin::TemporalFromMicros
        )
    }

    /// Whether it can fail to have an answer.
    ///
    /// `dict:get` on a key that is not there and `array:at` past the end have
    /// none, and [a rule derives a row only where every step of its body has
    /// one][semantics] — so a call to one becomes a node, a filter and a
    /// rebinding, exactly as division does.
    ///
    /// [semantics]: ../../../docs/grasp/semantics.md
    pub fn partial(self) -> bool {
        matches!(
            self,
            Builtin::DictGet
                | Builtin::ArrayAt
                // Text that is not a date has no answer, and neither does a
                // February 31st. Extraction from an instant always has one,
                // which is why parsing and extracting are two variants rather
                // than one name that is sometimes partial.
                | Builtin::TemporalParseDate
                | Builtin::TemporalParseTime
                | Builtin::TemporalParseTimestamp
                | Builtin::TemporalMakeDate
                | Builtin::TemporalMakeTime
        )
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
            Builtin::ArrayContains => "array:contains",
            Builtin::ArraySlice => "array:slice",
            Builtin::ArrayDrop => "array:drop",
            Builtin::DictLength => "dict:length",
            Builtin::DictGet => "dict:get",
            Builtin::DictHas => "dict:has",
            Builtin::DictKeys => "dict:keys",
            Builtin::DictValues => "dict:values",
            Builtin::DictEntries => "dict:entries",
            Builtin::DictFromEntries => "dict:from_entries",
            Builtin::DictWithout => "dict:without",
            Builtin::DictWithoutKeys => "dict:without_keys",
            // An implementation answers to the name it was reached through: a
            // diagnostic says `temporal:date`, which is what the program wrote.
            Builtin::TemporalDate
            | Builtin::TemporalParseDate
            | Builtin::TemporalDateOf
            | Builtin::TemporalMakeDate => "temporal:date",
            Builtin::TemporalTime
            | Builtin::TemporalParseTime
            | Builtin::TemporalTimeOf
            | Builtin::TemporalMakeTime => "temporal:time",
            Builtin::TemporalTimestamp
            | Builtin::TemporalParseTimestamp
            | Builtin::TemporalMakeTimestamp
            | Builtin::TemporalFromMicros => "temporal:timestamp",
            Builtin::TemporalYear => "temporal:years",
            Builtin::TemporalMonth => "temporal:months",
            Builtin::TemporalDay => "temporal:days",
            Builtin::TemporalHour => "temporal:hours",
            Builtin::TemporalMinute => "temporal:minutes",
            Builtin::TemporalSecond => "temporal:seconds",
            Builtin::TemporalMicrosecond => "temporal:microseconds",
            Builtin::TemporalEpochMicros => "temporal:epoch_micros",
            Builtin::TemporalInterval => "temporal:interval",
            Builtin::TemporalTotalDays => "temporal:total_days",
            Builtin::TemporalTotalHours => "temporal:total_hours",
            Builtin::TemporalTotalMinutes => "temporal:total_minutes",
            Builtin::TemporalTotalSeconds => "temporal:total_seconds",
            Builtin::TemporalTotalMicroseconds => "temporal:total_microseconds",
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
            Builtin::ArrayContains => Some("contains"),
            Builtin::ArraySlice => Some("slice"),
            Builtin::ArrayDrop => None,
            Builtin::DictLength => Some("length"),
            Builtin::DictGet => Some("get"),
            Builtin::DictHas => None,
            Builtin::DictKeys => Some("keys"),
            Builtin::DictValues => None,
            Builtin::DictEntries => Some("dict_entries"),
            Builtin::DictFromEntries => Some("dict"),
            Builtin::DictWithout => None,
            Builtin::DictWithoutKeys => None,
            // The three heads never reach emission: `infer` settles them.
            Builtin::TemporalDate | Builtin::TemporalTime | Builtin::TemporalTimestamp => None,
            // Parsing and the two halves of an instant are `cast` in the
            // target, which is not a call — so each has its own emitter arm.
            Builtin::TemporalParseDate
            | Builtin::TemporalParseTime
            | Builtin::TemporalParseTimestamp
            | Builtin::TemporalDateOf
            | Builtin::TemporalTimeOf => None,
            Builtin::TemporalMakeDate => Some("make_date"),
            Builtin::TemporalMakeTime => Some("make_time"),
            Builtin::TemporalMakeTimestamp => Some("make_timestamp"),
            Builtin::TemporalFromMicros => Some("timestamp_from_micros"),
            Builtin::TemporalYear => Some("year"),
            Builtin::TemporalMonth => Some("month"),
            Builtin::TemporalDay => Some("day"),
            Builtin::TemporalHour => Some("hour"),
            Builtin::TemporalMinute => Some("minute"),
            Builtin::TemporalSecond => Some("second"),
            Builtin::TemporalMicrosecond => Some("microsecond"),
            Builtin::TemporalEpochMicros => Some("epoch_micros"),
            // The five components are summed into microseconds, so this becomes
            // an expression rather than another call.
            Builtin::TemporalInterval => None,
            Builtin::TemporalTotalDays => Some("total_days"),
            Builtin::TemporalTotalHours => Some("total_hours"),
            Builtin::TemporalTotalMinutes => Some("total_minutes"),
            Builtin::TemporalTotalSeconds => Some("total_seconds"),
            Builtin::TemporalTotalMicroseconds => Some("total_microseconds"),
            Builtin::RecordGet => None,
        }
    }
}

/// One way of calling a [`Builtin`]: how many arguments come by position, and
/// which keywords follow, **in the order the core call holds them**.
///
/// The order is the one thing a [`Shape`] does not carry, and the first reason
/// this type exists beside it: a shape says whether a call resolves here, and a
/// signature says where each argument goes once it has.
///
/// The second is that a signature may cover **several** shapes. A parameter
/// with a default may be left out, so `array:slice`'s three optional keywords
/// are the eight variants `stdlib.grasp` writes out — written once here, and
/// enumerated by [`Signature::shapes`] for the test that compares the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    pub positional: usize,
    pub keyword: &'static [Param],
    /// The [`Builtin`] a call matching this shape means, where that is not the
    /// name itself.
    ///
    /// `None` for every function that is its own implementation, which is all
    /// but a **family** — a name several functions share. `temporal:date` is
    /// one: parsing a string and extracting from a timestamp are two operations
    /// under one name, and a row here says which is which.
    pub selects: Option<Builtin>,
}

/// One keyword parameter, and what it means when a call leaves it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Param {
    pub name: &'static str,
    /// `None` where the call must supply it. A parameter's being optional is
    /// what makes a second shape, so this is where the variants come from.
    pub default: Option<Omitted>,
}

/// What a parameter means when a call leaves it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Omitted {
    /// `NONE`. A slice's missing bound is this rather than a number, because
    /// *which* end it means depends on the sign of the step — there is no
    /// constant to write, which is why Python's own slice carries `None` there.
    Absent,
    Int(i64),
}

impl Omitted {
    pub fn literal(self) -> Lit {
        match self {
            Omitted::Absent => Lit::None,
            Omitted::Int(n) => Lit::Int(n),
        }
    }
}

impl Signature {
    /// Whether a call of this shape resolves here: the same count by position,
    /// no keyword this does not take, and every required one given.
    pub fn accepts(&self, shape: &Shape) -> bool {
        shape.positional == self.positional
            && shape
                .keyword
                .iter()
                .all(|given| self.keyword.iter().any(|p| p.name == given))
            && self
                .keyword
                .iter()
                .filter(|p| p.default.is_none())
                .all(|p| shape.keyword.iter().any(|given| given == p.name))
    }

    /// Every shape this accepts — the required parameters always, and any
    /// subset of the optional ones.
    pub fn shapes(&self) -> Vec<Shape> {
        let (optional, required): (Vec<&Param>, Vec<&Param>) =
            self.keyword.iter().partition(|p| p.default.is_some());
        (0..1u32 << optional.len())
            .map(|mask| {
                let taken = optional
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask >> i & 1 == 1)
                    .map(|(_, p)| *p);
                Shape::new(
                    self.positional,
                    required
                        .iter()
                        .copied()
                        .chain(taken)
                        .map(|p| p.name.to_string())
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    /// The builtin this row means, given the name it was found under.
    pub fn selected(&self, head: Builtin) -> Builtin {
        self.selects.unwrap_or(head)
    }

    /// How many arguments the core call this resolves to holds. Every
    /// parameter, given or defaulted — the core call is the same length for
    /// every shape of one signature.
    pub fn arity(&self) -> usize {
        self.positional + self.keyword.len()
    }
}
