use std::{borrow::Cow, fmt};

use crate::span::Span;

pub type KuResult<T> = Result<T, KuError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KuErrorKind {
    Lex,
    Parse,
    Runtime,
}

#[derive(Clone, PartialEq, Eq)]
pub struct KuError {
    pub kind: KuErrorKind,
    pub message: String,
    pub span: Span,
    pub domain: Option<Box<str>>,
    pub code: Option<Box<str>>,
    diagnostic_context: Option<Box<DiagnosticContext>>,
}

#[derive(Clone, PartialEq, Eq)]
struct DiagnosticContext {
    file: String,
    source: String,
}

struct DiagnosticContextSummary<'a> {
    file: &'a str,
    source_bytes: usize,
}

impl fmt::Debug for DiagnosticContextSummary<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticContext")
            .field("file", &self.file)
            .field("source_bytes", &self.source_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticData {
    pub level: &'static str,
    pub code: &'static str,
    pub message: String,
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub notes: Vec<&'static str>,
    pub helps: Vec<&'static str>,
}

const MAX_DIAGNOSTIC_MARKER_INDENT: usize = 256;
const MAX_DIAGNOSTIC_MARKER_CARETS: usize = 256;
const MAX_DIAGNOSTIC_LINE_CHARS: usize =
    MAX_DIAGNOSTIC_MARKER_INDENT + MAX_DIAGNOSTIC_MARKER_CARETS;
const DIAGNOSTIC_LINE_TRUNCATION: &str = "… [line truncated]";
const MAX_DIAGNOSTIC_INLINE_CHARS: usize = 1024;
const DIAGNOSTIC_INLINE_TRUNCATION: &str = "… [text truncated]";

impl fmt::Debug for KuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diagnostic_context =
            self.diagnostic_context
                .as_deref()
                .map(|context| DiagnosticContextSummary {
                    file: &context.file,
                    source_bytes: context.source.len(),
                });
        formatter
            .debug_struct("KuError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("span", &self.span)
            .field("domain", &self.domain)
            .field("code", &self.code)
            .field("diagnostic_context", &diagnostic_context)
            .finish()
    }
}

fn diagnostic_marker(column: usize, caret_len: usize) -> String {
    let indent = column.saturating_sub(1);
    let caret_len = caret_len.max(1);
    if indent > MAX_DIAGNOSTIC_MARKER_INDENT {
        return format!("... [marker omitted: column={column}, width={caret_len}]");
    }

    if caret_len > MAX_DIAGNOSTIC_MARKER_CARETS {
        return format!(
            "{}{}... [span truncated: width={caret_len}]",
            " ".repeat(indent),
            "^".repeat(MAX_DIAGNOSTIC_MARKER_CARETS),
        );
    }

    format!("{}{}", " ".repeat(indent), "^".repeat(caret_len))
}

