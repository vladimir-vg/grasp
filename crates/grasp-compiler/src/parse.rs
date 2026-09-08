//! The parser — `docs/grasp/syntax.md`.
//!
//! Recursive descent over the token stream, producing the AST that document
//! gives verbatim. Three things here are not the usual shape, and each is a
//! property of grasp rather than a choice:
//!
//! - **Operator groups are incomparable.** Precedence is defined within a group;
//!   between groups only three relations hold, and every other adjacency is an
//!   error. A binding-power table cannot say that, so each level checks the
//!   group of the operand it just parsed — see [`permits`].
//! - **Indentation delimits a rule body and nothing else.** There is no layout
//!   stack; a body's width is set by its first statement and read off the token
//!   spans.
//! - **`name(` at statement level is an atom or a call depending on the name.**
//!   The one-name rule — a name is either a relation or a callable, never
//!   both — is what makes that decidable without lookahead.

use crate::ast::*;
use crate::diag::{Diagnostic, Pass, Span};
use crate::lex::{Tok, Token, lex};

// ---------------------------------------------------------------------------
// Reserved names — `docs/grasp/syntax.md`, "Reserved words".
// ---------------------------------------------------------------------------

/// Keywords never reach the parser as identifiers; the lexer has already turned
/// them into their own tokens. The list is here so a diagnostic can name them
/// and so `tests/reserved.rs` can pin it against the specification.
pub const KEYWORDS: &[&str] = &["not", "and", "or", "input", "true", "false", "NONE"];

pub const TYPE_NAMES: &[&str] = &[
    "boolean", "i64", "f64", "string", "json", "optional", "record", "array", "dict", "relation",
];

pub const AGGREGATORS: &[&str] = &["sum", "count", "min", "max", "avg"];

/// The callables — `docs/grasp/semantics.md`, "Builtins". Not reserved as such:
/// a *column* may be called `length`. They are unavailable as relation names by
/// the one-name rule, which is a different check.
pub const BUILTINS: &[&str] = &[
    "abs", "floor", "ceil", "round", "length", "concat", "lower", "upper", "trim", "coalesce",
    "if", "keys", "entries",
];

/// "They are held now so that the standard library can grow into them without
/// taking names a program was already using."
pub const RESERVED_NAMESPACES: &[&str] = &[
    "string", "array", "dict", "record", "json", "agg", "temporal",
];

/// Reserved as a relation, variable or column name.
pub fn is_reserved(name: &str) -> bool {
    KEYWORDS.contains(&name) || TYPE_NAMES.contains(&name) || AGGREGATORS.contains(&name)
}

