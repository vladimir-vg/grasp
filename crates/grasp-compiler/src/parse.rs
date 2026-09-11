//! The parser — `docs/grasp/syntax.md`.
//!
//! Recursive descent over the token stream, producing the AST that document
//! gives verbatim. Three things here are not the usual shape, and each is a
//! property of grasp rather than a choice:
//!
//! - **Operator groups are incomparable.** Precedence is defined within a group;
//!   between groups only three relations hold, and every other adjacency is an
//!   error. A binding-power table cannot say that, so each level checks the
//!   group of the operand it just parsed — see `permits` below.
//! - **Indentation delimits a rule body and nothing else.** There is no layout
//!   stack; a body's width is set by its first statement and read off the token
//!   spans.
//! - **`name(` at statement level is an atom or a call depending on the name.**
//!   The one-name rule — a name is either a relation or a callable, never
//!   both — is what makes that decidable without lookahead.

use crate::ast::*;
use crate::diag::{Diagnostic, Pass, Span};
use crate::lex::{Tok, Token, lex};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Reserved names — `docs/grasp/syntax.md`, "Reserved words".
// ---------------------------------------------------------------------------

/// Keywords never reach the parser as identifiers; the lexer has already turned
/// them into their own tokens. The list is here so a diagnostic can name them
/// and so `tests/reserved.rs` can pin it against the specification.
pub const KEYWORDS: &[&str] = &["not", "and", "or", "input", "true", "false", "NONE"];

pub const TYPE_NAMES: &[&str] = &[
    "boolean",
    "i64",
    "f64",
    "string",
    "json",
    "optional",
    "record",
    "array",
    "dict",
    "bytes",
    "date",
    "time",
    "timestamp",
    "interval",
    "dynamic",
];

/// The word after `::` that says what kind of thing is being declared. Neither
/// is a type — `relation(…)` is not a value and a function is not one either —
/// but both are reserved for the same reason a type name is: a relation called
/// `function` would make `f :: function` two readings.
pub const DECL_KINDS: &[&str] = &["relation", "function"];

pub const AGGREGATORS: &[&str] = &["sum", "count", "min", "max", "avg"];

/// "They are held now so that the standard library can grow into them without
/// taking names a program was already using."
pub const RESERVED_NAMESPACES: &[&str] = &[
    "string", "array", "dict", "record", "json", "boolean", "agg", "temporal", "integer", "float",
    "numeric", "bytes", "bits", "crypto", "dynamic",
];

