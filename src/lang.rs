//! Lexing and parsing the source language.
//!
//! The grammar is in `docs/design/language.md`. This is a hand-written
//! recursive-descent parser producing an untyped AST: field accesses still carry
//! names, which `typecheck` resolves to positional indices.

use crate::diag::{Diagnostic, Pass, Span};
use crate::value::{BatchType, TypeDesc};

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decl {
    /// `name := <rhs>`
    Node { name: String, rhs: Rhs, span: Span },
    /// `name :: batch_type`
    TypeSpec { name: String, ty: BatchType, span: Span },
    /// `circuit name(label: internal, ...) { ... }`
    Circuit(CircuitDef),
}

/// What a node declaration is defined as.
#[derive(Debug, Clone, PartialEq)]
pub enum Rhs {
    Op(OpCall),
    /// `name(label: arg, ...)` — expands the circuit body here.
    Instantiate(Instantiation),
    /// `fixpoint(name(label: arg, ...))` — iterates it to convergence.
    Fixpoint(Instantiation),
    /// `other` or `inst.node` — an alias, which is how a circuit's output gets
    /// a name that can be selected as a program output.
    Ref(NodeRef),
}

/// A named, parameterised block of declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct CircuitDef {
    pub name: String,
    /// `(label, internal)`: the keyword used at the call site, and the name the
    /// body uses for the bound argument.
    pub params: Vec<(String, String)>,
    pub body: Vec<Decl>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instantiation {
    pub circuit: String,
    pub args: Vec<(String, Arg)>,
    pub span: Span,
}

/// A dotted path: `name`, `instance.node`, or `outer.inner.node` when circuits
/// nest.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRef {
    pub path: Vec<String>,
    pub span: Span,
}

impl NodeRef {
    /// The name a node is registered under, so a reference is an ordinary
    /// lookup rather than a second resolution mechanism.
    pub fn key(&self) -> String {
        self.path.join(".")
    }