fn namespace_of(name: &str) -> Option<&str> {
    name.split_once(':').map(|(head, _)| head)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn parse(source: &str) -> Result<Program, Diagnostic> {
    let tokens = lex(source)?;
    let mut p = Parser {
        toks: &tokens,
        pos: 0,
    };
    p.program()
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
        Ok(decls)
    }

    fn decl(&mut self) -> Result<Decl, Diagnostic> {
        let start = self.here();
        let (name, name_span) = self.expect_ident("a relation name")?;
        self.check_relation_name(&name, name_span)?;

        if self.at(&Tok::Annot) {
            return Ok(Decl::Spec(self.spec(name, start)?));
        }

        self.expect(&Tok::LParen, "`(` or `::`")?;
        let args = self.kv_args(&Tok::RParen)?;
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

        // A fact. The next declaration must start its own line, which is what
        // makes a missing `<-` a diagnostic rather than a silent second fact.
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
        // "A name is either a relation or a callable, never both." This is what
        // lets body-statement position resolve `name(` without lookahead.
        if BUILTINS.contains(&name) {
            return Err(self.error(
                span,
                format!("`{name}` is a builtin: a name cannot be both a relation and a callable"),
            ));
        }
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
        self.expect(&Tok::Annot, "`::`")?;
        let (kw, kw_span) = self.expect_ident("`relation`")?;
        if kw != "relation" {
            return Err(self.error(kw_span, format!("expected `relation`, found `{kw}`")));
        }
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

    // -- types --------------------------------------------------------------

    fn ty(&mut self) -> Result<Type, Diagnostic> {
        let (name, span) = self.expect_ident("a type")?;
        match name.as_str() {
            "boolean" => Ok(Type::Boolean),
            "i64" => Ok(Type::I64),
            "f64" => Ok(Type::F64),
            "string" => Ok(Type::String),
            "json" => Ok(Type::Json),
            "optional" => {
                self.expect(&Tok::LParen, "`(`")?;
                let inner = self.ty()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Type::Optional(Box::new(inner)))
            }
            "array" => {
                self.expect(&Tok::LParen, "`(`")?;
                let inner = self.ty()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(Type::Array(Box::new(inner)))
            }
            "dict" => {
                self.expect(&Tok::LParen, "`(`")?;
                let key = self.ty()?;
                self.expect(&Tok::Comma, "`,`")?;
                let value = self.ty()?;
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
                    fields.push((field, self.ty()?));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)` or `,`")?;
                Ok(Type::Record(fields))
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
                // for every name that can be a relation. A builtin cannot, by
                // the one-name rule, so `length(s) > 3` is a filter.
                Some(Tok::LParen) if !BUILTINS.contains(&name.as_str()) => {
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
        let args = self.kv_args(&Tok::RParen)?;
        let close = self.expect(&Tok::RParen, "`)` or `,`")?;
        check_duplicate_columns(&args)?;
        Ok((args, close.span))
    }

    /// `kv_arg ::= name ":" expr | name ":" | name ":" "_"`
    fn kv_args(&mut self, terminator: &Tok) -> Result<Vec<KvArg>, Diagnostic> {
        let mut args = Vec::new();
        while !self.at(terminator) {
            let start = self.here();
            let (column, col_span) = self.expect_ident("a column name")?;
            self.check_column_name(&column, col_span)?;
            self.expect(&Tok::Colon, "`:`")?;

            let value = if self.at(&Tok::Underscore) {
                // "legal only in an atom's argument position" — which is here.
                Arg::Wildcard(self.bump().span)
            } else if self.at(&Tok::Comma) || self.at(terminator) {
                // The shorthand `x:` means `x: x`. Desugaring makes that
                // explicit; the AST records what was written.
                self.check_variable_name(&column, col_span)?;
                Arg::Expr(Expr::Var {
                    name: column.clone(),
                    span: col_span,
                })
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

    /// `pat_field ::= dict_key ":" variable`, then an optional `**` rest.
    ///
    /// "its values must be variables, so `{a: f(1)} := d` is rejected there
    /// rather than by the grammar" — which is this function.
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
        // aggregate ::= aggregator "<" [expr] ">"
        if let Some(Tok::Ident(name)) = self.kind()
            && let Some(agg) = Aggregator::from_name(name)
            && self.kind_at(1) == Some(&Tok::Lt)
        {
            let start = self.bump().span; // the aggregator
            self.bump(); // `<`
            // Below the comparison level, or the closing `>` is taken as a
            // greater-than: `sum<r>` would read as `sum<(r > …)`. A comparison
            // inside an aggregate must be parenthesised, which is the same
            // answer grasp gives everywhere else it declines to guess.
            let arg = if self.at(&Tok::Gt) {
                None
            } else {
                Some(self.cat_expr()?.expr)
            };
            let close = self.expect(&Tok::Gt, "`>`")?;
            return Ok(Rhs::Aggregate {
                function: agg,
                arg,
                span: start.to(close.span),
            });
        }

        Ok(Rhs::Expr(self.expr()?.expr))
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

    fn postfix(&mut self) -> Result<Parsed, Diagnostic> {
        let mut base = self.primary()?;
        while self.at(&Tok::Dot) {
            self.bump();
            let (name, name_span) = self.expect_ident("a field name")?;
            let span = base.expr.span().to(name_span);
            base = Parsed::primary(Expr::Field {
                base: Box::new(base.expr),
                name,
                span,
            });
        }
        Ok(base)
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
                if name == "record" && self.kind_at(1) == Some(&Tok::LParen) {
                    return self.record_literal(start);
                }
                if self.kind_at(1) == Some(&Tok::LParen) {
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

const WILDCARD_MISUSE: &str = "the wildcard `_` binds nothing and cannot be used here; it is legal only \
     in an atom's argument position";

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
