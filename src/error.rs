use std::{borrow::Cow, fmt};

use crate::span::Span;

pub type KuResult<T> = Result<T, KuError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KuErrorKind {
    Lex,
    Parse,
    Runtime,
}

/// One registered meaning and repair direction for a compiler diagnostic code.
/// Runtime `KuError.domain` / `KuError.code` are a separate Result/Error contract.
#[derive(Debug, PartialEq, Eq)]
pub struct DiagnosticDefinition {
    pub id: DiagnosticId,
    pub code: &'static str,
    pub summary: &'static str,
    pub notes: &'static [&'static str],
    pub helps: &'static [&'static str],
}

macro_rules! diagnostic_registry {
    ($($id:ident => ($code:literal, $summary:literal, $notes:expr, $helps:expr)),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum DiagnosticId { $($id),* }

        pub const DIAGNOSTIC_REGISTRY: &[DiagnosticDefinition] = &[
            $(DiagnosticDefinition {
                id: DiagnosticId::$id,
                code: $code,
                summary: $summary,
                notes: $notes,
                helps: $helps,
            }),*
        ];

        impl DiagnosticId {
            pub fn definition(self) -> &'static DiagnosticDefinition {
                &DIAGNOSTIC_REGISTRY[self as usize]
            }
        }
    };
}

// Codes, descriptions and repair help are declared only here. Legacy producers
// below classify messages into IDs; new producers can attach an ID directly.
diagnostic_registry! {
    RuntimeError => ("E0001", "Unclassified runtime error", &[], &[]),
    SyntaxError => ("E0101", "Unclassified lexical or syntax error", &[], &[]),
    UnsupportedSwitch => ("E0104", "Unsupported switch syntax", &[],
        &["replace `switch` with `match`"]),
    UnsupportedLet => ("E0105", "Unsupported let declaration", &[],
        &["Ku declares variables by assignment, so remove `let`"]),
    TypeMismatch => ("E0301", "Type mismatch", &[],
        &["make the expression type match the declared or expected type"]),
    NonBooleanCondition => ("E0302", "Condition must be bool",
        &["Ku does not use truthy/falsy conditions"],
        &["compare explicitly, for example `value != 0` or `text != \"\"`"]),
    InvalidResultPropagation => ("E0401", "Invalid Result propagation",
        &["`?` can only unwrap recoverable Result values"],
        &["change the enclosing function to return `T!`, or handle the error with `try/catch`"]),
    NonExhaustiveMatch => ("E0501", "Non-exhaustive match", &[],
        &["add arms for the missing variants, or add an unguarded catch-all arm"]),
    UnreachableMatchArm => ("E0502", "Unreachable match arm", &[],
        &["remove the unreachable arm, or move the catch-all arm after specific patterns"]),
    ImportError => ("E0600", "Unclassified import resolution error", &[],
        &["check the import path, module name and referenced file"]),
    UnknownStdModule => ("E0601", "Unknown standard library module",
        &["standard library module names are lowercase"],
        &["use lowercase std names, for example `import { task, time } from \"std\"`"]),
    ConstructorRequiresCall => ("E0602", "Constructor requires a function call",
        &["stdlib constructors are functions, not property-style default objects"],
        &["write `app = http.service()` or `app = http.server(config)`"]),
    UnusedImport => ("E0603", "Unused import",
        &["Ku treats unused imports as errors by default"],
        &["remove the import, use the imported name, or alias it with a leading `_` when it is intentionally unused"]),
    MemberNotExported => ("E0604", "Module member is not exported",
        &["lowercase top-level names in user files are private to that file"],
        &["rename the exported fn/struct/enum so it starts with an uppercase ASCII letter"]),
    StdImportRequired => ("E0605", "Standard library module requires explicit import",
        &["std modules with side effects or runtime state must be imported explicitly"],
        &["add `import { module } from \"std\"`, or use the module path form such as `import \"std.task\"`"]),
    PackageError => ("E0606", "Unclassified package operation error", &[],
        &["check the package error detail, package configuration and dependency source"]),
    HttpError => ("E0700", "Unclassified HTTP error", &[],
        &["check the HTTP error detail, service configuration and route declaration"]),
    HttpHandlerSignature => ("E0701", "Invalid ordinary HTTP handler signature or response style",
        &["ordinary HTTP handlers use the Return model"],
        &["write `fn() { return http.text(...) }` when request data is not needed, or `fn(req) { ... }` when it is"]),
    HttpHandlerReturn => ("E0702", "HTTP handler must return HttpResponse",
        &["HTTP handlers must return an HttpResponse or HttpResponse!"],
        &["return `http.text/json/html/empty/redirect(...)` from the handler"]),
    HttpCapturedMutation => ("E0703", "HTTP handler modifies a captured variable",
        &["HTTP handlers may run concurrently"],
        &["avoid shared mutable captures in handlers until a sync/state API exists"]),
    InvalidTaskOperation => ("E0802", "Invalid task handle operation",
        &["Ku does not expose task.spawn, Task.new, or user-level task scheduling"],
        &["start work by calling an async fn, then use `result = await task?`"]),
    TaskCannotClone => ("E0803", "Task cannot be cloned",
        &["Task<T> is owned and move-only"],
        &["keep the original task handle and await it once"]),
    TaskAlreadyAwaited => ("E0804", "Task has already been awaited",
        &["awaiting a task consumes its result"],
        &["store the awaited value if you need to use it again"]),
    InvalidOwnedMove => ("E0901", "Invalid move or use of moved owned value",
        &["Ku is move-by-default; str/array/object/struct/enum/function values are owned"],
        &["call `.clone()` to keep the original, or restructure so the value is used once"]),
    CapturedOwnedMove => ("E0904", "Cannot move captured owned value",
        &["a closure shares captured variables by reference; it may borrow them but cannot move an owned capture out"],
        &["call `.clone()` inside the closure to return or store an owned copy"]),
    UnusedLocal => ("E0905", "Unused local binding",
        &["strict unused checks are enabled by `ku check --deny-unused`"],
        &["remove the binding, use it, or rename it with a leading `_` when it is intentionally unused"]),
    BorrowedMutation => ("E0910", "Cannot modify through borrowed parameter",
        &["borrowed parameters are read-only"],
        &["remove '&' if this function should take ownership and modify its parameter"]),
    BorrowedMove => ("E0911", "Cannot move out of borrowed value",
        &["a borrowed parameter does not own the value"],
        &["use '.clone()' to create an owned value before storing or returning it"]),
    BorrowedEscape => ("E0912", "Borrowed value escapes current call", &[],
        &["clone into an owned local before creating the closure"]),
    AsyncBorrowedParameter => ("E0913", "Async function declares borrowed parameter", &[],
        &["let the async function take ownership, or clone the value before calling it"]),
    CallableModeMismatch => ("E0914", "Callable parameter mode mismatch",
        &["owned and borrowed function parameter modes must match exactly"],
        &["use a function whose parameter declarations match the expected '&' modes"]),
    BorrowedOwningArgument => ("E0915", "Borrowed value passed to owning parameter", &[],
        &["use '.clone()' to create an owned argument, or declare the receiving parameter with '&'"]),
    SameCallBorrowConflict => ("E0916", "Borrow conflicts with move or mutation in the same call", &[],
        &["finish the borrowing call before moving or modifying the same value"]),
    UnsupportedBorrowedOperation => ("E0917", "Borrowed operation is not supported", &[],
        &["call '.clone()' explicitly before this consuming operation"]),
    CallSiteAmpersand => ("E0918", "Ampersand is not written at the call site", &[],
        &["write 'inspect(value)'; the function signature decides whether it borrows or moves"]),
    InvalidAmpersandPosition => ("E0919", "Ampersand is outside a parameter slot", &[],
        &["write '&name: Type' in a function declaration or '&Type' in a function type parameter slot"]),
}

