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
    pub domain: Option<Box<str>>,
    pub code: Option<Box<str>>,
    diagnostic_context: Option<Box<DiagnosticContext>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticContext {
    file: String,
    source: String,
}

impl KuError {
    pub fn new(kind: KuErrorKind, message: impl Into<String>, span: Span) -> Self {
        Self {
            kind,
            message: message.into(),
            span,
            domain: None,
            code: None,
            diagnostic_context: None,
        }
    }

    pub fn structured(
        kind: KuErrorKind,
        domain: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        span: Span,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            span,
            domain: Some(domain.into().into_boxed_str()),
            code: Some(code.into().into_boxed_str()),
            diagnostic_context: None,
        }
    }

    pub fn package(code: impl Into<String>, message: impl Into<String>, span: Span) -> Self {
        Self::structured(KuErrorKind::Runtime, "package", code, message, span)
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

    pub fn with_diagnostic_context(
        mut self,
        file: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        if self.diagnostic_context.is_none() {
            self.diagnostic_context = Some(Box::new(DiagnosticContext {
                file: file.into(),
                source: source.into(),
            }));
        }
        self
    }

    pub fn diagnostic(&self, file: &str, source: &str) -> String {
        let (file, source) = self
            .diagnostic_context
            .as_ref()
            .map(|context| (context.file.as_str(), context.source.as_str()))
            .unwrap_or((file, source));
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
