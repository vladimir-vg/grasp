//! Lexing and parsing the source language.
//!
//! The grammar is in `docs/design/language.md`. This is a hand-written
//! recursive-descent parser producing an untyped AST: field accesses still carry
//! names, which `typecheck` resolves to positional indices.

use crate::value::{BatchType, TypeDesc};
use std::fmt;

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decl {
    /// `name := op(args)`
    Node { name: String, op: OpCall, line: usize },
    /// `name :: batch_type`
    TypeSpec { name: String, ty: BatchType, line: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpCall {
    pub op: String,
    pub args: Vec<Arg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// A reference to another node, or a bare aggregator name. Which one it is
    /// depends on the operator, so the parser does not try to decide.
    Name(String),
    Str(String),
    Fun(FunLit),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunLit {
    pub params: Vec<String>,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Null,
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
    /// `(a, b)`. Only legal as the body of a `map_index` function.
    Tuple(Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

type PResult<T> = Result<T, ParseError>;

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
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer { src: src.as_bytes(), pos: 0, line: 1 }
    }

    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        Err(ParseError { message: msg.into(), line: self.line })
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

    fn next_token(&mut self) -> PResult<(Tok, usize)> {
        self.skip_trivia();
        let line = self.line;
        let Some(c) = self.peek_byte() else {
            return Ok((Tok::Eof, line));
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
                return Ok((tok, line));
            }
        }

        let single = match c {
            b'(' => Some(Tok::LParen),
            b')' => Some(Tok::RParen),
            b'[' => Some(Tok::LBracket),
            b']' => Some(Tok::RBracket),
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
            return Ok((tok, line));
        }

        if c == b'"' {
            return self.lex_string().map(|s| (Tok::Str(s), line));
        }
        if c.is_ascii_digit() {
            return self.lex_number().map(|t| (t, line));
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
            return Ok((Tok::Ident(word), line));
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
                .map_err(|e| ParseError { message: e.to_string(), line: self.line })
        } else {
            text.parse::<i64>()
                .map(Tok::Int)
                .map_err(|e| ParseError { message: e.to_string(), line: self.line })
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
        let (tok, line) = lexer.next_token()?;
        let eof = tok == Tok::Eof;
        toks.push((tok, line));
        if eof {
            break;
        }
    }
    Parser { toks, pos: 0 }.program()
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].0
    }

    fn line(&self) -> usize {
        self.toks[self.pos].1
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].0.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        Err(ParseError { message: msg.into(), line: self.line() })
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
        let line = self.line();
        let name = self.ident()?;
        if self.eat(&Tok::HasType) {
            let ty = self.batch_type()?;
            Ok(Decl::TypeSpec { name, ty, line })
        } else if self.eat(&Tok::Assign) {
            let op = self.op_call()?;
            Ok(Decl::Node { name, op, line })
        } else {
            self.err(format!(
                "expected `:=` or `::` after `{name}`, found {}",
                describe(self.peek())
            ))
        }
    }

    fn op_call(&mut self) -> PResult<OpCall> {
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
        Ok(OpCall { op, args })
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
                params.push(self.ident()?);
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
            "OrdZSet" => {
                let t = self.value_type()?;
                self.expect(&Tok::RParen, "`)` closing OrdZSet")?;
                Ok(BatchType::ZSet(t))
            }
            "OrdIndexedZSet" => {
                let k = self.value_type()?;
                self.expect(&Tok::Comma, "`,` between the key and value types")?;
                let v = self.value_type()?;
                self.expect(&Tok::RParen, "`)` closing OrdIndexedZSet")?;
                Ok(BatchType::IndexedZSet(k, v))
            }
            other => self.err(format!(
                "unknown batch type `{other}`, expected OrdZSet or OrdIndexedZSet"
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
            "Option" => {
                self.expect(&Tok::LParen, "`(` after Option")?;
                let inner = self.value_type()?;
                self.expect(&Tok::RParen, "`)` closing Option")?;
                Ok(TypeDesc::Option(Box::new(inner)))
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
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
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
        if self.eat(&Tok::Minus) {
            let e = self.unary()?;
            return Ok(Expr::Unary(UnOp::Neg, Box::new(e)));
        }
        if matches!(self.peek(), Tok::Ident(w) if w == "not") {
            self.bump();
            let e = self.unary()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(e)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        while self.eat(&Tok::Dot) {
            let name = self.field_name()?;
            e = Expr::Field(Box::new(e), name);
        }
        Ok(e)
    }

    fn primary(&mut self) -> PResult<Expr> {
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Ok(Expr::Int(v))
            }
            Tok::Float(v) => {
                self.bump();
                Ok(Expr::Float(v))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
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
                Ok(Expr::Tuple(items))
            }
            Tok::Ident(word) => self.ident_expr(word),
            other => self.err(format!("expected an expression, found {}", describe(&other))),
        }
    }

    fn ident_expr(&mut self, word: String) -> PResult<Expr> {
        self.bump();
        match word.as_str() {
            "true" => return Ok(Expr::Bool(true)),
            "false" => return Ok(Expr::Bool(false)),
            "null" => return Ok(Expr::Null),
            "if" => {
                let cond = self.expr()?;
                if !matches!(self.peek(), Tok::Ident(w) if w == "then") {
                    return self.err("expected `then`");
                }
                self.bump();
                let then = self.expr()?;
                if !matches!(self.peek(), Tok::Ident(w) if w == "else") {
                    return self.err("expected `else`");
                }
                self.bump();
                let els = self.expr()?;
                return Ok(Expr::If(Box::new(cond), Box::new(then), Box::new(els)));
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
                return Ok(Expr::Record(fields));
            }
            _ => {}
        }

        // A call if followed by `(`, otherwise a parameter reference.
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
            Ok(Expr::Call(word, args))
        } else {
            Ok(Expr::Var(word))
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
        Tok::Comma => "`,`".into(),
        Tok::Colon => "`:`".into(),
        Tok::Dot => "`.`".into(),
        Tok::Pipe => "`|`".into(),
        Tok::Minus => "`-`".into(),
        Tok::Op(_) => "an operator".into(),
        Tok::Eof => "end of input".into(),
    }
}