#[derive(Clone, PartialEq, Eq)]
pub struct KuError {
    pub kind: KuErrorKind,
    pub message: String,
    pub span: Span,
    pub domain: Option<Box<str>>,
    pub code: Option<Box<str>>,
    diagnostic_id: Option<DiagnosticId>,
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
            diagnostic_id: None,
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
            diagnostic_id: None,
            diagnostic_context: None,
        }
    }

    pub fn package(code: impl Into<String>, message: impl Into<String>, span: Span) -> Self {
        Self::structured(KuErrorKind::Runtime, "package", code, message, span)
            .with_diagnostic_id(DiagnosticId::PackageError)
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

    /// Attach a compiler diagnostic identity without relying on English wording.
    /// This does not replace a recoverable runtime error's domain or code.
    pub fn with_diagnostic_id(mut self, id: DiagnosticId) -> Self {
        self.diagnostic_id = Some(id);
        self
    }

    pub fn diagnostic_id(&self) -> DiagnosticId {
        self.diagnostic_id
            .unwrap_or_else(|| self.legacy_diagnostic_id())
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
            notes: info.notes.to_vec(),
            helps: info.helps.to_vec(),
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

    fn diagnostic_info(&self) -> &'static DiagnosticDefinition {
        self.diagnostic_id().definition()
    }

    // Compatibility adapter for producers not yet carrying a DiagnosticId.
    // Keep it separate from registration: wording changes must not create codes.
    fn legacy_diagnostic_id(&self) -> DiagnosticId {
        use DiagnosticId::*;

        if self.domain_name() == Some("package") {
            return PackageError;
        }
        let message = self.message.as_str();
        if message.contains("cannot modify through borrowed parameter") {
            return BorrowedMutation;
        }
        if message.contains("cannot move out of borrowed value") {
            return BorrowedMove;
        }
        if message.contains("borrowed value escapes current call") {
            return BorrowedEscape;
        }
        if message.contains("async functions cannot declare borrowed parameters") {
            return AsyncBorrowedParameter;
        }
        if message.contains("callable parameter mode mismatch") {
            return CallableModeMismatch;
        }
        if message.contains("cannot pass borrowed value") {
            return BorrowedOwningArgument;
        }
        if message.contains("borrow conflicts with move or mutation in the same call") {
            return SameCallBorrowConflict;
        }
        if message.contains("borrowed operation is not supported") {
            return UnsupportedBorrowedOperation;
        }
        if message.contains("'&' is not written at the call site") {
            return CallSiteAmpersand;
        }
        if message.contains("single '&' is only allowed") || message.contains("'&' is only valid") {
            return InvalidAmpersandPosition;
        }
        if message.contains("'let' is not supported") {
            return UnsupportedLet;
        }
        if message.contains("switch is not supported") {
            return UnsupportedSwitch;
        }
        if message.contains("condition must be bool") {
            return NonBooleanCondition;
        }
        if message.contains("'?' requires a Result return type")
            || message.contains("'?' expects Result")
        {
            return InvalidResultPropagation;
        }
        if message.contains("http handler cannot modify captured variable") {
            return HttpCapturedMutation;
        }
        if message.contains("unknown std module") {
            return UnknownStdModule;
        }
        if message.contains("not exported by")
            || message.contains("has no exported function")
            || message.contains("has no exported type")
        {
            return MemberNotExported;
        }
        if message.contains("std module") && message.contains("must be imported") {
            return StdImportRequired;
        }
        if message.contains("has already been awaited") {
            return TaskAlreadyAwaited;
        }
        if message.contains("task values cannot be cloned") {
            return TaskCannotClone;
        }
        if message.contains("task handles can only be awaited") {
            return InvalidTaskOperation;
        }
        if message.contains("cannot move captured owned value") {
            return CapturedOwnedMove;
        }
        if message.contains("use of moved value")
            || message.contains("cannot move an owned field or indexed element")
            || message.contains("cannot move outer owned value")
        {
            return InvalidOwnedMove;
        }
        if message.contains("unused local binding") {
            return UnusedLocal;
        }
        if message.contains("unused import") {
            return UnusedImport;
        }
        if message.contains("std module member 'http.service'")
            || message.contains("std module member 'http.server'")
        {
            return ConstructorRequiresCall;
        }
        if message.contains("http handler parameter")
            || message.contains("res/writer parameters are not allowed")
            || message.contains("side-effect response API")
            || message.contains("ordinary HTTP route handler")
            || message.contains("fn(req, res) is not allowed")
        {
            return HttpHandlerSignature;
        }
        if message.contains("handler did not return HttpResponse")
            || message.contains("HTTP handler must return HttpResponse")
        {
            return HttpHandlerReturn;
        }
        if message.starts_with("type error:") || message.contains("type mismatch") {
            return TypeMismatch;
        }
        if message.contains("not exhaustive") {
            return NonExhaustiveMatch;
        }
        if message.contains("unreachable match arm")
            || message.contains("match arm after catch-all pattern is unreachable")
            || message.contains("match arm pattern is unreachable")
            || (message.contains("match arm for '") && message.contains("' is unreachable"))
        {
            return UnreachableMatchArm;
        }
        if message.contains("std module") || message.contains("import") {
            return ImportError;
        }
        if message.contains("http ") {
            return HttpError;
        }
        match self.kind {
            KuErrorKind::Lex | KuErrorKind::Parse => SyntaxError,
            KuErrorKind::Runtime => RuntimeError,
        }
    }

    fn domain_name(&self) -> Option<&str> {
        self.domain.as_deref()
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
    fn borrow_diagnostic_codes_are_unique_and_have_actionable_help() {
        let messages = [
            "cannot modify through borrowed parameter 'value'",
            "cannot move out of borrowed value rooted at 'value'",
            "borrowed value escapes current call: cannot capture borrowed parameter 'value'",
            "async functions cannot declare borrowed parameters",
            "callable parameter mode mismatch",
            "cannot pass borrowed value rooted at 'value' to owning parameter",
            "borrow conflicts with move or mutation in the same call for 'value'",
            "borrowed operation is not supported: for iteration",
            "'&' is not written at the call site",
            "single '&' is only allowed before a function parameter",
        ];
        let mut seen = std::collections::HashSet::new();
        for (index, message) in messages.iter().enumerate() {
            let diagnostic =
                KuError::runtime(*message, Span::default()).diagnostic_data("borrow.ku", "");
            assert_eq!(diagnostic.code, format!("E{:04}", 910 + index));
            assert!(seen.insert(diagnostic.code));
            assert!(!diagnostic.helps.is_empty());
        }
        assert!(!seen.contains("E0901"));
        assert!(!seen.contains("E0904"));
    }

    #[test]
    fn borrow_diagnostic_text_golden_preserves_location_and_help() {
        let error = KuError::runtime(
            "cannot modify through borrowed parameter 'x'",
            Span::new(Position::new(2, 5, 21), Position::new(2, 10, 26)),
        );
        assert_eq!(error.diagnostic("borrow.ku", "fn F(&x: int) {\n    x = 1\n}"),
            "error[E0910]: error: cannot modify through borrowed parameter 'x'\n  --> borrow.ku:2:5\n   |\n  2 |     x = 1\n   |     ^^^^^\n   |\nnote: borrowed parameters are read-only\nhelp: remove '&' if this function should take ownership and modify its parameter");
    }

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
