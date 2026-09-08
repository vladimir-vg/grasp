//! Tokens — `docs/grasp/syntax.md`, "Lexical structure".
//!
//! Whitespace and comments are dropped, but each token records whether it is
//! the first on its line and what column it starts at. That is all the parser
//! needs: indentation delimits a multiline rule body and nothing else, so there
//! is no INDENT/DEDENT to synthesise and no layout stack to keep.

use crate::diag::{Diagnostic, Pass, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// An identifier, possibly namespace-qualified (`string:length`). The
    /// segments are kept joined; `is_qualified` splits when that matters.
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),

    // Keywords. Type names and aggregators are *reserved* but not keywords —
    // they arrive as `Ident` and are recognised by name, because `record(…)`
    // is a type, a literal and a pattern depending on where it stands.
    Not,
    And,
    Or,
    Input,
    True,
    False,
    None,

    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    /// `_`, the wildcard. `_x` is an ordinary identifier.
    Underscore,

    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusPlus,

    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    /// `:=`
    Assign,
    /// `::`
    Annot,
    /// `<-`
    Arrow,
    /// `=>`
    FatArrow,
    /// `**`
    StarStar,
}

impl Tok {
    /// How the token is named in a diagnostic.
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Int(n) => format!("`{n}`"),
            Tok::Float(f) => format!("`{f}`"),
            Tok::Str(_) => "a string".to_string(),
            Tok::Not => "`not`".to_string(),
            Tok::And => "`and`".to_string(),
            Tok::Or => "`or`".to_string(),
            Tok::Input => "`input`".to_string(),
            Tok::True => "`true`".to_string(),
            Tok::False => "`false`".to_string(),
            Tok::None => "`NONE`".to_string(),
            Tok::LParen => "`(`".to_string(),
            Tok::RParen => "`)`".to_string(),
            Tok::LBracket => "`[`".to_string(),
            Tok::RBracket => "`]`".to_string(),
            Tok::LBrace => "`{`".to_string(),
            Tok::RBrace => "`}`".to_string(),
            Tok::Comma => "`,`".to_string(),
            Tok::Colon => "`:`".to_string(),
            Tok::Dot => "`.`".to_string(),
            Tok::Underscore => "`_`".to_string(),
            Tok::Plus => "`+`".to_string(),
            Tok::Minus => "`-`".to_string(),
            Tok::Star => "`*`".to_string(),
            Tok::Slash => "`/`".to_string(),
            Tok::Percent => "`%`".to_string(),
            Tok::PlusPlus => "`++`".to_string(),
            Tok::Eq => "`=`".to_string(),
            Tok::Ne => "`!=`".to_string(),
            Tok::Lt => "`<`".to_string(),
            Tok::Le => "`<=`".to_string(),
            Tok::Gt => "`>`".to_string(),
            Tok::Ge => "`>=`".to_string(),
            Tok::Assign => "`:=`".to_string(),
            Tok::Annot => "`::`".to_string(),
            Tok::Arrow => "`<-`".to_string(),
            Tok::FatArrow => "`=>`".to_string(),
            Tok::StarStar => "`**`".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    pub span: Span,
    /// Whether this is the first token on its line. Only a rule body cares.
    pub first_on_line: bool,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    /// Byte offset of the current line's start, so a column is `pos - bol + 1`.
    bol: usize,
    line_has_token: bool,
    out: Vec<Token>,
}

/// Tokenise a whole program.
pub fn lex(source: &str) -> Result<Vec<Token>, Diagnostic> {
    let mut lx = Lexer {
        src: source.as_bytes(),
        pos: 0,
        line: 1,
        bol: 0,
        line_has_token: false,
        out: Vec::new(),
    };
    lx.run()?;
    Ok(lx.out)
}

impl<'a> Lexer<'a> {
    fn column(&self) -> usize {
        self.pos - self.bol + 1
    }