fn unsafe_diagnostic_char(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn sanitized_diagnostic_preview(text: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    let max_chars = max_chars.min(MAX_DIAGNOSTIC_INLINE_CHARS);
    let mut sanitized = None;
    let mut end = 0;
    let mut truncated = false;
    for (index, (offset, character)) in text.char_indices().enumerate() {
        if index == max_chars {
            truncated = true;
            break;
        }
        end = offset + character.len_utf8();
        let replacement = if character == '\t' {
            Some(' ')
        } else if unsafe_diagnostic_char(character) {
            Some('\u{fffd}')
        } else {
            None
        };
        if let Some(replacement) = replacement {
            let output = sanitized.get_or_insert_with(|| {
                let capacity = max_chars.saturating_mul(4);
                let mut output = String::with_capacity(capacity);
                output.push_str(&text[..offset]);
                output
            });
            output.push(replacement);
        } else if let Some(output) = sanitized.as_mut() {
            output.push(character);
        }
    }

    match sanitized {
        Some(output) => (Cow::Owned(output), truncated),
        None if truncated => (Cow::Borrowed(&text[..end]), true),
        None => (Cow::Borrowed(text), false),
    }
}

fn diagnostic_line_preview(line: &str) -> (Cow<'_, str>, &'static str) {
    let (preview, truncated) = sanitized_diagnostic_preview(line, MAX_DIAGNOSTIC_LINE_CHARS);
    (
        preview,
        if truncated {
            DIAGNOSTIC_LINE_TRUNCATION
        } else {
            ""
        },
    )
}

fn diagnostic_inline_preview(text: &str) -> (Cow<'_, str>, &'static str) {
    let (preview, truncated) = sanitized_diagnostic_preview(text, MAX_DIAGNOSTIC_INLINE_CHARS);
    (
        preview,
        if truncated {
            DIAGNOSTIC_INLINE_TRUNCATION
        } else {
            ""
        },
    )
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
        let (line, column, end_line, end_column) = self.diagnostic_location();
        let info = self.diagnostic_info();
        let line_text = source.lines().nth(line.saturating_sub(1)).unwrap_or("");
        let (line_text, line_suffix) = diagnostic_line_preview(line_text);
        let caret_len = if end_line == line {
            end_column.saturating_sub(column).max(1)
        } else {
            1
        };
        let marker = diagnostic_marker(column, caret_len);
        let (message, message_suffix) = diagnostic_inline_preview(&self.message);
        let (file, file_suffix) = diagnostic_inline_preview(file);

        let mut output = format!(
            "error[{}]: error: {message}{message_suffix}\n  --> {file}{file_suffix}:{line}:{column}\n   |\n{line:>3} | {line_text}{line_suffix}\n   | {marker}",
            info.code
        );
        for note in info.notes {
            output.push_str(&format!("\n   |\nnote: {note}"));
        }
        for help in info.helps {
            output.push_str(&format!("\nhelp: {help}"));
        }
        output
    }

    pub fn diagnostic_data(&self, file: &str, _source: &str) -> DiagnosticData {
        let file = self
            .diagnostic_context
            .as_ref()
            .map(|context| context.file.as_str())
            .unwrap_or(file);
        let (line, column, end_line, end_column) = self.diagnostic_location();
        let info = self.diagnostic_info();
        DiagnosticData {
            level: "error",
            code: info.code,
            message: self.message.clone(),
            file: file.to_string(),
            line,
            column,
            end_line,
            end_column,
            notes: info.notes,
            helps: info.helps,
        }
    }

    fn diagnostic_location(&self) -> (usize, usize, usize, usize) {
        let line = self.line().max(1);
        let column = self.column().max(1);
        let mut end_line = self.span.end.line.max(line);
        let mut end_column = self.span.end.column.max(1);
        if end_line == line {
            end_column = end_column.max(column.saturating_add(1));
        } else if self.span.end.line == 0 {
            end_line = line;
            end_column = column.saturating_add(1);
        }
        (line, column, end_line, end_column)
    }

    fn diagnostic_info(&self) -> DiagnosticInfo {
        let message = self.message.as_str();
        if message.contains("'let' is not supported") {
            return DiagnosticInfo::new("E0105")
                .help("Ku declares variables by assignment, so remove `let`");
        }
        if message.contains("switch is not supported") {
            return DiagnosticInfo::new("E0104").help("replace `switch` with `match`");
        }
        if message.contains("condition must be bool") {
            return DiagnosticInfo::new("E0302")
                .note("Ku does not use truthy/falsy conditions")
                .help("compare explicitly, for example `value != 0` or `text != \"\"`");
        }
        if message.contains("'?' requires a Result return type")
            || message.contains("'?' expects Result")
        {
            return DiagnosticInfo::new("E0401")
                .note("`?` can only unwrap recoverable Result values")
                .help("change the enclosing function to return `T!`, or handle the error with `try/catch`");
        }
        if message.contains("http handler cannot modify captured variable") {
            return DiagnosticInfo::new("E0701")
                .note("HTTP handlers may run concurrently")
                .help("avoid shared mutable captures in handlers until a sync/state API exists");
        }
        if message.contains("unknown std module") {
            return DiagnosticInfo::new("E0601")
                .note("standard library module names are lowercase")
                .help("use lowercase std names, for example `import { task, time } from \"std\"`");
        }
        if message.contains("not exported by")
            || message.contains("has no exported function")
            || message.contains("has no exported type")
        {
            return DiagnosticInfo::new("E0601")
                .note("lowercase top-level names in user files are private to that file")
                .help("rename the exported fn/struct/enum so it starts with an uppercase ASCII letter");
        }
        if message.contains("std module") && message.contains("must be imported") {
            return DiagnosticInfo::new("E0601")
                .note("std modules with side effects or runtime state must be imported explicitly")
                .help("add `import { module } from \"std\"`, or use the module path form such as `import \"std.task\"`");
        }
        if message.contains("has already been awaited") {
            return DiagnosticInfo::new("E0804")
                .note("awaiting a task consumes its result")
                .help("store the awaited value if you need to use it again");
        }
        if message.contains("task values cannot be cloned") {
            return DiagnosticInfo::new("E0803")
                .note("Task<T> is owned and move-only")
                .help("keep the original task handle and await it once");
        }
        if message.contains("task handles can only be awaited") {
            return DiagnosticInfo::new("E0802")
                .note("Ku does not expose task.spawn, Task.new, or user-level task scheduling")
                .help("start work by calling an async fn, then use `result = await task?`");
        }
        if message.contains("cannot move captured owned value") {
            return DiagnosticInfo::new("E0904")
                .note("a closure shares captured variables by reference; it may borrow them but cannot move an owned capture out")
                .help("call `.clone()` inside the closure to return or store an owned copy");
        }
        if message.contains("use of moved value")
            || message.contains("cannot move an owned field or indexed element")
            || message.contains("cannot move outer owned value")
        {
            return DiagnosticInfo::new("E0901")
                .note("Ku is move-by-default; str/array/object/struct/enum/function values are owned")
                .help("call `.clone()` to keep the original, or restructure so the value is used once");
        }
        if message.contains("unused local binding") {
            return DiagnosticInfo::new("E0905")
                .note("strict unused checks are enabled by `ku check --deny-unused`")
                .help("remove the binding, use it, or rename it with a leading `_` when it is intentionally unused");
        }
        if message.contains("unused import") {
            return DiagnosticInfo::new("E0603")
                .note("Ku treats unused imports as errors by default")
                .help("remove the import, use the imported name, or alias it with a leading `_` when it is intentionally unused");
        }
        if message.contains("std module member 'http.service'")
            || message.contains("std module member 'http.server'")
        {
            return DiagnosticInfo::new("E0602")
                .note("stdlib constructors are functions, not property-style default objects")
                .help("write `app = http.service()` or `app = http.server(config)`");
        }
        if message.contains("http handler parameter")
            || message.contains("res/writer parameters are not allowed")
            || message.contains("side-effect response API")
            || message.contains("ordinary HTTP route handler")
            || message.contains("fn(req, res) is not allowed")
        {
            return DiagnosticInfo::new("E0701")
                .note("ordinary HTTP handlers use the Return model")
                .help("write `fn() { return http.text(...) }` when request data is not needed, or `fn(req) { ... }` when it is");
        }
        if message.contains("handler did not return HttpResponse")
            || message.contains("HTTP handler must return HttpResponse")
        {
            return DiagnosticInfo::new("E0702")
                .note("HTTP handlers must return an HttpResponse or HttpResponse!")
                .help("return `http.text/json/html/empty/redirect(...)` from the handler");
        }
        if message.starts_with("type error:") || message.contains("type mismatch") {
            return DiagnosticInfo::new("E0301");
        }
        if message.contains("not exhaustive") || message.contains("unreachable match arm") {
            return DiagnosticInfo::new("E0501");
        }
        if message.contains("std module")
            || message.contains("import")
            || self.domain_name() == Some("package")
        {
            return DiagnosticInfo::new("E0601");
        }
        if message.contains("http ") {
            return DiagnosticInfo::new("E0700");
        }
        match self.kind {
            KuErrorKind::Lex | KuErrorKind::Parse => DiagnosticInfo::new("E0101"),
            KuErrorKind::Runtime => DiagnosticInfo::new("E0001"),
        }
    }

    fn domain_name(&self) -> Option<&str> {
        self.domain.as_deref()
    }
}

