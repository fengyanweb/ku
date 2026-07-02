use crate::error::{KuError, KuResult};
use crate::span::{Position, Span};
use crate::token::{Token, TokenKind};

pub struct Lexer<'a> {
    chars: Vec<char>,
    current: usize,
    line: usize,
    column: usize,
    offset: usize,
    _file: &'a str,
    _source: &'a str,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self::with_file("<source>", source)
    }

    pub fn with_file(file: &'a str, source: &'a str) -> Self {
        Self {
            chars: source.chars().collect(),
            current: 0,
            line: 1,
            column: 1,
            offset: 0,
            _file: file,
            _source: source,
        }
    }

    pub fn from_source(source: &'a str) -> Self {
        Self::new(source)
    }

    pub fn lex(mut self) -> KuResult<Vec<Token>> {
        let mut tokens = Vec::new();

        while !self.is_at_end() {
            self.skip_whitespace_and_comments()?;
            if self.is_at_end() {
                break;
            }

            let start = self.position();
            let ch = self.advance().unwrap();
            let kind = match ch {
                '(' => TokenKind::LParen,
                ')' => TokenKind::RParen,
                '{' => TokenKind::LBrace,
                '}' => TokenKind::RBrace,
                '[' => TokenKind::LBracket,
                ']' => TokenKind::RBracket,
                ',' => TokenKind::Comma,
                ':' => TokenKind::Colon,
                ';' => TokenKind::Semicolon,
                '.' => {
                    if self.match_char('.') {
                        if self.match_char('.') {
                            TokenKind::Ellipsis
                        } else {
                            return Err(KuError::lex(
                                "expected third '.' for '...'",
                                Span::point(start),
                            ));
                        }
                    } else {
                        TokenKind::Dot
                    }
                }
                '+' => {
                    if self.match_char('+') {
                        TokenKind::PlusPlus
                    } else if self.match_char('=') {
                        TokenKind::PlusEqual
                    } else {
                        TokenKind::Plus
                    }
                }
                '-' => {
                    if self.match_char('-') {
                        TokenKind::MinusMinus
                    } else if self.match_char('=') {
                        TokenKind::MinusEqual
                    } else {
                        TokenKind::Minus
                    }
                }
                '*' => {
                    if self.match_char('=') {
                        TokenKind::StarEqual
                    } else {
                        TokenKind::Star
                    }
                }
                '%' => {
                    if self.match_char('=') {
                        TokenKind::PercentEqual
                    } else {
                        TokenKind::Percent
                    }
                }
                '?' => {
                    if self.match_char('.') {
                        TokenKind::QuestionDot
                    } else {
                        TokenKind::Question
                    }
                }
                '!' => {
                    if self.match_char('=') {
                        TokenKind::BangEqual
                    } else {
                        TokenKind::Bang
                    }
                }
                '=' => {
                    if self.match_char('=') {
                        TokenKind::EqualEqual
                    } else if self.match_char('>') {
                        TokenKind::Arrow
                    } else {
                        TokenKind::Equal
                    }
                }
                '<' => {
                    if self.match_char('=') {
                        TokenKind::LessEqual
                    } else {
                        TokenKind::Less
                    }
                }
                '>' => {
                    if self.match_char('=') {
                        TokenKind::GreaterEqual
                    } else {
                        TokenKind::Greater
                    }
                }
                '/' => {
                    if self.match_char('=') {
                        TokenKind::SlashEqual
                    } else {
                        TokenKind::Slash
                    }
                }
                '&' => {
                    if self.match_char('&') {
                        TokenKind::AndAnd
                    } else {
                        return Err(KuError::lex("expected '&' after '&'", Span::point(start)));
                    }
                }
                '|' => {
                    if self.match_char('|') {
                        TokenKind::OrOr
                    } else {
                        TokenKind::Pipe
                    }
                }
                '"' => {
                    let value = self.string(start)?;
                    let span = Span::new(start, self.position());
                    tokens.push(Token::new(TokenKind::String(value), span));
                    continue;
                }
                '\'' => {
                    let value = self.single_string(start)?;
                    let span = Span::new(start, self.position());
                    tokens.push(Token::new(TokenKind::String(value), span));
                    continue;
                }
                '`' => {
                    let value = self.template_string(start)?;
                    let span = Span::new(start, self.position());
                    tokens.push(Token::new(TokenKind::TemplateString(value), span));
                    continue;
                }
                c if c.is_ascii_digit() => {
                    let kind = self.number(c, start)?;
                    let span = Span::new(start, self.position());
                    tokens.push(Token::new(kind, span));
                    continue;
                }
                c if is_ident_start(c) => {
                    let kind = self.identifier(c);
                    let span = Span::new(start, self.position());
                    tokens.push(Token::new(kind, span));
                    continue;
                }
                _ => {
                    return Err(KuError::lex(
                        format!("unexpected character '{}'", ch),
                        Span::point(start),
                    ));
                }
            };

            tokens.push(Token::new(kind, Span::new(start, self.position())));
        }

        let eof = self.position();
        tokens.push(Token::new(TokenKind::Eof, Span::point(eof)));
        Ok(tokens)
    }

    pub fn tokenize(self) -> KuResult<Vec<Token>> {
        self.lex()
    }

    fn skip_whitespace_and_comments(&mut self) -> KuResult<()> {
        loop {
            match self.peek() {
                Some(' ' | '\r' | '\t' | '\n' | '\u{feff}') => {
                    self.advance();
                }
                Some('/') if self.peek_next() == Some('/') => {
                    while self.peek().is_some() && self.peek() != Some('\n') {
                        self.advance();
                    }
                }
                Some('/') if self.peek_next() == Some('*') => {
                    let start = self.position();
                    self.advance();
                    self.advance();
                    while !(self.peek() == Some('*') && self.peek_next() == Some('/')) {
                        if self.is_at_end() {
                            return Err(KuError::lex(
                                "unterminated block comment",
                                Span::point(start),
                            ));
                        }
                        self.advance();
                    }
                    self.advance();
                    self.advance();
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn string(&mut self, start: Position) -> KuResult<String> {
        let mut value = String::new();
        while let Some(ch) = self.peek() {
            if ch == '"' {
                self.advance();
                return Ok(value);
            }

            if ch == '\\' {
                self.advance();
                let escaped = self.advance().ok_or_else(|| {
                    KuError::lex("unterminated string escape", Span::point(start))
                })?;
                match escaped {
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    other => {
                        return Err(KuError::lex(
                            format!("unknown string escape '\\{}'", other),
                            Span::point(self.position()),
                        ));
                    }
                }
            } else {
                value.push(ch);
                self.advance();
            }
        }

        Err(KuError::lex("unterminated string", Span::point(start)))
    }

    fn single_string(&mut self, start: Position) -> KuResult<String> {
        let mut value = String::new();
        while let Some(ch) = self.peek() {
            if ch == '\'' {
                self.advance();
                return Ok(value);
            }

            if ch == '\\' {
                self.advance();
                let escaped = self.advance().ok_or_else(|| {
                    KuError::lex("unterminated string escape", Span::point(start))
                })?;
                match escaped {
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    '\'' => value.push('\''),
                    '\\' => value.push('\\'),
                    other => {
                        return Err(KuError::lex(
                            format!("unknown string escape '\\{}'", other),
                            Span::point(self.position()),
                        ));
                    }
                }
            } else {
                value.push(ch);
                self.advance();
            }
        }

        Err(KuError::lex("unterminated string", Span::point(start)))
    }

    fn template_string(&mut self, start: Position) -> KuResult<String> {
        let mut value = String::new();
        while let Some(ch) = self.peek() {
            if ch == '`' {
                self.advance();
                return Ok(value);
            }

            if ch == '\\' {
                self.advance();
                let escaped = self.advance().ok_or_else(|| {
                    KuError::lex("unterminated template string escape", Span::point(start))
                })?;
                match escaped {
                    '`' => value.push('`'),
                    '{' => value.push_str("\\{"),
                    '}' => value.push_str("\\}"),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    '\\' => value.push('\\'),
                    other => {
                        value.push('\\');
                        value.push(other);
                    }
                }
            } else {
                value.push(ch);
                self.advance();
            }
        }

        Err(KuError::lex(
            "unterminated template string",
            Span::point(start),
        ))
    }

    fn number(&mut self, first: char, start: Position) -> KuResult<TokenKind> {
        let mut text = String::new();
        text.push(first);

        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                text.push(ch);
                self.advance();
            } else {
                break;
            }
        }

        if self.peek() == Some('.') && self.peek_next().is_some_and(|c| c.is_ascii_digit()) {
            text.push('.');
            self.advance();
            while let Some(ch) = self.peek() {
                if ch.is_ascii_digit() {
                    text.push(ch);
                    self.advance();
                } else {
                    break;
                }
            }
            let value = text
                .parse::<f64>()
                .map_err(|_| KuError::lex("invalid float literal", Span::point(start)))?;
            Ok(TokenKind::Float(value))
        } else {
            let value = text
                .parse::<i64>()
                .map_err(|_| KuError::lex("invalid int literal", Span::point(start)))?;
            Ok(TokenKind::Int(value))
        }
    }

    fn identifier(&mut self, first: char) -> TokenKind {
        let mut text = String::new();
        text.push(first);

        while let Some(ch) = self.peek() {
            if is_ident_continue(ch) {
                text.push(ch);
                self.advance();
            } else {
                break;
            }
        }

        match text.as_str() {
            "async" => TokenKind::Async,
            "await" => TokenKind::Await,
            "fn" => TokenKind::Fn,
            "struct" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "module" => TokenKind::Module,
            "import" => TokenKind::Import,
            "from" => TokenKind::From,
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "match" => TokenKind::Match,
            "switch" => TokenKind::Switch,
            "try" => TokenKind::Try,
            "catch" => TokenKind::Catch,
            "finally" => TokenKind::Finally,
            "fail" => TokenKind::Fail,
            "panic" => TokenKind::Panic,
            "return" => TokenKind::Return,
            "print" => TokenKind::Print,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "null" => TokenKind::Null,
            _ => TokenKind::Ident(text),
        }
    }

    fn match_char(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.current += 1;
        self.offset += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.current).copied()
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.current + 1).copied()
    }

    fn is_at_end(&self) -> bool {
        self.current >= self.chars.len()
    }

    fn position(&self) -> Position {
        Position::new(self.line, self.column, self.offset)
    }
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}