    fn span_from(&self, start: usize, start_col: usize) -> Span {
        Span::new(self.line, start_col, self.pos - start)
    }

    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::error(Pass::Parse, span, message)
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.src.get(self.pos + n).copied()
    }

    fn push(&mut self, kind: Tok, span: Span) {
        let first = !self.line_has_token;
        self.line_has_token = true;
        self.out.push(Token {
            kind,
            span,
            first_on_line: first,
        });
    }

    fn run(&mut self) -> Result<(), Diagnostic> {
        while let Some(c) = self.peek() {
            match c {
                b' ' | b'\t' | b'\r' => {
                    self.pos += 1;
                }
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.bol = self.pos;
                    self.line_has_token = false;
                }
                // A comment runs to the end of the line; the newline itself is
                // handled on the next turn, so a comment-only line still
                // resets `line_has_token` without ever setting it.
                b'#' => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.pos += 1;
                    }
                }
                b'"' => self.string()?,
                b'0'..=b'9' => self.number()?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.word(),
                _ => self.operator()?,
            }
        }
        Ok(())
    }

    fn string(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let start_col = self.column();
        self.pos += 1; // the opening quote
        let mut value = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error(
                    Span::new(self.line, start_col, self.pos - start),
                    "unterminated string",
                ));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                // A string does not span lines: reporting the newline is far
                // more useful than swallowing the rest of the program.
                b'\n' => {
                    return Err(self.error(
                        Span::new(self.line, start_col, self.pos - start),
                        "unterminated string",
                    ));
                }
                b'\\' => {
                    let esc = self.peek_at(1);
                    let decoded = match esc {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'n') => '\n',
                        Some(b't') => '\t',
                        Some(b'r') => '\r',
                        // "a backslash before anything else is an error, so a
                        //  typo is reported rather than silently dropped."
                        other => {
                            let shown = other.map(|b| b as char).unwrap_or(' ');
                            return Err(self.error(
                                Span::new(self.line, self.column(), 2),
                                format!("unknown escape `\\{shown}`"),
                            ));
                        }
                    };
                    value.push(decoded);
                    self.pos += 2;
                }
                _ => {
                    // Copy the whole UTF-8 sequence, not the leading byte.
                    let len = utf8_len(c);
                    let text =
                        std::str::from_utf8(&self.src[self.pos..self.pos + len]).map_err(|_| {
                            self.error(
                                Span::new(self.line, self.column(), len),
                                "invalid UTF-8 in a string",
                            )
                        })?;
                    value.push_str(text);
                    self.pos += len;
                }
            }
        }
        let span = self.span_from(start, start_col);
        self.push(Tok::Str(value), span);
        Ok(())
    }

    /// `[0-9]+` or `[0-9]+ "." [0-9]+ ([eE] [+-]? [0-9]+)?`.
    ///
    /// Numbers are unsigned tokens: `-5` is unary minus applied to `5`. A `.`
    /// not followed by a digit is left alone, so `1.foo` is a field access and
    /// `1.` is a parse error at the `.` rather than a malformed float here.
    fn number(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let start_col = self.column();
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if is_float && matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            } else {
                // Not an exponent after all — `1.0e` is `1.0` then `e`.
                self.pos = save;
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).expect("ascii");
        let span = self.span_from(start, start_col);
        let tok = if is_float {
            Tok::Float(
                text.parse()
                    .map_err(|_| self.error(span, format!("`{text}` is not a valid f64")))?,
            )
        } else {
            Tok::Int(
                text.parse()
                    .map_err(|_| self.error(span, format!("`{text}` does not fit in an i64")))?,
            )
        };
        self.push(tok, span);
        Ok(())
    }

    /// An identifier or a keyword.
    ///
    /// `identifier ::= [a-zA-Z_][a-zA-Z0-9_]* (":" [a-zA-Z][a-zA-Z0-9_]*)*`, so
    /// a namespace segment is taken only when a letter follows the colon
    /// immediately. That is what keeps `x :: T` and `x: expr` out of it, and it
    /// is why a column and its value want the space: `x:x` is one qualified
    /// name, exactly as the grammar says.
    fn word(&mut self) {
        let start = self.pos;
        let start_col = self.column();
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.pos += 1;
        }
        while self.peek() == Some(b':') && self.peek_at(1).is_some_and(|c| c.is_ascii_alphabetic())
        {
            self.pos += 1;
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).expect("ascii");
        let span = self.span_from(start, start_col);
        let tok = match text {
            "not" => Tok::Not,
            "and" => Tok::And,
            "or" => Tok::Or,
            "input" => Tok::Input,
            "true" => Tok::True,
            "false" => Tok::False,
            "NONE" => Tok::None,
            "_" => Tok::Underscore,
            _ => Tok::Ident(text.to_string()),
        };
        self.push(tok, span);
    }

    fn operator(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let start_col = self.column();
        let c = self.peek().expect("called with a byte available");
        let next = self.peek_at(1);
        // Two-byte operators first, so `++` never lexes as two `+`.
        let (tok, len) = match (c, next) {
            (b'+', Some(b'+')) => (Tok::PlusPlus, 2),
            (b':', Some(b'=')) => (Tok::Assign, 2),
            (b':', Some(b':')) => (Tok::Annot, 2),
            (b'<', Some(b'-')) => (Tok::Arrow, 2),
            (b'<', Some(b'=')) => (Tok::Le, 2),
            (b'>', Some(b'=')) => (Tok::Ge, 2),
            (b'!', Some(b'=')) => (Tok::Ne, 2),
            (b'=', Some(b'>')) => (Tok::FatArrow, 2),
            (b'*', Some(b'*')) => (Tok::StarStar, 2),
            (b'(', _) => (Tok::LParen, 1),
            (b')', _) => (Tok::RParen, 1),
            (b'[', _) => (Tok::LBracket, 1),
            (b']', _) => (Tok::RBracket, 1),
            (b'{', _) => (Tok::LBrace, 1),
            (b'}', _) => (Tok::RBrace, 1),
            (b',', _) => (Tok::Comma, 1),
            (b':', _) => (Tok::Colon, 1),
            (b'.', _) => (Tok::Dot, 1),
            (b'+', _) => (Tok::Plus, 1),
            (b'-', _) => (Tok::Minus, 1),
            (b'*', _) => (Tok::Star, 1),
            (b'/', _) => (Tok::Slash, 1),
            (b'%', _) => (Tok::Percent, 1),
            (b'=', _) => (Tok::Eq, 1),
            (b'<', _) => (Tok::Lt, 1),
            (b'>', _) => (Tok::Gt, 1),
            _ => {
                let len = utf8_len(c);
                let shown = String::from_utf8_lossy(&self.src[start..start + len]).into_owned();
                return Err(self.error(
                    Span::new(self.line, start_col, len),
                    format!("unexpected character `{shown}`"),
                ));
            }
        };
        self.pos += len;
        let span = self.span_from(start, start_col);
        self.push(tok, span);
        Ok(())
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