    /// The first segment — the name that introduces whatever the rest selects.
    pub fn base(&self) -> &String {
        &self.path[0]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpCall {
    pub op: String,
    pub args: Vec<Arg>,
    /// The operator name's own position, which a nested call needs for its
    /// diagnostics and its synthesized name.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// A reference to another node, or a bare aggregator name. Which one it is
    /// depends on the operator, so the parser does not try to decide.
    Name(String),
    Str(String),
    Fun(FunLit),
    /// A nested operator call, which the type checker turns into an anonymous
    /// node. `Vec<Arg>` inside `OpCall` breaks the recursion, so no `Box`.
    Op(OpCall),
    /// `instance.node`.
    Field(NodeRef),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunLit {
    pub params: Vec<String>,
    pub body: Expr,
}

/// An expression together with the source it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// The `NONE` literal: a value meaning "this field has none".
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A bound parameter.
    Var(String),
    /// `e.name` — resolved to a positional index during type checking.
    Field(Box<Expr>, String),
    /// `record(a: x, b: y)`
    Record(Vec<(String, Expr)>),
    /// `(a, b)`. Only legal as the body of a `map_index` or `join_index`
    /// function, or as an element of a `flat_map_index` list.
    ///
    /// This is *syntax, not a value*: the type checker destructures it into
    /// independent expressions, and no pair is ever streamed. There is no
    /// corresponding `DynValue` variant.
    Tuple(Vec<Expr>),
    /// `[a, b]`. Only legal as the body of a `flat_map` or `flat_map_index`
    /// function, and syntax rather than a value for the same reason: it says
    /// how many rows the operator emits per input row.
    List(Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

type PResult<T> = Result<T, Diagnostic>;

fn parse_error<T>(span: Span, message: impl Into<String>) -> PResult<T> {
    Err(Diagnostic::error(Pass::Parse, span, message))
}

// ---------------------------------------------------------------------------
// Reserved words
// ---------------------------------------------------------------------------

/// Type constructors and namespaces.
const TYPE_NAMES: &[&str] = &[
    "bool", "i64", "f64", "String", "optional", "record", "sql", "zset", "indexed_zset",
];

/// Literals and keywords. `if`/`then`/`else` are reserved although the language
/// has no conditionals yet, so adding them later is not a breaking change.
const KEYWORDS: &[&str] = &[
    "true", "false", "NONE", "null", "fun", "and", "or", "not", "if", "then", "else", "circuit",
    "fixpoint",
];

/// Whether `name` is reserved, and so may not name a node or a parameter.
///
/// Assembled from the operator, aggregator and builtin lists rather than
/// duplicating them, because a reserved list that drifts from the real names is
/// worse than none.
pub fn is_reserved(name: &str) -> bool {
    crate::typecheck::OPERATORS.contains(&name)
        || crate::typecheck::AGGREGATORS.contains(&name)
        || crate::expr::Builtin::ALL.contains(&name)
        || TYPE_NAMES.contains(&name)
        || KEYWORDS.contains(&name)
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    /// `:=`
    Assign,
    /// `::`
    HasType,
    /// `->`
    Arrow,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    Pipe,
    Op(BinOp),
    Minus,
    Eof,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    /// Byte offset of the current line's first character, so a column is just
    /// `pos - line_start`.
    line_start: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer { src: src.as_bytes(), pos: 0, line: 1, line_start: 0 }
    }

    /// 1-based column of the current position.
    fn column(&self) -> usize {
        self.pos - self.line_start + 1
    }

    fn here(&self) -> Span {
        Span::new(self.line, self.column(), 0)
    }

    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        parse_error(self.here(), msg)
    }

    fn peek_byte(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek_byte() {
                Some(b'\n') => {
                    self.line += 1;
                    self.pos += 1;
                    self.line_start = self.pos;
                }
                Some(c) if c.is_ascii_whitespace() => self.pos += 1,
                Some(b'#') => {
                    while let Some(c) = self.peek_byte() {
                        if c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// A token plus the span it occupies, measured from after trivia to the
    /// position the scan ended at.
    fn next_token(&mut self) -> PResult<(Tok, Span)> {
        self.skip_trivia();
        let (line, column, start) = (self.line, self.column(), self.pos);
        let tok = self.scan()?;
        Ok((tok, Span::new(line, column, self.pos - start)))
    }

    fn scan(&mut self) -> PResult<Tok> {
        let Some(c) = self.peek_byte() else {
            return Ok(Tok::Eof);
        };

        // Multi-character punctuation first.
        let two = self.src.get(self.pos..self.pos + 2);
        if let Some(t) = two {
            let tok = match t {
                b":=" => Some(Tok::Assign),
                b"::" => Some(Tok::HasType),
                b"->" => Some(Tok::Arrow),
                b"==" => Some(Tok::Op(BinOp::Eq)),
                b"!=" => Some(Tok::Op(BinOp::Ne)),
                b"<=" => Some(Tok::Op(BinOp::Le)),
                b">=" => Some(Tok::Op(BinOp::Ge)),
                _ => None,
            };
            if let Some(tok) = tok {
                self.pos += 2;
                return Ok(tok);
            }
        }

        let single = match c {
            b'(' => Some(Tok::LParen),
            b')' => Some(Tok::RParen),
            b'[' => Some(Tok::LBracket),
            b']' => Some(Tok::RBracket),
            b'{' => Some(Tok::LBrace),
            b'}' => Some(Tok::RBrace),
            b',' => Some(Tok::Comma),
            b':' => Some(Tok::Colon),
            b'.' => Some(Tok::Dot),
            b'|' => Some(Tok::Pipe),
            b'+' => Some(Tok::Op(BinOp::Add)),
            b'*' => Some(Tok::Op(BinOp::Mul)),
            b'/' => Some(Tok::Op(BinOp::Div)),
            b'%' => Some(Tok::Op(BinOp::Rem)),
            b'<' => Some(Tok::Op(BinOp::Lt)),
            b'>' => Some(Tok::Op(BinOp::Gt)),
            b'-' => Some(Tok::Minus),
            _ => None,
        };
        if let Some(tok) = single {
            self.pos += 1;
            return Ok(tok);
        }

        if c == b'"' {
            return self.lex_string().map(Tok::Str);
        }
        if c.is_ascii_digit() {
            return self.lex_number();
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.pos;
            while let Some(c) = self.peek_byte() {
                if c.is_ascii_alphanumeric() || c == b'_' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let word = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
            return Ok(Tok::Ident(word));
        }

        self.err(format!("unexpected character {:?}", c as char))
    }

    fn lex_string(&mut self) -> PResult<String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            match self.peek_byte() {
                None | Some(b'\n') => return self.err("unterminated string literal"),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let esc = match self.peek_byte() {
                        Some(b'n') => '\n',
                        Some(b't') => '\t',
                        Some(b'\\') => '\\',
                        Some(b'"') => '"',
                        other => {
                            return self.err(format!(
                                "unknown escape \\{}",
                                other.map(|c| c as char).unwrap_or('?')
                            ))
                        }
                    };
                    out.push(esc);
                    self.pos += 1;
                }
                Some(c) => {
                    out.push(c as char);
                    self.pos += 1;
                }
            }
        }
    }

    fn lex_number(&mut self) -> PResult<Tok> {
        let start = self.pos;
        while self.peek_byte().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let mut is_float = false;
        // A `.` only starts a fraction when a digit follows, so `x.0` on a tuple
        // and `1.5` do not collide.
        if self.peek_byte() == Some(b'.')
            && self.src.get(self.pos + 1).is_some_and(|c| c.is_ascii_digit())
        {
            is_float = true;
            self.pos += 1;
            while self.peek_byte().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = String::from_utf8_lossy(&self.src[start..self.pos]);
        if is_float {
            text.parse::<f64>()
                .map(Tok::Float)
                .map_err(|e| Diagnostic::error(Pass::Parse, self.here(), e.to_string()))
        } else {
            text.parse::<i64>()
                .map(Tok::Int)
                .map_err(|e| Diagnostic::error(Pass::Parse, self.here(), e.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub fn parse(src: &str) -> PResult<Program> {
    let mut toks = Vec::new();
    let mut lexer = Lexer::new(src);
    loop {
        let (tok, span) = lexer.next_token()?;
        let eof = tok == Tok::Eof;
        toks.push((tok, span));
        if eof {
            break;
        }
    }
    let start = toks[0].1;
    Parser { toks, pos: 0, prev: start }.program()
}

struct Parser {
    toks: Vec<(Tok, Span)>,
    pos: usize,
    /// Span of the most recently consumed token, so a production can close a
    /// span over everything it consumed.
    prev: Span,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].0
    }

    fn span(&self) -> Span {
        self.toks[self.pos].1
    }

    fn peek_ahead(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].0
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].0.clone();
        self.prev = self.toks[self.pos].1;
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    /// Builds an expression spanning from `start` through the last token
    /// consumed.
    fn mk(&self, start: Span, kind: ExprKind) -> Expr {
        Expr { kind, span: start.to(self.prev) }
    }

    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        parse_error(self.span(), msg)
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == want {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: &Tok, what: &str) -> PResult<()> {
        if self.eat(want) {
            Ok(())
        } else {
            self.err(format!("expected {what}, found {}", describe(self.peek())))
        }
    }

    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                Ok(name)
            }
            other => self.err(format!("expected an identifier, found {}", describe(&other))),
        }
    }

    /// A field name: a bare identifier, or a quoted string for names that are
    /// not valid identifiers.
    fn field_name(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                Ok(name)
            }
            Tok::Str(name) => {
                self.bump();
                Ok(name)
            }
            other => self.err(format!("expected a field name, found {}", describe(&other))),
        }
    }

    fn program(&mut self) -> PResult<Program> {
        let mut decls = Vec::new();
        while self.peek() != &Tok::Eof {
            decls.push(self.decl()?);
        }
        Ok(Program { decls })
    }

    fn decl(&mut self) -> PResult<Decl> {
        let span = self.span();
        if matches!(self.peek(), Tok::Ident(w) if w == "circuit") {
            return self.circuit_def().map(Decl::Circuit);
        }
        let name = self.ident()?;
        if is_reserved(&name) {
            return parse_error(
                span,
                format!("`{name}` is a reserved word and cannot name a node"),
            );
        }
        if self.eat(&Tok::HasType) {
            let ty = self.batch_type()?;
            Ok(Decl::TypeSpec { name, ty, span })
        } else if self.eat(&Tok::Assign) {
            let rhs = self.rhs()?;
            Ok(Decl::Node { name, rhs, span })
        } else {
            self.err(format!(
                "expected `:=` or `::` after `{name}`, found {}",
                describe(self.peek())
            ))
        }
    }

    /// `circuit name(label: internal, ...) { ... }`
    fn circuit_def(&mut self) -> PResult<CircuitDef> {
        let span = self.span();
        self.bump(); // `circuit`
        let name = self.ident()?;
        if is_reserved(&name) {
            return parse_error(span, format!("`{name}` is a reserved word and cannot name a circuit"));
        }
        self.expect(&Tok::LParen, "`(` after a circuit name")?;
        let mut params: Vec<(String, String)> = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                let pspan = self.span();
                let label = self.ident()?;
                self.expect(&Tok::Colon, "`:` between a parameter's label and its internal name")?;
                let internal = self.ident()?;
                if is_reserved(&internal) {
                    return parse_error(
                        pspan,
                        format!("`{internal}` is a reserved word and cannot name a parameter"),
                    );
                }
                if params.iter().any(|(l, _)| *l == label) {
                    return parse_error(pspan, format!("duplicate parameter `{label}`"));
                }
                params.push((label, internal));
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "`,` or `)` in a parameter list")?;
                break;
            }
        }
        self.expect(&Tok::LBrace, "`{` opening a circuit body")?;
        let mut body = Vec::new();
        while !self.eat(&Tok::RBrace) {
            if self.peek() == &Tok::Eof {
                return parse_error(span, "unterminated circuit body: expected `}`");
            }
            match self.decl()? {
                d @ (Decl::Node { .. } | Decl::TypeSpec { .. }) => body.push(d),
                Decl::Circuit(c) => {
                    return parse_error(c.span, "a circuit cannot be defined inside another");
                }
            }
        }
        Ok(CircuitDef { name, params, body, span })
    }

    /// Distinguishes the four right-hand sides by lookahead.
    fn rhs(&mut self) -> PResult<Rhs> {
        if matches!(self.peek(), Tok::Ident(w) if w == "fixpoint") {
            self.bump();
            self.expect(&Tok::LParen, "`(` after `fixpoint`")?;
            let inst = self.instantiation()?;
            self.expect(&Tok::RParen, "`)` closing `fixpoint`")?;
            return Ok(Rhs::Fixpoint(inst));
        }
        match (self.peek(), self.peek_ahead(1)) {
            // `name(label: ...)` is an instantiation; `name(a, ...)` an operator.
            (Tok::Ident(_), Tok::LParen)
                if matches!(self.peek_ahead(2), Tok::Ident(_))
                    && matches!(self.peek_ahead(3), Tok::Colon) =>
            {
                Ok(Rhs::Instantiate(self.instantiation()?))
            }
            (Tok::Ident(_), Tok::LParen) => Ok(Rhs::Op(self.op_call()?)),
            (Tok::Ident(_), _) => Ok(Rhs::Ref(self.node_ref()?)),
            (other, _) => {
                self.err(format!("expected a definition, found {}", describe(other)))
            }
        }
    }

    fn instantiation(&mut self) -> PResult<Instantiation> {
        let span = self.span();
        let circuit = self.ident()?;
        self.expect(&Tok::LParen, "`(` after a circuit name")?;
        let mut args: Vec<(String, Arg)> = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                let aspan = self.span();
                let label = self.ident()?;
                self.expect(&Tok::Colon, "`:` after an argument label")?;
                if args.iter().any(|(l, _)| *l == label) {
                    return parse_error(aspan, format!("duplicate argument `{label}`"));
                }
                args.push((label, self.arg()?));
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "`,` or `)` in an argument list")?;
                break;
            }
        }
        Ok(Instantiation { circuit, args, span })
    }

    fn node_ref(&mut self) -> PResult<NodeRef> {
        let span = self.span();
        let mut path = vec![self.ident()?];
        while self.eat(&Tok::Dot) {
            path.push(self.ident()?);
        }
        Ok(NodeRef { path, span })
    }

    fn op_call(&mut self) -> PResult<OpCall> {
        let span = self.span();
        let op = self.ident()?;
        self.expect(&Tok::LParen, "`(` after an operator name")?;
        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.arg()?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "`,` or `)` in an argument list")?;
                break;
            }
        }
        Ok(OpCall { op, args, span })
    }

    fn arg(&mut self) -> PResult<Arg> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                Ok(Arg::Str(s))
            }
            Tok::Ident(name) if name == "fun" => {
                self.bump();
                Ok(Arg::Fun(self.fun_literal()?))
            }
            // An identifier followed by `(` is a nested operator call;
            // otherwise it names a node, or is a bare aggregator.
            Tok::Ident(_) if matches!(self.peek_ahead(1), Tok::LParen) => {
                Ok(Arg::Op(self.op_call()?))
            }
            Tok::Ident(_) if matches!(self.peek_ahead(1), Tok::Dot) => {
                Ok(Arg::Field(self.node_ref()?))
            }
            Tok::Ident(name) => {
                self.bump();
                Ok(Arg::Name(name))
            }
            other => self.err(format!("expected an argument, found {}", describe(&other))),
        }
    }

    /// `fun((a, b) -> expr)`, with the leading `fun` already consumed.
    fn fun_literal(&mut self) -> PResult<FunLit> {
        self.expect(&Tok::LParen, "`(` after `fun`")?;
        self.expect(&Tok::LParen, "`(` before the parameter list")?;
        let mut params = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                let span = self.span();
                let name = self.ident()?;
                if is_reserved(&name) {
                    return parse_error(
                        span,
                        format!("`{name}` is a reserved word and cannot name a parameter"),
                    );
                }
                params.push(name);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "`,` or `)` in a parameter list")?;
                break;
            }
        }
        self.expect(&Tok::Arrow, "`->` after the parameter list")?;
        let body = self.expr()?;
        self.expect(&Tok::RParen, "`)` closing `fun`")?;
        Ok(FunLit { params, body })
    }

    // -- types --------------------------------------------------------------

    fn batch_type(&mut self) -> PResult<BatchType> {
        let name = self.ident()?;
        self.expect(&Tok::LParen, "`(` after a batch type name")?;
        match name.as_str() {
            "zset" => {
                let t = self.value_type()?;
                self.expect(&Tok::RParen, "`)` closing zset")?;
                Ok(BatchType::ZSet(t))
            }
            "indexed_zset" => {
                let k = self.value_type()?;
                self.expect(&Tok::Comma, "`,` between the key and value types")?;
                let v = self.value_type()?;
                self.expect(&Tok::RParen, "`)` closing indexed_zset")?;
                Ok(BatchType::IndexedZSet(k, v))
            }
            other => self.err(format!(
                "unknown batch type `{other}`, expected zset or indexed_zset"
            )),
        }
    }

    fn value_type(&mut self) -> PResult<TypeDesc> {
        let name = self.ident()?;

        // `sql.Foo`
        if name == "sql" {
            self.expect(&Tok::Dot, "`.` after `sql`")?;
            let sql_name = self.ident()?;
            return match sql_name.as_str() {
                "SqlString" => Ok(TypeDesc::SqlString),
                other => self.err(format!(
                    "`sql.{other}` is not supported yet; this build has sql.SqlString only"
                )),
            };
        }

        match name.as_str() {
            "bool" => Ok(TypeDesc::Bool),
            "i64" => Ok(TypeDesc::I64),
            "f64" => Ok(TypeDesc::F64),
            "String" => Ok(TypeDesc::String),
            "optional" => {
                self.expect(&Tok::LParen, "`(` after optional")?;
                let inner = self.value_type()?;
                self.expect(&Tok::RParen, "`)` closing optional")?;
                if inner.is_optional() {
                    // There is one null, so a doubly-nullable type has no
                    // values the singly-nullable one lacks. Rejecting it beats
                    // silently flattening it.
                    return self.err("`optional(optional(T))` is not a distinct type; use `optional(T)`");
                }
                Ok(TypeDesc::Optional(Box::new(inner)))
            }
            "record" => {
                self.expect(&Tok::LParen, "`(` after record")?;
                let mut fields = Vec::new();
                if !self.eat(&Tok::RParen) {
                    loop {
                        let fname = self.field_name()?;
                        self.expect(&Tok::Colon, "`:` after a field name")?;
                        let fty = self.value_type()?;
                        fields.push((fname, fty));
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RParen, "`,` or `)` in a record type")?;
                        break;
                    }
                }
                Ok(TypeDesc::Record(fields))
            }
            other => self.err(format!("unknown type `{other}`")),
        }
    }

    // -- expressions --------------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.binary(0)
    }

    /// Precedence climbing. Lower number binds less tightly.
    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let Some((op, prec)) = self.peek_binop() else {
                break;
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            // All binary operators here are left-associative.
            let rhs = self.binary(prec + 1)?;
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span };
        }
        Ok(lhs)
    }

    fn peek_binop(&self) -> Option<(BinOp, u8)> {
        let op = match self.peek() {
            Tok::Op(op) => *op,
            Tok::Minus => BinOp::Sub,
            Tok::Ident(w) if w == "and" => BinOp::And,
            Tok::Ident(w) if w == "or" => BinOp::Or,
            _ => return None,
        };
        let prec = match op {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
            BinOp::Add | BinOp::Sub => 4,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 5,
        };
        Some((op, prec))
    }

    fn unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.eat(&Tok::Minus) {
            let e = self.unary()?;
            return Ok(self.mk(start, ExprKind::Unary(UnOp::Neg, Box::new(e))));
        }
        if matches!(self.peek(), Tok::Ident(w) if w == "not") {
            self.bump();
            let e = self.unary()?;
            return Ok(self.mk(start, ExprKind::Unary(UnOp::Not, Box::new(e))));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        while self.eat(&Tok::Dot) {
            let name = self.field_name()?;
            // The span covers the whole access, so `r.nope` is reported rather
            // than just the field name.
            let span = e.span.to(self.prev);
            e = Expr { kind: ExprKind::Field(Box::new(e), name), span };
        }
        Ok(e)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Ok(self.mk(start, ExprKind::Int(v)))
            }
            Tok::Float(v) => {
                self.bump();
                Ok(self.mk(start, ExprKind::Float(v)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(self.mk(start, ExprKind::Str(s)))
            }
            Tok::LBracket => {
                self.bump();
                let mut items = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    loop {
                        items.push(self.expr()?);
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RBracket, "`,` or `]` in a list")?;
                        break;
                    }
                }
                Ok(self.mk(start, ExprKind::List(items)))
            }
            Tok::LParen => {
                self.bump();
                let first = self.expr()?;
                if self.eat(&Tok::RParen) {
                    return Ok(first); // parenthesized, not a tuple
                }
                let mut items = vec![first];
                loop {
                    self.expect(&Tok::Comma, "`,` or `)` in a tuple")?;
                    items.push(self.expr()?);
                    if self.eat(&Tok::RParen) {
                        break;
                    }
                }
                Ok(self.mk(start, ExprKind::Tuple(items)))
            }
            Tok::Ident(word) => self.ident_expr(word, start),
            other => self.err(format!("expected an expression, found {}", describe(&other))),
        }
    }

    fn ident_expr(&mut self, word: String, start: Span) -> PResult<Expr> {
        self.bump();
        match word.as_str() {
            "true" => return Ok(self.mk(start, ExprKind::Bool(true))),
            "false" => return Ok(self.mk(start, ExprKind::Bool(false))),
            "NONE" => return Ok(self.mk(start, ExprKind::None)),
            "null" => {
                return parse_error(
                    start,
                    "`null` is reserved for the JSON null value inside `sql.Variant`, \
                     which is not implemented; write `NONE` for a missing value",
                );
            }
            "record" => {
                self.expect(&Tok::LParen, "`(` after record")?;
                let mut fields: Vec<(String, Expr)> = Vec::new();
                if !self.eat(&Tok::RParen) {
                    loop {
                        let name = self.field_name()?;
                        self.expect(&Tok::Colon, "`:` after a field name")?;
                        let value = self.expr()?;
                        if fields.iter().any(|(n, _)| *n == name) {
                            return self.err(format!("duplicate field `{name}` in record"));
                        }
                        fields.push((name, value));
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        self.expect(&Tok::RParen, "`,` or `)` in a record literal")?;
                        break;
                    }
                }
                return Ok(self.mk(start, ExprKind::Record(fields)));
            }
            _ => {}
        }

        // A call if followed by `(`, otherwise a parameter reference. A bare
        // reserved word can never resolve, since parameters cannot be named
        // one, so say so rather than failing later and less clearly.
        if !matches!(self.peek(), Tok::LParen) && is_reserved(&word) {
            return parse_error(start, format!("`{word}` is a reserved word"));
        }
        if self.eat(&Tok::LParen) {
            let mut args = Vec::new();
            if !self.eat(&Tok::RParen) {
                loop {
                    args.push(self.expr()?);
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    self.expect(&Tok::RParen, "`,` or `)` in a call")?;
                    break;
                }
            }
            Ok(self.mk(start, ExprKind::Call(word, args)))
        } else {
            Ok(self.mk(start, ExprKind::Var(word)))
        }
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(s) => format!("`{s}`"),
        Tok::Str(s) => format!("string {s:?}"),
        Tok::Int(v) => format!("`{v}`"),
        Tok::Float(v) => format!("`{v}`"),
        Tok::Assign => "`:=`".into(),
        Tok::HasType => "`::`".into(),
        Tok::Arrow => "`->`".into(),
        Tok::LParen => "`(`".into(),
        Tok::RParen => "`)`".into(),
        Tok::LBracket => "`[`".into(),
        Tok::RBracket => "`]`".into(),
        Tok::LBrace => "`{`".into(),
        Tok::RBrace => "`}`".into(),
        Tok::Comma => "`,`".into(),
        Tok::Colon => "`:`".into(),
        Tok::Dot => "`.`".into(),
        Tok::Pipe => "`|`".into(),
        Tok::Minus => "`-`".into(),
        Tok::Op(_) => "an operator".into(),
        Tok::Eof => "end of input".into(),
    }
}