/// Reserved as a relation, variable or column name.
/// grasp's identifier shape, `[a-zA-Z_][a-zA-Z0-9_]*`.
///
/// A dict key may be quoted, and a quoted one is arbitrary text — so this is
/// what decides whether `{k:}` names a variable. It is deliberately not
/// `check_variable_name`, which also rules out reserved and namespaced names:
/// those are separate objections with their own messages.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a name is a type variable: `T`, `K`, `V`.
///
/// An initial capital is the whole of the rule. Every type's own name is
/// lowercase, so a variable needs no declaration to be told from one, and the
/// `array(T)` in a typespec reads as it would in any other language.
fn is_type_var(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Whether a name sits under one of the namespaces the language holds.
///
/// Those hold callables and nothing else, which is what lets body-statement
/// position tell an atom from a filter without a lookahead or a list.
pub fn in_reserved_namespace(name: &str) -> bool {
    name.split_once(':')
        .is_some_and(|(ns, _)| RESERVED_NAMESPACES.contains(&ns))
}

pub fn is_reserved(name: &str) -> bool {
    KEYWORDS.contains(&name)
        || TYPE_NAMES.contains(&name)
        || DECL_KINDS.contains(&name)
        || AGGREGATORS.contains(&name)
}

fn namespace_of(name: &str) -> Option<&str> {
    name.split_once(':').map(|(head, _)| head)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn parse(source: &str) -> Result<Program, Diagnostic> {
    let mut tokens = lex(source)?;
    split_in_subscripts(&mut tokens);
    let mut p = Parser {
        toks: &tokens,
        pos: 0,
    };
    p.program()
}

/// Undo the lexer's joining of namespace segments, inside a subscript only.
///
/// `identifier ::= name (":" name)*`, so `arr[x:y]` lexes as
/// `arr [ Ident("x:y") ]` and reads as one qualified name where a slice was
/// meant. **A qualified name is only ever a callable, and a callable is only
/// ever followed by `(`** — so inside a subscript an identifier that is not
/// followed by `(` is split back into its segments and the colons between them,
/// and nothing is lost. `arr[1:5]` and `arr[x:5]` were never at risk, a segment
/// having to begin with a letter.
///
/// It is done over the token stream because it has to happen before the
/// expression is parsed. `arr[a+b:c]` joins `b` to `c`, and no surgery on the
/// parsed tree could put that back: the tree is `(a + (b:c))`, and the slice
/// wanted is `a+b` and `c`.
///
/// The same goes for `::`, which the lexer builds out of two colons for an
/// annotation: `a[::2]` wants them back. An assertion is a statement and never
/// an expression, so inside a subscript there is nothing else it could be.
///
/// This is the one place either is undone.
fn split_in_subscripts(toks: &mut Vec<Token>) {
    // Which enclosing brackets are subscripts. A `[` opens one when it follows
    // something a postfix expression can end with and does not start a line —
    // the same test `Parser::opens_here` makes.
    let mut brackets: Vec<bool> = Vec::new();
    let mut out: Vec<Token> = Vec::with_capacity(toks.len());
    for (i, tok) in toks.iter().enumerate() {
        match tok.kind {
            Tok::LBracket => brackets.push(!tok.first_on_line && ends_a_primary(toks, i)),
            Tok::RBracket => {
                brackets.pop();
            }
            _ => {}
        }
        let subscript = brackets.iter().any(|&b| b);
        // `a[::2]` — the other token the lexer builds out of colons. A `::`
        // inside a subscript can only be two of a slice's, an assertion being a
        // statement and never an expression.
        if subscript && tok.kind == Tok::Annot {
            for n in 0..2 {
                out.push(Token {
                    kind: Tok::Colon,
                    span: Span::new(tok.span.line, tok.span.column + n, 1),
                    first_on_line: tok.first_on_line && n == 0,
                });
            }
            continue;
        }
        let split = subscript
            && matches!(&tok.kind, Tok::Ident(n) if n.contains(':'))
            && !matches!(toks.get(i + 1).map(|t| &t.kind), Some(Tok::LParen));
        let Tok::Ident(name) = &tok.kind else {
            out.push(tok.clone());
            continue;
        };
        if !split {
            out.push(tok.clone());
            continue;
        }
        let mut column = tok.span.column;
        for (n, segment) in name.split(':').enumerate() {
            if n > 0 {
                out.push(Token {
                    kind: Tok::Colon,
                    span: Span::new(tok.span.line, column, 1),
                    first_on_line: false,
                });
                column += 1;
            }
            out.push(Token {
                kind: Tok::Ident(segment.to_string()),
                span: Span::new(tok.span.line, column, segment.len()),
                // Only the piece that was there keeps it.
                first_on_line: tok.first_on_line && n == 0,
            });
            column += segment.len();
        }
    }
    *toks = out;
}

/// Whether the token before position `i` can end a postfix expression, which is
/// what makes a `[` a subscript rather than an array literal or pattern.
fn ends_a_primary(toks: &[Token], i: usize) -> bool {
    i > 0
        && matches!(
            toks[i - 1].kind,
            Tok::Ident(_)
                | Tok::Int(_)
                | Tok::Float(_)
                | Tok::Str(_)
                | Tok::True
                | Tok::False
                | Tok::None
                | Tok::RParen
                | Tok::RBracket
                | Tok::RBrace
        )
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    // -- token access -------------------------------------------------------

    fn peek(&self) -> Option<&'a Token> {
        self.toks.get(self.pos)
    }

    fn peek_at(&self, n: usize) -> Option<&'a Token> {
        self.toks.get(self.pos + n)
    }

    fn kind(&self) -> Option<&'a Tok> {
        self.peek().map(|t| &t.kind)
    }

    fn kind_at(&self, n: usize) -> Option<&'a Tok> {
        self.peek_at(n).map(|t| &t.kind)
    }

    fn at(&self, k: &Tok) -> bool {
        self.kind() == Some(k)
    }

    /// Whether the token `n` along opens a bracket **on this line**.
    ///
    /// A body statement is one per line, so an expression may not reach across
    /// one to take the bracket that opens the next: `n := a` above
    /// `(v) := *arr` is a match and then an unnest, never the call `a(v)`, and
    /// above `[x] := arr` never the subscript `a[x]`. Without this the parser
    /// runs the two lines together and reports the `:=` it then trips over,
    /// which names neither line.
    fn opens_here(&self, n: usize, bracket: &Tok) -> bool {
        self.peek_at(n)
            .is_some_and(|t| &t.kind == bracket && !t.first_on_line)
    }

    fn bump(&mut self) -> &'a Token {
        let t = &self.toks[self.pos];
        self.pos += 1;
        t
    }

    fn eat(&mut self, k: &Tok) -> bool {
        if self.at(k) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// The span to blame when the input has run out: the last token, or the
    /// very start of an empty program.
    fn eof_span(&self) -> Span {
        self.toks
            .last()
            .map(|t| t.span)
            .unwrap_or_else(|| Span::new(1, 1, 0))
    }

    fn here(&self) -> Span {
        self.peek()
            .map(|t| t.span)
            .unwrap_or_else(|| self.eof_span())
    }

    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(Pass::Parse, span, message)
    }

    fn unexpected(&self, expected: &str) -> Diagnostic {
        match self.peek() {
            Some(t) => self.error(
                t.span,
                format!(
                    "unexpected token {}; expected {expected}",
                    t.kind.describe()
                ),
            ),
            None => self.error(
                self.eof_span(),
                format!("unexpected end of input; expected {expected}"),
            ),
        }
    }

    fn expect(&mut self, k: &Tok, expected: &str) -> Result<&'a Token, Diagnostic> {
        if self.at(k) {
            Ok(self.bump())
        } else {
            Err(self.unexpected(expected))
        }
    }

    /// An identifier, checked for the wildcard so `_` gets its own message
    /// wherever it is misused rather than a bare "unexpected token".
    fn expect_ident(&mut self, expected: &str) -> Result<(String, Span), Diagnostic> {
        match self.kind() {
            Some(Tok::Ident(name)) => {
                let span = self.bump().span;
                Ok((name.clone(), span))
            }
            Some(Tok::Underscore) => Err(self.error(self.here(), WILDCARD_MISUSE)),
            _ => Err(self.unexpected(expected)),
        }
    }

    // -- program ------------------------------------------------------------

    fn program(&mut self) -> Result<Program, Diagnostic> {
        let mut decls = Vec::new();
        while self.peek().is_some() {
            decls.push(self.decl()?);
        }
        check_one_typespec_each(&decls)?;
        Ok(decls)
    }

    fn decl(&mut self) -> Result<Decl, Diagnostic> {
        let start = self.here();
        let (name, name_span) = self.expect_ident("a relation name")?;

        // `::` introduces a typespec, and the word after it says of what. The
        // name is checked *after* that word rather than before, because the two
        // kinds hold opposite rules: a relation may not be namespaced and a
        // function must be.
        if self.at(&Tok::Annot) {
            self.bump();
            let (kind, kind_span) = self.expect_ident("`relation` or `function`")?;
            return match kind.as_str() {
                "relation" => {
                    self.check_relation_name(&name, name_span)?;
                    Ok(Decl::Spec(self.spec(name, start)?))
                }
                "function" => {
                    self.check_function_name(&name, name_span)?;
                    Ok(Decl::Function(self.fn_spec(name, start)?))
                }
                _ => Err(self.error(
                    kind_span,
                    format!("expected `relation` or `function`, found `{kind}`"),
                )),
            };
        }
        self.check_relation_name(&name, name_span)?;

        self.expect(&Tok::LParen, "`(` or `::`")?;
        // A head or a fact — neither may hold a wildcard.
        let args = self.kv_args(&Tok::RParen, Position::Head)?;
        let close = self.expect(&Tok::RParen, "`)` or `,`")?;
        check_duplicate_columns(&args)?;

        if self.at(&Tok::Arrow) {
            let arrow = self.bump().span;
            let head = Head {
                relation: name,
                args,
                span: start.to(close.span),
            };
            let body = self.rule_body(start, arrow)?;
            let span = start.to(body.last().map(|s| s.span()).unwrap_or(arrow));
            return Ok(Decl::Rule(Rule { head, body, span }));
        }

        // A fact. Only now is it known which of the two this is, which is why
        // the argument list admitted an aggregate: one folds a rule's body, and
        // a fact has none.
        if let Some(a) = args
            .iter()
            .find(|a| matches!(a.value, Arg::Aggregate { .. }))
        {
            return Err(self.error(
                a.span,
                "a fact asserts a value, and an aggregate folds a rule's body — \
                 this one has no body to fold",
            ));
        }
        // The next declaration must start its own line, which is what makes a
        // missing `<-` a diagnostic rather than a silent second fact.
        self.expect_line_start("a declaration")?;
        Ok(Decl::Fact(Fact {
            relation: name,
            args,
            span: start.to(close.span),
        }))
    }

    fn expect_line_start(&self, what: &str) -> Result<(), Diagnostic> {
        match self.peek() {
            Some(t) if !t.first_on_line => Err(self.error(
                t.span,
                format!(
                    "unexpected token {}; {what} must start a new line",
                    t.kind.describe()
                ),
            )),
            _ => Ok(()),
        }
    }

    fn check_relation_name(&self, name: &str, span: Span) -> Result<(), Diagnostic> {
        if is_reserved(name) {
            return Err(self.error(
                span,
                format!("`{name}` is a reserved word and cannot name a relation"),
            ));
        }
        // "A name is either a relation or a callable, never both." Every
        // callable is namespaced, so holding the namespaces *is* the one-name
        // rule — which is what lets body-statement position resolve `name(`
        // without lookahead, and why a relation may still be called `length`.
        if let Some(ns) = namespace_of(name)
            && RESERVED_NAMESPACES.contains(&ns)
        {
            return Err(self.error(
                span,
                format!("the namespace `{ns}:` is reserved for the language"),
            ));
        }
        Ok(())
    }

    /// grasp has no user-defined functions: a function typespec declares a name
    /// in the standard library, and the library lives under the held
    /// namespaces. A bare name would also be one the [one-name rule][syntax]
    /// cannot keep apart from a relation's.
    ///
    /// [syntax]: ../../../docs/grasp/syntax.md
    fn check_function_name(&self, name: &str, span: Span) -> Result<(), Diagnostic> {
        if in_reserved_namespace(name) {
            return Ok(());
        }
        Err(self.error(
            span,
            format!(
                "`{name}` is under no reserved namespace, and grasp has no \
                 user-defined functions: a function typespec declares a name in \
                 the standard library"
            ),
        ))
    }

    fn check_column_name(&self, name: &str, span: Span) -> Result<(), Diagnostic> {
        if is_reserved(name) {
            return Err(self.error(
                span,
                format!("`{name}` is a reserved word and cannot name a column"),
            ));
        }
        if name.contains(':') {
            return Err(self.error(
                span,
                format!("`{name}` is namespace-qualified; a column name is a single segment"),
            ));
        }
        Ok(())
    }

    /// A keyword argument's label is **not** a name: it names no relation, no
    /// variable and no column, so the reserved words are free here.
    ///
    /// `temporal:timestamp(date: d, time: t)` is the reason — the label a
    /// reader wants is the type's own word. A call site never checked this at
    /// all, so it is the typespec that was the odd one out.
    fn check_parameter_name(&self, name: &str, span: Span) -> Result<(), Diagnostic> {
        if name.contains(':') {
            return Err(self.error(
                span,
                format!("`{name}` is namespace-qualified; a parameter name is a single segment"),
            ));
        }
        Ok(())
    }

    fn check_variable_name(&self, name: &str, span: Span) -> Result<(), Diagnostic> {
        if is_reserved(name) {
            return Err(self.error(
                span,
                format!("`{name}` is a reserved word and cannot name a variable"),
            ));
        }
        // "Variables are always single-segment: no namespace."
        if name.contains(':') {
            return Err(self.error(
                span,
                format!("`{name}` is namespace-qualified; a variable name is a single segment"),
            ));
        }
        Ok(())
    }

    // -- spec ---------------------------------------------------------------

    fn spec(&mut self, relation: String, start: Span) -> Result<Spec, Diagnostic> {
        self.expect(&Tok::LParen, "`(`")?;
        let mut columns: Vec<(String, Type)> = Vec::new();
        let mut seen: Vec<(String, Span)> = Vec::new();
        while !self.at(&Tok::RParen) {
            let (name, name_span) = self.expect_ident("a column name")?;
            self.check_column_name(&name, name_span)?;
            if seen.iter().any(|(s, _)| *s == name) {
                return Err(self.error(name_span, format!("duplicate column `{name}`")));
            }
            seen.push((name.clone(), name_span));
            self.expect(&Tok::Colon, "`:`")?;
            columns.push((name, self.ty()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let close = self.expect(&Tok::RParen, "`)` or `,`")?;
        self.expect_line_start("a declaration")?;
        Ok(Spec {
            relation,
            columns,
            span: start.to(close.span),
        })
    }

    // -- function typespecs -------------------------------------------------

    /// The variants under `name :: function`, one per line and indented past
    /// the name.
    ///
    /// Written in bulk — one block per name, however many ways there are to
    /// call it — so that a function's typespec is in one place and a second
    /// block for the same name is plainly a conflict rather than an addition.
    /// The layout is a rule body's, and ends the same way: at the first line
    /// that is not indented into it.
    fn fn_spec(&mut self, name: String, start: Span) -> Result<FnSpec, Diagnostic> {
        let indent = match self.peek() {
            Some(t) if t.first_on_line && t.span.column > start.column => t.span.column,
            _ => {
                return Err(self.error(
                    self.here(),
                    "a function typespec needs at least one variant, on its own \
                     line and indented past the name",
                ));
            }
        };

        let mut variants: Vec<Variant> = Vec::new();
        while let Some(t) = self.peek() {
            if !t.first_on_line {
                return Err(self.error(
                    t.span,
                    format!(
                        "unexpected token {}; a typespec is one variant per line",
                        t.kind.describe()
                    ),
                ));
            }
            let column = t.span.column;
            if column < indent {
                if column > start.column {
                    return Err(self.error(
                        t.span,
                        format!(
                            "inconsistent indentation: this variant starts at column \
                             {column}, but the first one starts at column {indent}"
                        ),
                    ));
                }
                break;
            }
            let variant = self.variant()?;
            // "There shouldn't be conflicting typespecs for the same function."
            // Two variants that answer one shape *with the same types* are two
            // answers to one call, rather than an ambiguity to break by
            // preferring the first. With different types they are an overload,
            // and the argument's type is what picks.
            //
            // Shapes rather than a shape, because an optional parameter makes a
            // variant answer several.
            let signature = variant.signature();
            let shapes = variant.shapes();
            let clash = variants.iter().find(|v| {
                v.signature() == signature && v.shapes().iter().any(|s| shapes.contains(s))
            });
            if let Some(other) = clash {
                let shape = other
                    .shapes()
                    .into_iter()
                    .find(|s| shapes.contains(s))
                    .expect("the clash was found by a shared shape");
                return Err(self.error(
                    variant.span,
                    format!("`{name}` already has a variant called `{shape}` with these types"),
                ));
            }
            variants.push(variant);
        }

        let span = start.to(variants.last().map(|v| v.span).unwrap_or(start));
        Ok(FnSpec {
            name,
            variants,
            span,
        })
    }

    /// `variant ::= "(" [params] ")" "->" type`
    /// The value after `=` in a typespec parameter.
    ///
    /// A literal and nothing else: a default is what a call means when it
    /// leaves the parameter out, and an expression there would be one the
    /// language has no place to evaluate. A leading `-` is admitted on a
    /// number, since `= -1` is a default a program would want to write.
    fn default_literal(&mut self) -> Result<Lit, Diagnostic> {
        let negated = self.eat(&Tok::Minus);
        let span = self.here();
        let value = match self.kind() {
            Some(Tok::Int(n)) => Lit::Int(*n),
            Some(Tok::Float(f)) => Lit::Float(*f),
            Some(Tok::Str(s)) if !negated => Lit::Str(s.clone()),
            Some(Tok::True) if !negated => Lit::Bool(true),
            Some(Tok::False) if !negated => Lit::Bool(false),
            Some(Tok::None) if !negated => Lit::None,
            _ => return Err(self.error(span, "a default must be a literal")),
        };
        self.bump();
        Ok(match (negated, value) {
            (true, Lit::Int(n)) => Lit::Int(-n),
            (true, Lit::Float(f)) => Lit::Float(-f),
            (_, other) => other,
        })
    }

    fn variant(&mut self) -> Result<Variant, Diagnostic> {
        let start = self.here();
        self.expect(&Tok::LParen, "`(`")?;
        let mut positional: Vec<Type> = Vec::new();
        let mut keyword: Vec<Parameter> = Vec::new();
        while !self.at(&Tok::RParen) {
            // `index: i64` is named; `array(T)` is not. A type name is followed
            // by `(` or by nothing, never by `:`, so one token of lookahead
            // settles it — the same test a call's arguments use.
            let named =
                matches!(self.kind(), Some(Tok::Ident(_))) && self.kind_at(1) == Some(&Tok::Colon);
            if named {
                let (param, param_span) = self.expect_ident("a parameter name")?;
                self.check_parameter_name(&param, param_span)?;
                self.bump(); // `:`
                if keyword.iter().any(|p| p.name == param) {
                    return Err(self.error(param_span, format!("duplicate parameter `{param}`")));
                }
                let ty = self.ty_of(true)?;
                // `= 0` makes it optional, and a variant with optional
                // parameters answers one shape per subset of them — which is
                // what keeps `array:slice` to one line instead of eight.
                let default = self
                    .eat(&Tok::Eq)
                    .then(|| self.default_literal())
                    .transpose()?;
                keyword.push(Parameter {
                    name: param,
                    ty,
                    default,
                });
            } else {
                // "Subject positional, everything else keyword" — so a
                // positional parameter after a named one would name a subject
                // the keywords already passed.
                if !keyword.is_empty() {
                    return Err(self.error(
                        self.here(),
                        "a positional parameter cannot follow a named one",
                    ));
                }
                positional.push(self.ty_of(true)?);
                // A default is what makes a parameter optional, and a
                // positional one cannot be left out: the shape is its count.
                if self.at(&Tok::Eq) {
                    return Err(self.error(
                        self.here(),
                        "only a named parameter may have a default; a positional \
                         one cannot be left out",
                    ));
                }
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)` or `,`")?;
        self.expect(&Tok::Returns, "`->`")?;
        let result = self.ty_of(true)?;
        Ok(Variant {
            positional,
            keyword,
            result,
            span: start.to(self.previous_span()),
        })
    }

    // -- types --------------------------------------------------------------

    fn ty(&mut self) -> Result<Type, Diagnostic> {
        self.ty_of(false)
    }

    /// `vars` is whether a type variable is legal here, which is only inside a
    /// [function typespec][`Self::fn_spec`]: `T` in a relation's columns would
    /// be a type nothing ever settles.
    fn ty_of(&mut self, vars: bool) -> Result<Type, Diagnostic> {
        let (name, span) = self.expect_ident("a type")?;
        match name.as_str() {
            "boolean" => Ok(Type::Boolean),
            "i64" => Ok(Type::I64),
            "f64" => Ok(Type::F64),
            "string" => Ok(Type::String),
            "json" => Ok(Type::Json),
            "date" => Ok(Type::Date),
            "time" => Ok(Type::Time),
            "timestamp" => Ok(Type::Timestamp),
            "interval" => Ok(Type::Interval),
            "bytes" => Ok(Type::Bytes),
            "dynamic" => Ok(Type::Dynamic),
            "optional" => {
                self.expect(&Tok::LParen, "`(`")?;
                let inner = self.ty_of(vars)?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Type::Optional(Box::new(inner)))
            }
            "array" => {
                self.expect(&Tok::LParen, "`(`")?;
                let inner = self.ty_of(vars)?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Type::Array(Box::new(inner)))
            }
            "dict" => {
                self.expect(&Tok::LParen, "`(`")?;
                let key_span = self.here();
                let key = self.ty_of(vars)?;
                // `syntax.md`'s grammar has said `dict "(" key_type ...` all
                // along; this is where it starts being true. Without it a
                // `dict(record(…), i64)` written anywhere — a spec, a column,
                // an assertion — reaches the target, which refuses it against
                // text the program never wrote.
                if !key.is_dict_key() {
                    return Err(self.error(
                        key_span,
                        format!(
                            "`{key}` cannot be a dict key: a key is a scalar — `boolean`, \
                             `i64`, `f64`, `string`, `bytes`, `date`, `time`, `timestamp` \
                             or `interval`"
                        ),
                    ));
                }
                self.expect(&Tok::Comma, "`,`")?;
                let value = self.ty_of(vars)?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Type::Dict(Box::new(key), Box::new(value)))
            }
            "record" => {
                self.expect(&Tok::LParen, "`(`")?;
                let mut fields: Vec<(String, Type)> = Vec::new();
                while !self.at(&Tok::RParen) {
                    let (field, field_span) = self.expect_ident("a field name")?;
                    self.check_column_name(&field, field_span)?;
                    if fields.iter().any(|(f, _)| *f == field) {
                        return Err(self.error(field_span, format!("duplicate field `{field}`")));
                    }
                    self.expect(&Tok::Colon, "`:`")?;
                    fields.push((field, self.ty_of(vars)?));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)` or `,`")?;
                Ok(Type::Record(fields))
            }
            // `T` — a type variable. Uppercase is the whole of the rule: a
            // type's name is lowercase, so nothing has to be declared before it
            // is used.
            _ if is_type_var(&name) => {
                if !vars {
                    return Err(self.error(
                        span,
                        format!(
                            "`{name}` is a type variable, which is legal in a \
                             function typespec and nowhere else"
                        ),
                    ));
                }
                Ok(Type::Var(name))
            }
            _ => Err(self.error(span, format!("`{name}` is not a type"))),
        }
    }

    // -- rule bodies --------------------------------------------------------

    /// `head "<-" body_stmt` on one line, or `head "<-"` and then an indented
    /// block. Indentation delimits a multiline body and nothing else.
    fn rule_body(&mut self, head_start: Span, arrow: Span) -> Result<Vec<Stmt>, Diagnostic> {
        let Some(first) = self.peek() else {
            return Err(self.error(arrow, "a rule needs a body"));
        };

        if !first.first_on_line {
            // Single-line: exactly one statement, and nothing may follow it on
            // the same line.
            let stmt = self.body_stmt()?;
            self.expect_line_start("a declaration")?;
            return Ok(vec![stmt]);
        }

        // "The width is set by the first body statement."
        let indent = first.span.column;
        if indent <= head_start.column {
            return Err(self.error(
                arrow,
                "a rule body on its own line must be indented past the head",
            ));
        }

        let mut body = Vec::new();
        while let Some(t) = self.peek() {
            if !t.first_on_line {
                return Err(self.error(
                    t.span,
                    format!(
                        "unexpected token {}; a body statement is one per line",
                        t.kind.describe()
                    ),
                ));
            }
            let column = t.span.column;
            if column < indent {
                // A line indented less than the first statement ends the rule —
                // unless it is still indented past the head, in which case it is
                // plainly meant to be a body statement and is merely misaligned.
                if column > head_start.column {
                    return Err(self.error(
                        t.span,
                        format!(
                            "inconsistent indentation: this body statement starts at \
                             column {column}, but the body starts at column {indent}"
                        ),
                    ));
                }
                break;
            }
            body.push(self.body_stmt()?);
        }

        if body.is_empty() {
            return Err(self.error(arrow, "a rule needs a body"));
        }
        Ok(body)
    }

    fn body_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.here();

        // input_stmt ::= "input"
        if self.at(&Tok::Input) {
            let span = self.bump().span;
            return Ok(Stmt::Input { span });
        }

        // negated_atom ::= "not" positive_atom
        if self.at(&Tok::Not) {
            let not_span = self.bump().span;
            let (name, name_span) = self.expect_ident("a relation name")?;
            self.check_relation_name(&name, name_span)?;
            let (args, close) = self.atom_args()?;
            return Ok(Stmt::Atom {
                relation: name,
                args,
                negated: true,
                span: not_span.to(close),
            });
        }

        // A bracketed form is a pattern when `:=` follows it, and an expression
        // otherwise. Both readings are grammatical, so this is the one place
        // that needs to look ahead — over a balanced group, which is cheap.
        if let Some(close) = self.pattern_opener()
            && self.assign_follows_balanced(close)
        {
            return self.match_stmt(start);
        }

        if let Some(Tok::Ident(name)) = self.kind() {
            let name = name.clone();
            match self.kind_at(1) {
                // type_assertion ::= variable "::" type
                Some(Tok::Annot) => {
                    let var_span = self.bump().span;
                    self.check_variable_name(&name, var_span)?;
                    self.bump(); // `::`
                    let ty = self.ty()?;
                    return Ok(Stmt::Assert {
                        variable: name,
                        ty,
                        span: start.to(self.previous_span()),
                    });
                }
                // match ::= variable ":=" ...
                Some(Tok::Assign) => return self.match_stmt(start),
                // "At body-statement level, `name(` always begins an atom" —
                // for every name that can be a relation. A name under a
                // reserved namespace cannot be one, so `string:length(s) > 3`
                // is a filter.
                //
                // Structural, where it used to be a lookup in the builtin list:
                // every callable is namespaced now, and every namespace a
                // callable lives in is reserved, so the shape of the name is
                // the answer.
                Some(Tok::LParen) if !in_reserved_namespace(&name) => {
                    let name_span = self.bump().span;
                    self.check_relation_name(&name, name_span)?;
                    let (args, close) = self.atom_args()?;
                    return Ok(Stmt::Atom {
                        relation: name,
                        args,
                        negated: false,
                        span: start.to(close),
                    });
                }
                _ => {}
            }
        }

        // filter ::= expr
        let expr = self.expr()?.expr;
        let span = start.to(self.previous_span());
        Ok(Stmt::Filter { expr, span })
    }

    fn previous_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1).min(self.toks.len() - 1)].span
    }

    /// If the next token opens a bracketed group that could be a pattern, the
    /// token that closes it.
    fn pattern_opener(&self) -> Option<Tok> {
        match self.kind()? {
            Tok::LParen => Some(Tok::RParen),
            Tok::LBrace => Some(Tok::RBrace),
            Tok::LBracket => Some(Tok::RBracket),
            // `record(a: x) := rec` — the only keyword-led pattern.
            Tok::Ident(n) if n == "record" && self.kind_at(1) == Some(&Tok::LParen) => {
                Some(Tok::RParen)
            }
            _ => None,
        }
    }

    /// Whether `:=` follows the balanced group starting at the cursor.
    fn assign_follows_balanced(&self, close: Tok) -> bool {
        let mut i = self.pos;
        // Step over a leading `record`, so the scan starts at the bracket.
        if matches!(self.kind(), Some(Tok::Ident(n)) if n == "record") {
            i += 1;
        }
        let open = match &close {
            Tok::RParen => Tok::LParen,
            Tok::RBrace => Tok::LBrace,
            _ => Tok::LBracket,
        };
        let mut depth = 0usize;
        while let Some(t) = self.toks.get(i) {
            if t.kind == open {
                depth += 1;
            } else if t.kind == close {
                depth -= 1;
                if depth == 0 {
                    return self.toks.get(i + 1).map(|t| &t.kind) == Some(&Tok::Assign);
                }
            }
            i += 1;
        }
        false
    }

    fn atom_args(&mut self) -> Result<(Vec<KvArg>, Span), Diagnostic> {
        self.expect(&Tok::LParen, "`(`")?;
        let args = self.kv_args(&Tok::RParen, Position::Atom)?;
        let close = self.expect(&Tok::RParen, "`)` or `,`")?;
        check_duplicate_columns(&args)?;
        Ok((args, close.span))
    }

    /// `kv_arg ::= name ":" expr | name ":" | name ":" "_"`
    fn kv_args(&mut self, terminator: &Tok, position: Position) -> Result<Vec<KvArg>, Diagnostic> {
        let mut args = Vec::new();
        while !self.at(terminator) {
            let start = self.here();
            let (column, col_span) = self.expect_ident("a column name")?;
            self.check_column_name(&column, col_span)?;
            self.expect(&Tok::Colon, "`:`")?;

            let value = if self.at(&Tok::Underscore) {
                // "It is legal only in an atom's argument position, where
                //  'there is a column here I do not care about' is the whole
                //  meaning." A head has to produce a value for every column and
                //  a fact has to assert one, so neither has that meaning
                //  available — which is why this list has to know which it is.
                if position == Position::Head {
                    return Err(self.error(self.here(), WILDCARD_MISUSE));
                }
                Arg::Wildcard(self.bump().span)
            } else if self.at(&Tok::Comma) || self.at(terminator) {
                // The shorthand `x:` means `x: x`, and this is where it becomes
                // that — one row of `semantics.md`'s desugaring table performed
                // by the parser, so desugaring does not do it again.
                self.check_variable_name(&column, col_span)?;
                Arg::Expr(Expr::Var {
                    name: column.clone(),
                    span: col_span,
                })
            } else if self.at_aggregate() {
                // `total: sum<sal>` — sugar for `total := sum<sal>`, so the
                // column names the variable and has to be a name one can have.
                if position == Position::Atom {
                    return Err(self.error(
                        self.here(),
                        "an aggregate folds a rule's body, so it belongs in a head \
                         argument or on the right of `:=`; an atom's argument is one \
                         of the things it folds",
                    ));
                }
                self.check_variable_name(&column, col_span)?;
                let (function, arg, span) = self.aggregate()?;
                Arg::Aggregate {
                    function,
                    arg,
                    span,
                }
            } else {
                Arg::Expr(self.expr()?.expr)
            };

            args.push(KvArg {
                column,
                value,
                span: start.to(self.previous_span()),
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(args)
    }

    // -- matches and patterns ------------------------------------------------

    fn match_stmt(&mut self, start: Span) -> Result<Stmt, Diagnostic> {
        let mut lhs = self.pattern()?;
        self.expect(&Tok::Assign, "`:=`")?;

        // `*` and `**` are pattern markers, never binary operators; they are
        // told apart by position, and this is the position. Which unnest it is
        // belongs to the pattern, so the marker sets it there.
        if self.at(&Tok::Star) || self.at(&Tok::StarStar) {
            let dict = self.at(&Tok::StarStar);
            let marker = self.bump().span;
            let Pattern::Unnest { kind, .. } = &mut lhs else {
                return Err(self.error(marker, "an unnest needs a `(…)` pattern on the left"));
            };
            *kind = if dict {
                UnnestKind::Dict
            } else {
                UnnestKind::Array
            };
            let rhs = Rhs::Expr(self.expr()?.expr);
            return Ok(Stmt::Match {
                lhs,
                rhs,
                span: start.to(self.previous_span()),
            });
        }

        if let Pattern::Unnest { span, .. } = &lhs {
            return Err(self.error(
                *span,
                "a `(…)` pattern is an unnest; its right side is `*` or `**`",
            ));
        }

        let rhs = self.rhs()?;
        Ok(Stmt::Match {
            lhs,
            rhs,
            span: start.to(self.previous_span()),
        })
    }

    fn pattern(&mut self) -> Result<Pattern, Diagnostic> {
        let start = self.here();
        // record_pattern ::= "record" "(" … ")" — checked before the plain
        // identifier arm, which would otherwise read `record` as a variable.
        if matches!(self.kind(), Some(Tok::Ident(n)) if n == "record")
            && self.kind_at(1) == Some(&Tok::LParen)
        {
            self.bump();
            self.bump();
            let (fields, rest) = self.pattern_fields(&Tok::RParen)?;
            let close = self.expect(&Tok::RParen, "`)` or `,`")?;
            return Ok(Pattern::Record {
                fields,
                rest,
                span: start.to(close.span),
            });
        }
        match self.kind() {
            Some(Tok::Ident(name)) => {
                let name = name.clone();
                let span = self.bump().span;
                self.check_variable_name(&name, span)?;
                Ok(Pattern::Var { name, span })
            }
            Some(Tok::Underscore) => Err(self.error(self.here(), WILDCARD_MISUSE)),
            // unnest_pattern ::= "(" variable ("," variable)* ")"
            Some(Tok::LParen) => {
                self.bump();
                let mut vars = Vec::new();
                while !self.at(&Tok::RParen) {
                    let (name, span) = self.expect_ident("a variable")?;
                    self.check_variable_name(&name, span)?;
                    vars.push(name);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let close = self.expect(&Tok::RParen, "`)` or `,`")?;
                if vars.is_empty() {
                    return Err(self.error(
                        start.to(close.span),
                        "an unnest pattern needs at least one variable",
                    ));
                }
                // Which unnest it is comes from the `*` or `**` on the right,
                // so it is filled in by `rhs`.
                Ok(Pattern::Unnest {
                    vars,
                    kind: UnnestKind::Array,
                    span: start.to(close.span),
                })
            }
            Some(Tok::LBracket) => {
                self.bump();
                let mut elems = Vec::new();
                let mut rest = Rest::None;
                while !self.at(&Tok::RBracket) {
                    if self.at(&Tok::Star) {
                        self.bump();
                        rest = self.rest_binding()?;
                        break;
                    }
                    let (name, span) = self.expect_ident("a variable")?;
                    self.check_variable_name(&name, span)?;
                    elems.push(name);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let close = self.expect(&Tok::RBracket, "`]` or `,`")?;
                Ok(Pattern::Array {
                    elems,
                    rest,
                    span: start.to(close.span),
                })
            }
            Some(Tok::LBrace) => {
                self.bump();
                let (fields, rest) = self.pattern_fields(&Tok::RBrace)?;
                let close = self.expect(&Tok::RBrace, "`}` or `,`")?;
                Ok(Pattern::Dict {
                    fields,
                    rest,
                    span: start.to(close.span),
                })
            }
            _ => Err(self.unexpected("a pattern")),
        }
    }

    fn rest_binding(&mut self) -> Result<Rest, Diagnostic> {
        match self.kind() {
            Some(Tok::Ident(name)) => {
                let name = name.clone();
                let span = self.bump().span;
                self.check_variable_name(&name, span)?;
                Ok(Rest::Bind(name))
            }
            _ => Ok(Rest::Ignore),
        }
    }

    /// `pat_field ::= dict_key ":" [variable]`, then an optional `**` rest.
    ///
    /// "its values must be variables, so `{a: f(1)} := d` is rejected there
    /// rather than by the grammar" — which is this function.
    ///
    /// The variable may be left out: `{a:}` means `{a: a}`, the same shorthand
    /// an atom's `r(a:)` already has, performed here so that desugaring does
    /// not do it again. It needs the key to be a name a variable can have,
    /// which a quoted key need not be.
    fn pattern_fields(
        &mut self,
        terminator: &Tok,
    ) -> Result<(Vec<(String, String)>, Rest), Diagnostic> {
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut rest = Rest::None;
        while !self.at(terminator) {
            if self.at(&Tok::StarStar) {
                self.bump();
                rest = self.rest_binding()?;
                break;
            }
            let (key, key_span) = self.dict_key("a field name")?;
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(self.error(key_span, format!("duplicate field `{key}`")));
            }
            // "`=>` is not allowed in a pattern. A pattern names the key it
            //  extracts, so its keys are literal."
            if self.at(&Tok::FatArrow) {
                return Err(self.error(
                    self.here(),
                    "`=>` is not allowed in a pattern; a pattern names the key it extracts",
                ));
            }
            self.expect(&Tok::Colon, "`:`")?;
            let value_span = self.here();
            let (name, span) = match self.kind() {
                // `{a: f(1)} := d` — an identifier followed by `(` is a call,
                // which is exactly what a pattern may not hold.
                Some(Tok::Ident(_)) if self.kind_at(1) == Some(&Tok::LParen) => {
                    return Err(
                        self.error(value_span, "a pattern binds variables; a call is not one")
                    );
                }
                Some(Tok::Ident(n)) => {
                    let n = n.clone();
                    (n, self.bump().span)
                }
                // The shorthand: nothing between the `:` and what ends the
                // field, so the key names the variable.
                _ if self.at(&Tok::Comma) || self.at(terminator) => {
                    if !is_identifier(&key) {
                        return Err(self.error(
                            key_span,
                            format!(
                                "`{key}` is not a name a variable can have, so `{key}:` \
                                 names none; write the variable"
                            ),
                        ));
                    }
                    (key.clone(), key_span)
                }
                _ => {
                    return Err(
                        self.error(value_span, "a pattern binds variables; this is not one")
                    );
                }
            };
            self.check_variable_name(&name, span)?;
            fields.push((key, name));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok((fields, rest))
    }

    /// `dict_key ::= name | STRING` — quoted only when not an identifier.
    fn dict_key(&mut self, expected: &str) -> Result<(String, Span), Diagnostic> {
        match self.kind() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                Ok((s, self.bump().span))
            }
            Some(Tok::Ident(_)) => self.expect_ident(expected),
            _ => Err(self.unexpected(expected)),
        }
    }

    /// The right of `:=`, once the unnest markers have been dealt with: an
    /// aggregate, or an ordinary expression.
    fn rhs(&mut self) -> Result<Rhs, Diagnostic> {
        if self.at_aggregate() {
            let (function, arg, span) = self.aggregate()?;
            return Ok(Rhs::Aggregate {
                function,
                arg,
                span,
            });
        }

        Ok(Rhs::Expr(self.expr()?.expr))
    }

    /// Whether an `aggregate ::= aggregator "<" [expr] ">"` begins here.
    ///
    /// Peeks only, so a caller can refuse the form on the grounds of *where* it
    /// is before committing to reading it.
    fn at_aggregate(&self) -> bool {
        matches!(self.kind(), Some(Tok::Ident(name)) if Aggregator::from_name(name).is_some())
            && self.kind_at(1) == Some(&Tok::Lt)
    }

    /// `aggregate ::= aggregator "<" [expr] ">"`, which two places read: the
    /// right of a `:=`, and a head argument.
    fn aggregate(&mut self) -> Result<(Aggregator, Option<Expr>, Span), Diagnostic> {
        let Some(Tok::Ident(name)) = self.kind() else {
            unreachable!("`at_aggregate` said there was one")
        };
        let agg = Aggregator::from_name(name).expect("`at_aggregate` said there was one");
        let start = self.bump().span; // the aggregator
        self.bump(); // `<`
        // Below the comparison level, or the closing `>` is taken as a
        // greater-than: `sum<r>` would read as `sum<(r > …)`. A comparison
        // inside an aggregate must be parenthesised, which is the same answer
        // grasp gives everywhere else it declines to guess.
        let arg = if self.at(&Tok::Gt) {
            None
        } else {
            Some(self.cat_expr()?.expr)
        };
        let close = self.expect(&Tok::Gt, "`>`")?;
        Ok((agg, arg, start.to(close.span)))
    }

    // -- expressions ---------------------------------------------------------

    fn expr(&mut self) -> Result<Parsed, Diagnostic> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Parsed, Diagnostic> {
        let mut lhs = self.and_expr()?;
        while self.at(&Tok::Or) {
            let op_span = self.bump().span;
            let rhs = self.and_expr()?;
            lhs = self.combine(lhs, BinOp::Or, op_span, rhs)?;
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Parsed, Diagnostic> {
        let mut lhs = self.not_expr()?;
        while self.at(&Tok::And) {
            let op_span = self.bump().span;
            let rhs = self.not_expr()?;
            lhs = self.combine(lhs, BinOp::And, op_span, rhs)?;
        }
        Ok(lhs)
    }

    fn not_expr(&mut self) -> Result<Parsed, Diagnostic> {
        if self.at(&Tok::Not) {
            let span = self.bump().span;
            let operand = self.not_expr()?;
            // A unary operator adjacent to a binary one needs parentheses.
            // `not not a` is fine — that is two unary operators.
            if operand.unary.is_none()
                && let Some(inner) = operand.op_name
            {
                return Err(self.error(
                    span,
                    format!("operators `not` and `{inner}` cannot be mixed without parentheses"),
                ));
            }
            return Ok(Parsed::unary(UnOp::Not, span, operand.expr));
        }
        self.comparison()
    }

    /// `comparison ::= cat_expr [cmp_op cat_expr]` — **non-associative**.
    fn comparison(&mut self) -> Result<Parsed, Diagnostic> {
        let lhs = self.cat_expr()?;
        let Some(op) = self.comparison_op() else {
            return Ok(lhs);
        };
        let op_span = self.bump().span;
        let rhs = self.cat_expr()?;
        // "Comparison does not chain: `a < b < c` is an error, not
        //  `(a < b) < c`."
        if let Some(next) = self.comparison_op() {
            return Err(self.error(
                self.here(),
                format!(
                    "comparison cannot be chained; `{}` cannot follow `{}` without \
                     parentheses",
                    next.as_str(),
                    op.as_str()
                ),
            ));
        }
        self.combine(lhs, op, op_span, rhs)
    }

    fn comparison_op(&self) -> Option<BinOp> {
        match self.kind()? {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        }
    }

    fn cat_expr(&mut self) -> Result<Parsed, Diagnostic> {
        let mut lhs = self.add_expr()?;
        while self.at(&Tok::PlusPlus) {
            let op_span = self.bump().span;
            let rhs = self.add_expr()?;
            lhs = self.combine(lhs, BinOp::Concat, op_span, rhs)?;
        }
        Ok(lhs)
    }

    fn add_expr(&mut self) -> Result<Parsed, Diagnostic> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.kind() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => break,
            };
            let op_span = self.bump().span;
            let rhs = self.mul_expr()?;
            lhs = self.combine(lhs, op, op_span, rhs)?;
        }
        Ok(lhs)
    }

    fn mul_expr(&mut self) -> Result<Parsed, Diagnostic> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.kind() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Rem,
                _ => break,
            };
            let op_span = self.bump().span;
            let rhs = self.unary()?;
            lhs = self.combine(lhs, op, op_span, rhs)?;
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Parsed, Diagnostic> {
        if self.at(&Tok::Minus) {
            let span = self.bump().span;
            let operand = self.postfix()?;
            return Ok(Parsed::unary(UnOp::Neg, span, operand.expr));
        }
        self.postfix()
    }

    /// `postfix ::= primary ("." name | "[" expr "]")*`
    ///
    /// A `[` opening a line is an array destructure pattern, not a subscript on
    /// whatever the line above ended with — see [`Parser::opens_here`], which
    /// is the whole disambiguation and covers `(` for the same reason.
    fn postfix(&mut self) -> Result<Parsed, Diagnostic> {
        let mut base = self.primary()?;
        loop {
            if self.at(&Tok::Dot) {
                self.bump();
                let (name, name_span) = self.expect_ident("a field name")?;
                let span = base.expr.span().to(name_span);
                base = Parsed::primary(Expr::Field {
                    base: Box::new(base.expr),
                    name,
                    span,
                });
                continue;
            }
            if self.opens_here(0, &Tok::LBracket) {
                self.bump();
                base = Parsed::primary(self.subscript(base.expr)?);
                continue;
            }
            break;
        }
        Ok(base)
    }

    /// `subscript ::= expr | [expr] ":" [expr] [":" [expr]]`, the `[` consumed.
    ///
    /// One colon makes it a slice, and every part may be left out — `arr[:]` is
    /// a whole copy. The lookup form is the one that needs a type to be
    /// understood; a slice is over an array whatever it holds.
    fn subscript(&mut self, base: Expr) -> Result<Expr, Diagnostic> {
        let part = |p: &mut Self| -> Result<Option<Box<Expr>>, Diagnostic> {
            if p.at(&Tok::Colon) || p.at(&Tok::RBracket) {
                return Ok(None);
            }
            Ok(Some(Box::new(p.expr()?.expr)))
        };

        let start = part(self)?;
        if !self.eat(&Tok::Colon) {
            let Some(key) = start else {
                return Err(self.unexpected("an expression"));
            };
            let close = self.expect(&Tok::RBracket, "`]`")?;
            return Ok(Expr::Index {
                base: Box::new(base.clone()),
                key,
                span: base.span().to(close.span),
            });
        }
        let stop = part(self)?;
        let step = if self.eat(&Tok::Colon) {
            part(self)?
        } else {
            None
        };
        let close = self.expect(&Tok::RBracket, "`]`")?;
        Ok(Expr::Slice {
            base: Box::new(base.clone()),
            start,
            stop,
            step,
            span: base.span().to(close.span),
        })
    }

    fn primary(&mut self) -> Result<Parsed, Diagnostic> {
        let start = self.here();
        let Some(kind) = self.kind() else {
            return Err(self.unexpected("an expression"));
        };
        let lit = |p: &mut Self, value: Lit| {
            let span = p.bump().span;
            Ok(Parsed::primary(Expr::Lit { value, span }))
        };
        match kind {
            Tok::Int(n) => {
                let n = *n;
                lit(self, Lit::Int(n))
            }
            Tok::Float(f) => {
                let f = *f;
                lit(self, Lit::Float(f))
            }
            Tok::Str(s) => {
                let s = s.clone();
                lit(self, Lit::Str(s))
            }
            Tok::True => lit(self, Lit::Bool(true)),
            Tok::False => lit(self, Lit::Bool(false)),
            Tok::None => lit(self, Lit::None),
            // "It is legal only in an atom's argument position", and this is
            // not one.
            Tok::Underscore => Err(self.error(start, WILDCARD_MISUSE)),
            // A parenthesised expression is a *primary*: it belongs to no
            // operator group, which is exactly what parentheses are for here.
            Tok::LParen => {
                self.bump();
                let inner = self.expr()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Parsed::primary(inner.expr))
            }
            Tok::LBracket => {
                self.bump();
                let mut elems = Vec::new();
                while !self.at(&Tok::RBracket) {
                    elems.push(self.expr()?.expr);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let close = self.expect(&Tok::RBracket, "`]` or `,`")?;
                Ok(Parsed::primary(Expr::ArrayLit {
                    elems,
                    span: start.to(close.span),
                }))
            }
            Tok::LBrace => self.dict_literal(start),
            Tok::Ident(name) => {
                let name = name.clone();
                if name == "record" && self.opens_here(1, &Tok::LParen) {
                    return self.record_literal(start);
                }
                if self.opens_here(1, &Tok::LParen) {
                    return self.call(name, start);
                }
                let span = self.bump().span;
                self.check_variable_name(&name, span)?;
                Ok(Parsed::primary(Expr::Var { name, span }))
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// `dict_entry ::= expr "=>" expr | dict_key ":" expr`
    ///
    /// The `:` form is sugar for the common case, and its key is a string. The
    /// AST has one dict form, so it is written as one here — `{a: v}` **is**
    /// `{"a" => v}`, which is what `docs/grasp/semantics.md` says it means.
    fn dict_literal(&mut self, start: Span) -> Result<Parsed, Diagnostic> {
        self.bump(); // `{`
        let mut entries: Vec<(Expr, Expr)> = Vec::new();
        while !self.at(&Tok::RBrace) {
            let key_start = self.here();
            // A bare name or a quoted string followed by `:` is the sugar; an
            // expression followed by `=>` is the general form. A string can
            // begin either, so the `:` is what decides.
            let is_sugar = match self.kind() {
                Some(Tok::Ident(_)) | Some(Tok::Str(_)) => self.kind_at(1) == Some(&Tok::Colon),
                _ => false,
            };
            let key = if is_sugar {
                let (name, span) = self.dict_key("a key")?;
                self.bump(); // `:`
                Expr::Lit {
                    value: Lit::Str(name),
                    span,
                }
            } else {
                let k = self.expr()?.expr;
                if self.at(&Tok::Colon) {
                    return Err(self.error(
                        self.here(),
                        "a `:` dict key is a name or a string; write `=>` for an \
                         expression key",
                    ));
                }
                self.expect(&Tok::FatArrow, "`=>` or `:`")?;
                k
            };
            if let Expr::Lit { value, .. } = &key
                && entries.iter().any(|(k, _)| match k {
                    Expr::Lit { value: seen, .. } => seen == value,
                    _ => false,
                })
            {
                let shown = match value {
                    Lit::Str(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                return Err(self.error(key_start, format!("duplicate field `{shown}`")));
            }
            let value = self.expr()?.expr;
            entries.push((key, value));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let close = self.expect(&Tok::RBrace, "`}` or `,`")?;
        Ok(Parsed::primary(Expr::DictLit {
            entries,
            span: start.to(close.span),
        }))
    }

    fn record_literal(&mut self, start: Span) -> Result<Parsed, Diagnostic> {
        self.bump(); // `record`
        self.bump(); // `(`
        let mut fields: Vec<(String, Expr)> = Vec::new();
        while !self.at(&Tok::RParen) {
            let (name, name_span) = self.dict_key("a field name")?;
            if fields.iter().any(|(f, _)| *f == name) {
                return Err(self.error(name_span, format!("duplicate field `{name}`")));
            }
            self.expect(&Tok::Colon, "`:`")?;
            fields.push((name, self.expr()?.expr));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let close = self.expect(&Tok::RParen, "`)` or `,`")?;
        Ok(Parsed::primary(Expr::RecordLit {
            fields,
            span: start.to(close.span),
        }))
    }

    /// `args ::= expr ("," expr)* ("," name ":" expr)* | name ":" expr (…)*`
    fn call(&mut self, name: String, start: Span) -> Result<Parsed, Diagnostic> {
        self.bump(); // the name
        self.bump(); // `(`
        let mut positional = Vec::new();
        let mut keyword: Vec<(String, Expr)> = Vec::new();
        while !self.at(&Tok::RParen) {
            let is_keyword =
                matches!(self.kind(), Some(Tok::Ident(_))) && self.kind_at(1) == Some(&Tok::Colon);
            if is_keyword {
                let (arg, arg_span) = self.expect_ident("an argument name")?;
                self.bump(); // `:`
                if keyword.iter().any(|(k, _)| *k == arg) {
                    return Err(self.error(arg_span, format!("duplicate argument `{arg}`")));
                }
                keyword.push((arg, self.expr()?.expr));
            } else {
                if !keyword.is_empty() {
                    return Err(self.error(
                        self.here(),
                        "a positional argument cannot follow a named one",
                    ));
                }
                positional.push(self.expr()?.expr);
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let close = self.expect(&Tok::RParen, "`)` or `,`")?;
        Ok(Parsed::primary(Expr::Call {
            name,
            positional,
            keyword,
            span: start.to(close.span),
        }))
    }

    /// Build a binary node, enforcing the two rules that a precedence table
    /// cannot express.
    fn combine(
        &self,
        lhs: Parsed,
        op: BinOp,
        op_span: Span,
        rhs: Parsed,
    ) -> Result<Parsed, Diagnostic> {
        // "A unary operator adjacent to a binary one from another group is
        //  likewise an error: write `(not a) and b` and `(-a) * b`."
        for side in [&lhs, &rhs] {
            if let Some(u) = side.unary {
                return Err(self.error(
                    op_span,
                    format!(
                        "operators `{}` and `{}` cannot be mixed without parentheses",
                        u.as_str(),
                        op.as_str()
                    ),
                ));
            }
        }
        let outer = op.group();
        for side in [&lhs, &rhs] {
            if let Some(inner) = side.group
                && !permits(outer, inner)
            {
                return Err(self.error(
                    op_span,
                    format!(
                        "operators `{}` and `{}` cannot be mixed without parentheses: \
                         {} and {} are unrelated operator groups",
                        side.op_name.unwrap_or("?"),
                        op.as_str(),
                        inner.as_str(),
                        outer.as_str(),
                    ),
                ));
            }
        }
        let span = lhs.expr.span().to(rhs.expr.span());
        Ok(Parsed {
            expr: Expr::Binary {
                op,
                lhs: Box::new(lhs.expr),
                rhs: Box::new(rhs.expr),
                span,
            },
            group: Some(outer),
            unary: None,
            op_name: Some(op.as_str()),
        })
    }
}

/// An expression together with the operator group it sits in.
///
/// The group is *not* a property of the AST node: a parenthesised expression
/// carries `None`, because parentheses are exactly what makes a group stop
/// mattering. That is why this is a parser-local type and not a method on
/// `Expr`.
struct Parsed {
    expr: Expr,
    group: Option<Group>,
    /// Set when the outermost, unparenthesised form is a unary operator.
    unary: Option<UnOp>,
    op_name: Option<&'static str>,
}

impl Parsed {
    fn primary(expr: Expr) -> Parsed {
        Parsed {
            expr,
            group: None,
            unary: None,
            op_name: None,
        }
    }

    fn unary(op: UnOp, op_span: Span, operand: Expr) -> Parsed {
        let span = op_span.to(operand.span());
        Parsed {
            expr: Expr::Unary {
                op,
                operand: Box::new(operand),
                span,
            },
            group: Some(op.group()),
            unary: Some(op),
            op_name: Some(op.as_str()),
        }
    }
}

/// Between groups only three relations hold:
///
/// ```text
/// arithmetic    < comparison
/// concatenation < comparison
/// comparison    < boolean
/// ```
///
/// "Every other combination is a parse error and must be parenthesised."
fn permits(outer: Group, inner: Group) -> bool {
    outer == inner
        || matches!(
            (outer, inner),
            (Group::Comparison, Group::Arithmetic)
                | (Group::Comparison, Group::Concatenation)
                | (Group::Boolean, Group::Comparison)
        )
}

/// Which argument list is being read, which decides two things at once.
///
/// An atom's may hold `_` and may not hold an aggregate; a head's or a fact's
/// is the other way round. Two flags would have to be kept in step with each
/// other for no reason — there is one question here, and it is which position
/// this is.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Position {
    /// A rule head or a fact. Which of the two is not known until the parser
    /// looks for `<-`, so an aggregate is admitted here and refused for a fact
    /// in [`Parser::decl`].
    Head,
    Atom,
}

const WILDCARD_MISUSE: &str = "the wildcard `_` binds nothing and cannot be used here; it is legal only \
     in an atom's argument position";

/// "There shouldn't be conflicting typespecs for the same function or
/// relation."
///
/// A second typespec for one name is not a redefinition to merge or a later one
/// to prefer: it is two answers to one question, and this is the only place
/// that holds both. Without it `infer`'s spec table keeps whichever came last,
/// and the program compiles against a declaration its author may not have meant.
///
/// A relation's name may not be namespaced and a function's must be, so the two
/// kinds cannot collide with each other — one table over both is enough, and
/// the kind is carried only so the message can name it.
fn check_one_typespec_each(decls: &[Decl]) -> Result<(), Diagnostic> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for decl in decls {
        let (name, kind, span) = match decl {
            Decl::Spec(s) => (s.relation.as_str(), "relation", s.span),
            Decl::Function(f) => (f.name.as_str(), "function", f.span),
            Decl::Fact(_) | Decl::Rule(_) => continue,
        };
        if !seen.insert(name) {
            return Err(Diagnostic::error(
                Pass::Parse,
                span,
                format!("`{name}` already has a {kind} typespec, and a name has one"),
            ));
        }
    }
    Ok(())
}

fn check_duplicate_columns(args: &[KvArg]) -> Result<(), Diagnostic> {
    for (i, a) in args.iter().enumerate() {
        if args[..i].iter().any(|b| b.column == a.column) {
            return Err(Diagnostic::error(
                Pass::Parse,
                a.span,
                format!("duplicate column `{}`", a.column),
            ));
        }
    }
    Ok(())
}
