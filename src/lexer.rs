use crate::diagnostic::Diagnostic;
use crate::span::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Int(i128),
    Float(f64),
    Str(String),
    Fn,
    /// `pure` function modifier: keyword that precedes `fn`.
    /// Marks the function as side-effect-free.
    Pure,
    /// `parallel` loop modifier: keyword that precedes `for`.
    /// Marks the iteration as independently parallelizable
    /// (verified by the effects checker).
    Parallel,
    /// `reduce <var> with <op>;` clause on a `parallel for`. The
    /// body must update `<var>` only via the named op; each thread
    /// accumulates a partial value and the runtime combines them.
    Reduce,
    /// Part of the `reduce <var> with <op>;` clause syntax.
    With,
    /// `min` reduction op + builtin function `min(a, b)`.
    Min,
    /// `max` reduction op + builtin function `max(a, b)`.
    Max,
    /// `task <name> { ... }` — declares an affine handle of type
    /// `Task` and a side-effect-free body that runs once. v1
    /// lowers sequentially; the verifier is the value-add.
    Task,
    /// `join <name>;` — consumes a `Task` handle. v1 lowers to a
    /// no-op once the spawn's body has executed.
    Join,
    Let,
    Return,
    If,
    Else,
    While,
    Break,
    Continue,
    Mut,
    For,
    In,
    /// `ref x` — prefix borrow operator. Replaces the older
    /// `&x` shape; the same keyword is used in type position
    /// (`ref T`) and at call-site / for-iter borrows. Refines
    /// T0.0 of the consolidated TODO.
    Ref,
    /// `struct Name { f1: T1, … }` — top-level record-type
    /// declaration. T1.2.
    Struct,
    /// `enum Name { Variant1, Variant2, … }` — top-level
    /// tagged-union declaration. T1.3.
    Enum,
    /// `match expr { Pat then expr, … }` — pattern-match
    /// expression. T1.3.
    Match,
    /// `Pattern then body` — match-arm separator. T1.3.
    Then,
    /// `interface Name { fn …; }` — abstract behavior
    /// declaration. T1.5.
    Interface,
    /// `implement Iface for Type { … }` — bind interface
    /// methods to a concrete type. T1.5.
    Implement,
    /// `where T is Iface` — generic bound clause. T1.5.
    Where,
    /// `T is Iface` — bound predicate keyword. T1.5.
    Is,
    /// `const NAME: T = expr;` — top-level compile-time
    /// constant. v1 restricts the initializer to a literal
    /// expression and the type to Copy. T4.15.
    Const,
    /// `type Name = Type;` — top-level type alias. v1
    /// rejects recursive aliases. T4.15 (type-alias half).
    Type,
    /// `methods on TypeName { fn foo(self: …) -> … { … } }`
    /// — group of methods attached to a concrete type.
    /// Method bodies lower to free functions with names
    /// mangled as `<TypeName>_<methodName>`, so callers can
    /// write `p.foo(args)` and have the checker rewrite the
    /// MethodCall into the mangled call. T1.2 phase 2a.
    Methods,
    /// `from EXPR` — opening of the range form
    /// `from <start> to <end>` used by `for` / `parallel for`.
    /// Replaces `<start>..<end>`. T0.0.
    From,
    /// `to EXPR` — closing of the range form (and future slice
    /// shape `xs[lo to hi]`). T0.0.
    To,
    DotDot,
    /// `.` — field access (`p.x`) and tuple-index (`t.0`)
    /// postfix operator. Distinct from `DotDot`. T1.1 / T1.2.
    Dot,
    Intent,
    Use,
    Requires,
    Ensures,
    Invariant,
    Assert,
    Prove,
    Print,
    Len,
    As,
    True,
    False,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
    Vec,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    ColonColon,
    Semicolon,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Equal,
    EqEq,
    BangEq,
    Less,
    LessEq,
    LessLess,
    Greater,
    GreaterEq,
    GreaterGreater,
    Amp,
    AndAnd,
    Pipe,
    OrOr,
    Caret,
    Arrow,
    Eof,
}

