use std::fmt;

use crate::span::Span;

pub type KuResult<T> = Result<T, KuError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KuErrorKind {
    Lex,
    Parse,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KuError {
    pub kind: KuErrorKind,
    pub message: String,
    pub span: Span,
}

impl KuError {
    pub fn new(kind: KuErrorKind, message: impl Into<String>, span: Span) -> Self {
        Self {
            kind,
            message: message.into(),
            span,
        }
    }

    pub fn lex(message: impl Into<String>, span: Span) -> Self {
        Self::new(KuErrorKind::Lex, message, span)
    }

    pub fn parse(message: impl Into<String>, span: Span) -> Self {
        Self::new(KuErrorKind::Parse, message, span)
    }

    pub fn runtime(message: impl Into<String>, span: Span) -> Self {
        Self::new(KuErrorKind::Runtime, message, span)
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::new(KuErrorKind::Runtime, message, Span::default())
    }

    pub fn at(span: Span, message: impl Into<String>) -> Self {
        Self::new(KuErrorKind::Runtime, message, span)
    }

    pub fn line(&self) -> usize {
        self.span.start.line
    }

    pub fn column(&self) -> usize {
        self.span.start.column
    }

    pub fn diagnostic(&self, file: &str, source: &str) -> String {
        let line = self.line().max(1);
        let column = self.column().max(1);
        let line_text = source.lines().nth(line.saturating_sub(1)).unwrap_or("");
        let caret_len = self
            .span
            .end
            .column
            .saturating_sub(self.span.start.column)
            .max(1);
        let marker = format!(
            "{}{}",
            " ".repeat(column.saturating_sub(1)),
            "^".repeat(caret_len)
        );

        let heading = if self.message.starts_with("type error:") {
            self.message.clone()
        } else {
            format!("error: {}", self.message)
        };

        format!(
            "{heading}\n  --> {file}:{line}:{column}\n   |\n{line:>3} | {line_text}\n   | {marker}"
        )
    }
}

impl fmt::Display for KuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.span == Span::default() {
            return write!(f, "{}", self.message);
        }
        write!(
            f,
            "{:?} error at {}:{}: {}",
            self.kind,
            self.line(),
            self.column(),
            self.message
        )
    }
}

impl std::error::Error for KuError {}
