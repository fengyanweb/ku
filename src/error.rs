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
        let data = self.diagnostic_data(file, source);
        let (_, source) = self
            .diagnostic_context
            .as_ref()
            .map(|context| (context.file.as_str(), context.source.as_str()))
            .unwrap_or((file, source));
        let line = data.line;
        let column = data.column;
        let line_text = source.lines().nth(line.saturating_sub(1)).unwrap_or("");
        let caret_len = if data.end_line == line {
            data.end_column.saturating_sub(column).max(1)
        } else {
            1
        };
        let marker = format!(
            "{}{}",
            " ".repeat(column.saturating_sub(1)),
            "^".repeat(caret_len)
        );

        let mut output = format!(
            "error[{}]: error: {}\n  --> {}:{line}:{column}\n   |\n{line:>3} | {line_text}\n   | {marker}",
            data.code, data.message, data.file
        );
        for note in data.notes {
            output.push_str(&format!("\n   |\nnote: {note}"));
        }
        for help in data.helps {
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
        if message.contains("unused local binding") {
            return DiagnosticInfo::new("E0905")
                .note("strict unused checks are enabled by `ku check --deny-unused`")
                .help("remove the binding, use it, or rename it with a leading `_` when it is intentionally unused");
        }
        if message.contains("unused import") {
            return DiagnosticInfo::new("E0901")
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