pub fn lex(source: &str) -> Result<Vec<Token>, Diagnostic> {
    Lexer::new(source).lex()
}

/// A `// …` comment recovered from source for later use by tools
/// (currently the formatter). The lexer's main pass drops comments
/// to keep the token stream lean for parsing; this side-channel scan
/// recovers them with their byte spans so a downstream formatter can
/// re-interleave them at the right indent.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    /// The full text of the line including the leading `//`. Trailing
    /// whitespace before the newline is preserved verbatim so that a
    /// careful tool could reproduce the original exactly; the
    /// formatter trims it.
    pub text: String,
    pub span: Span,
}

/// Scan `source` for `//` line comments, returning them in document
/// order. String literals are skipped correctly so `"//"` inside a
/// string is not mistaken for a comment. This is a deliberately
/// separate pass from `lex`: keeping comments off the main token
/// stream avoids polluting every parser site with comment-skipping
/// logic.
pub fn extract_comments(source: &str) -> Vec<Comment> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip a string literal. Honors `\X` two-byte escapes
                // so that `"\""` isn't terminated by the inner quote.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        // The real lexer will surface this. Bail out
                        // so we don't claim everything after as
                        // string content.
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                let text = std::str::from_utf8(&bytes[start..i])
                    .unwrap_or("")
                    .to_string();
                out.push(Comment {
                    text,
                    span: Span::new(start, i),
                });
            }
            _ => i += 1,
        }
    }
    out
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
        }
    }

    fn lex(mut self) -> Result<Vec<Token>, Diagnostic> {
        while !self.is_at_end() {
            let start = self.pos;
            let byte = self.advance();

            match byte {
                b' ' | b'\r' | b'\t' | b'\n' => {}
                b'/' if self.match_byte(b'/') => self.skip_line_comment(),
                b'0'..=b'9' => self.lex_number(start)?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident(start),
                b'"' => self.lex_string(start)?,
                b'(' => self.push(TokenKind::LParen, start),
                b')' => self.push(TokenKind::RParen, start),
                b'{' => self.push(TokenKind::LBrace, start),
                b'}' => self.push(TokenKind::RBrace, start),
                b'[' => self.push(TokenKind::LBracket, start),
                b']' => self.push(TokenKind::RBracket, start),
                b':' => self.push(TokenKind::Colon, start),
                b';' => self.push(TokenKind::Semicolon, start),
                b',' => self.push(TokenKind::Comma, start),
                b'+' => self.push(TokenKind::Plus, start),
                b'-' if self.match_byte(b'>') => self.push(TokenKind::Arrow, start),
                b'-' => self.push(TokenKind::Minus, start),
                b'*' => self.push(TokenKind::Star, start),
                b'/' => self.push(TokenKind::Slash, start),
                b'%' => self.push(TokenKind::Percent, start),
                b'!' if self.match_byte(b'=') => self.push(TokenKind::BangEq, start),
                b'!' => self.push(TokenKind::Bang, start),
                b'=' if self.match_byte(b'=') => self.push(TokenKind::EqEq, start),
                b'=' => self.push(TokenKind::Equal, start),
                b'<' if self.match_byte(b'<') => self.push(TokenKind::LessLess, start),
                b'<' if self.match_byte(b'=') => self.push(TokenKind::LessEq, start),
                b'<' => self.push(TokenKind::Less, start),
                b'>' if self.match_byte(b'>') => self.push(TokenKind::GreaterGreater, start),
                b'>' if self.match_byte(b'=') => self.push(TokenKind::GreaterEq, start),
                b'>' => self.push(TokenKind::Greater, start),
                b'&' if self.match_byte(b'&') => self.push(TokenKind::AndAnd, start),
                b'&' => self.push(TokenKind::Amp, start),
                b'|' if self.match_byte(b'|') => self.push(TokenKind::OrOr, start),
                b'|' => self.push(TokenKind::Pipe, start),
                b'^' => self.push(TokenKind::Caret, start),
                b'.' if self.match_byte(b'.') => self.push(TokenKind::DotDot, start),
                b'.' => self.push(TokenKind::Dot, start),
                other => {
                    return Err(Diagnostic::new(
                        Span::new(start, start + 1),
                        format!("unexpected character '{}'", other as char),
                    ));
                }
            }
        }

        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(self.source.len(), self.source.len()),
        });
        Ok(self.tokens)
    }

    fn lex_number(&mut self, start: usize) -> Result<(), Diagnostic> {
        let first = self.bytes[start];
        if first == b'0' && matches!(self.peek(), Some(b'x' | b'X' | b'b' | b'B' | b'o' | b'O')) {
            return self.lex_radix_int(start);
        }

        while matches!(self.peek(), Some(b'0'..=b'9' | b'_')) {
            self.advance();
        }

        let mut is_float = false;

        if self.peek() == Some(b'.') && matches!(self.peek_next(), Some(b'0'..=b'9')) {
            is_float = true;
            self.advance();
            while matches!(self.peek(), Some(b'0'..=b'9' | b'_')) {
                self.advance();
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            let exponent_start = self.pos;
            self.advance();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.advance();
            }
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                is_float = true;
                while matches!(self.peek(), Some(b'0'..=b'9' | b'_')) {
                    self.advance();
                }
            } else {
                return Err(Diagnostic::new(
                    Span::new(exponent_start, self.pos),
                    "expected digits after float exponent",
                ));
            }
        }

        let span = Span::new(start, self.pos);
        let raw = &self.source[start..self.pos];
        let cleaned = strip_underscores(raw);

        if is_float {
            let value = cleaned.parse::<f64>().map_err(|_| {
                Diagnostic::new(span, format!("float literal '{}' cannot be parsed", raw))
            })?;
            if !value.is_finite() {
                return Err(Diagnostic::new(
                    span,
                    format!("float literal '{}' is not finite", raw),
                ));
            }
            self.tokens.push(Token {
                kind: TokenKind::Float(value),
                span,
            });
            return Ok(());
        }

        let value = cleaned.parse::<i128>().map_err(|_| {
            Diagnostic::new(
                span,
                format!("integer literal '{}' does not fit in i128", raw),
            )
        })?;

        self.tokens.push(Token {
            kind: TokenKind::Int(value),
            span,
        });
        Ok(())
    }

    fn lex_radix_int(&mut self, start: usize) -> Result<(), Diagnostic> {
        let prefix = self.advance();
        let (radix, name): (u32, &str) = match prefix {
            b'x' | b'X' => (16, "hex"),
            b'b' | b'B' => (2, "binary"),
            b'o' | b'O' => (8, "octal"),
            _ => unreachable!("called only on valid radix prefixes"),
        };

        let digits_start = self.pos;
        while let Some(byte) = self.peek() {
            if byte == b'_' || is_digit_for_radix(byte, radix) {
                self.advance();
            } else {
                break;
            }
        }

        if self.pos == digits_start {
            return Err(Diagnostic::new(
                Span::new(start, self.pos),
                format!("expected {} digits after '0{}' prefix", name, prefix as char),
            ));
        }

        let span = Span::new(start, self.pos);
        let cleaned = strip_underscores(&self.source[digits_start..self.pos]);
        let value = i128::from_str_radix(&cleaned, radix).map_err(|_| {
            Diagnostic::new(
                span,
                format!(
                    "{} integer literal '{}' does not fit in i128",
                    name,
                    &self.source[start..self.pos]
                ),
            )
        })?;

        self.tokens.push(Token {
            kind: TokenKind::Int(value),
            span,
        });
        Ok(())
    }

    fn lex_ident(&mut self, start: usize) {
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.advance();
        }

        let text = &self.source[start..self.pos];
        let kind = match text {
            "fn" => TokenKind::Fn,
            "pure" => TokenKind::Pure,
            "parallel" => TokenKind::Parallel,
            "reduce" => TokenKind::Reduce,
            "with" => TokenKind::With,
            "task" => TokenKind::Task,
            "join" => TokenKind::Join,
            // Note: `min` / `max` are NOT global reserved
            // keywords — they're context-sensitive
            // identifiers used by `reduce X with min;`
            // and the `min(a,b)` / `max(a,b)` intrinsics.
            // Users can declare struct fields, locals,
            // and other names called `min`/`max` without
            // collision.
            "let" => TokenKind::Let,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "mut" => TokenKind::Mut,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "ref" => TokenKind::Ref,
            "from" => TokenKind::From,
            "to" => TokenKind::To,
            "struct" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "match" => TokenKind::Match,
            "then" => TokenKind::Then,
            "interface" => TokenKind::Interface,
            "implement" => TokenKind::Implement,
            "where" => TokenKind::Where,
            "is" => TokenKind::Is,
            "const" => TokenKind::Const,
            "type" => TokenKind::Type,
            "methods" => TokenKind::Methods,
            "intent" => TokenKind::Intent,
            "use" => TokenKind::Use,
            "requires" => TokenKind::Requires,
            "ensures" => TokenKind::Ensures,
            "invariant" => TokenKind::Invariant,
            "assert" => TokenKind::Assert,
            "prove" => TokenKind::Prove,
            "print" => TokenKind::Print,
            "len" => TokenKind::Len,
            "as" => TokenKind::As,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "i8" => TokenKind::I8,
            "i16" => TokenKind::I16,
            "i32" => TokenKind::I32,
            "i64" => TokenKind::I64,
            "u8" => TokenKind::U8,
            "u16" => TokenKind::U16,
            "u32" => TokenKind::U32,
            "u64" => TokenKind::U64,
            "f32" => TokenKind::F32,
            "f64" => TokenKind::F64,
            "bool" => TokenKind::Bool,
            "Vec" => TokenKind::Vec,
            _ => TokenKind::Ident(text.to_owned()),
        };

        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn lex_string(&mut self, start: usize) -> Result<(), Diagnostic> {
        let mut value = String::new();

        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.advance();
                    self.tokens.push(Token {
                        kind: TokenKind::Str(value),
                        span: Span::new(start, self.pos),
                    });
                    return Ok(());
                }
                b'\\' => {
                    self.advance();
                    let Some(escaped) = self.peek() else {
                        break;
                    };
                    self.advance();
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'n' => value.push('\n'),
                        b't' => value.push('\t'),
                        b'r' => value.push('\r'),
                        b'0' => value.push('\0'),
                        other => {
                            return Err(Diagnostic::new(
                                Span::new(self.pos.saturating_sub(2), self.pos),
                                format!("unknown escape sequence '\\{}'", other as char),
                            ));
                        }
                    }
                }
                b'\n' => {
                    return Err(Diagnostic::new(
                        Span::new(start, self.pos),
                        "string literal cannot span lines",
                    ));
                }
                _ => {
                    let char_start = self.pos;
                    let ch = self
                        .next_char()
                        .ok_or_else(|| Diagnostic::new(
                            Span::new(char_start, self.pos),
                            "invalid character in string literal",
                        ))?;
                    value.push(ch);
                }
            }
        }

        Err(Diagnostic::new(
            Span::new(start, self.pos),
            "unterminated string literal",
        ))
    }

    fn skip_line_comment(&mut self) {
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.advance();
        }
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> u8 {
        let byte = self.bytes[self.pos];
        self.pos += 1;
        byte
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.source[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn match_byte(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }
}

fn strip_underscores(text: &str) -> String {
    text.chars().filter(|ch| *ch != '_').collect()
}

fn is_digit_for_radix(byte: u8, radix: u32) -> bool {
    match radix {
        2 => matches!(byte, b'0' | b'1'),
        8 => matches!(byte, b'0'..=b'7'),
        16 => byte.is_ascii_hexdigit(),
        10 => byte.is_ascii_digit(),
        _ => false,
    }
}