struct DiagnosticInfo {
    code: &'static str,
    notes: Vec<&'static str>,
    helps: Vec<&'static str>,
}

impl DiagnosticInfo {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            notes: Vec::new(),
            helps: Vec::new(),
        }
    }

    fn note(mut self, note: &'static str) -> Self {
        self.notes.push(note);
        self
    }

    fn help(mut self, help: &'static str) -> Self {
        self.helps.push(help);
        self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Position;

    #[test]
    fn debug_redacts_diagnostic_source_text() {
        let secret_source = "token = \"do-not-log-this\"";
        let error = KuError::parse("expected expression", Span::default())
            .with_diagnostic_context("secret.ku", secret_source);

        let debug = format!("{error:?}");
        assert!(debug.contains("secret.ku"));
        assert!(debug.contains(&format!("source_bytes: {}", secret_source.len())));
        assert!(!debug.contains(secret_source));
        assert!(!debug.contains("do-not-log-this"));
    }

    #[test]
    fn ordinary_diagnostic_rendering_is_unchanged() {
        let error = KuError::parse(
            "expected expression",
            Span::new(Position::new(2, 5, 16), Position::new(2, 10, 21)),
        );

        assert_eq!(
            error.diagnostic("sample.ku", "fn main() {\n    value = 1\n}\n"),
            "error[E0101]: error: expected expression\n  --> sample.ku:2:5\n   |\n  2 |     value = 1\n   |     ^^^^^"
        );
    }

    #[test]
    fn extreme_diagnostic_marker_is_bounded_and_explicit() {
        let extreme_column = KuError::parse(
            "bad column",
            Span::new(
                Position::new(1, usize::MAX, 0),
                Position::new(1, usize::MAX, 0),
            ),
        )
        .diagnostic("sample.ku", "x");
        assert!(extreme_column.len() < 1024);
        assert!(extreme_column.contains(&format!(
            "... [marker omitted: column={}, width=1]",
            usize::MAX
        )));

        let extreme_width = KuError::parse(
            "bad width",
            Span::new(Position::new(1, 1, 0), Position::new(1, usize::MAX, 1)),
        )
        .diagnostic("sample.ku", "x");
        assert!(extreme_width.len() < 1024);
        assert!(extreme_width.contains(&format!(
            "{}... [span truncated: width={}]",
            "^".repeat(MAX_DIAGNOSTIC_MARKER_CARETS),
            usize::MAX - 1
        )));
    }

    #[test]
    fn diagnostic_marker_limits_have_no_off_by_one_gap() {
        let visible = diagnostic_marker(
            MAX_DIAGNOSTIC_MARKER_INDENT + 1,
            MAX_DIAGNOSTIC_MARKER_CARETS,
        );
        assert_eq!(
            visible,
            format!(
                "{}{}",
                " ".repeat(MAX_DIAGNOSTIC_MARKER_INDENT),
                "^".repeat(MAX_DIAGNOSTIC_MARKER_CARETS)
            )
        );

        let omitted = diagnostic_marker(MAX_DIAGNOSTIC_MARKER_INDENT + 2, 1);
        assert_eq!(
            omitted,
            format!(
                "... [marker omitted: column={}, width=1]",
                MAX_DIAGNOSTIC_MARKER_INDENT + 2
            )
        );
    }

    #[test]
    fn diagnostic_line_preview_is_unicode_safe_and_bounded() {
        let exact = "界".repeat(MAX_DIAGNOSTIC_LINE_CHARS);
        let (preview, suffix) = diagnostic_line_preview(&exact);
        assert_eq!(preview.as_ref(), exact);
        assert_eq!(suffix, "");

        let long = format!("{}🦀", exact);
        let error = KuError::parse(
            "long line",
            Span::new(Position::new(1, 1, 0), Position::new(1, 2, 1)),
        );
        let diagnostic = error.diagnostic("sample.ku", &long);
        assert!(diagnostic.contains(&format!("{exact}{DIAGNOSTIC_LINE_TRUNCATION}")));
        assert!(!diagnostic.contains('🦀'));
        assert!(diagnostic.len() < exact.len() + 256);
    }

    #[test]
    fn diagnostic_line_preview_neutralizes_terminal_controls() {
        let source = "\u{1b}\t";
        let error = crate::lexer::Lexer::new(source)
            .lex()
            .expect_err("ESC must be rejected by the lexer");
        let diagnostic = error.diagnostic("bad\u{7}\u{2028}\u{202e}.ku", source);
        for unsafe_character in ['\t', '\u{1b}', '\u{7}', '\u{2028}', '\u{202e}'] {
            assert!(
                !diagnostic.contains(unsafe_character),
                "diagnostic retained unsafe {unsafe_character:?}: {diagnostic:?}"
            );
        }
        assert_eq!(diagnostic.matches('\u{fffd}').count(), 5);
    }

    #[test]
    fn diagnostic_inline_text_has_a_unicode_safe_hard_limit() {
        let exact = "界".repeat(MAX_DIAGNOSTIC_INLINE_CHARS);
        let message = format!("{exact}🦀");
        let file = format!("{exact}🦀");
        let error = KuError::parse(
            message,
            Span::new(Position::new(1, 1, 0), Position::new(1, 2, 1)),
        );
        let diagnostic = error.diagnostic(&file, "x");
        assert_eq!(
            diagnostic
                .matches(&format!("{exact}{DIAGNOSTIC_INLINE_TRUNCATION}"))
                .count(),
            2
        );
        assert!(!diagnostic.contains('🦀'));
        assert!(diagnostic.len() < exact.len() * 2 + 512);
    }

    #[test]
    fn debug_without_context_keeps_the_stable_field_shape() {
        let error = KuError::parse("expected expression", Span::default());
        assert_eq!(
            format!("{error:?}"),
            "KuError { kind: Parse, message: \"expected expression\", span: Span { start: Position { line: 0, column: 0, offset: 0 }, end: Position { line: 0, column: 0, offset: 0 } }, domain: None, code: None, diagnostic_context: None }"
        );
    }
}
