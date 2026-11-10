use ku::{browser, checker::Checker, error::KuError, interpreter::Interpreter, span::Span};
use serde_json::{json, Value};
use std::collections::HashMap;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn engine_info() -> String {
    info().to_string()
}

fn info() -> Value {
    json!({
        "version":ku::LANGUAGE_VERSION,
        "buildId":option_env!("KU_WEB_BUILD_ID").unwrap_or("unverified-local-preview"),
        "sourceCommit":option_env!("KU_WEB_RELEASE_COMMIT").unwrap_or("unknown"),
        "releaseCommit":option_env!("KU_WEB_RELEASE_COMMIT").unwrap_or("unknown"),
        "releaseTag":option_env!("KU_WEB_RELEASE_TAG").unwrap_or("unverified"),
        "sourceDigest":option_env!("KU_WEB_SOURCE_SHA256").unwrap_or("unknown"),
        "overlayDigest":option_env!("KU_WEB_OVERLAY_SHA256").unwrap_or("unknown"),
        "dirty":true,"preview":true,"engine":"wasm-interpreter","protocolVersion":1,
        "capabilities":["expressions","control-flow","functions","closures","ownership","structs","enums","result","json","unicode","virtual-imports","string","array","object","lexer","parser","clock"],
        "unsupported":["native-network","http-server","database","process","native-compilation","disk","os-threads","async-executor"],
        "budgets":{"sourceBytes":browser::SOURCE_BYTES,"fileCount":16,"fileBytes":browser::FILE_BYTES,"outputBytes":browser::OUTPUT_BYTES,"diagnostics":64,"fuel":browser::FUEL,"callDepth":32,"valueDepth":browser::VALUE_DEPTH,"valueBytes":browser::VALUE_BYTES,"memoryBytes":134217728,"runTimeoutMs":5000,"initTimeoutMs":15000},
        "diagnosticUnits":{"offset":"utf8-bytes","line":"one-based","column":"one-based-unicode-scalars"}
    })
}

#[wasm_bindgen]
pub fn check(request: &str) -> String {
    execute(request, false)
}
#[wasm_bindgen]
pub fn run(request: &str) -> String {
    execute(request, true)
}

fn execute(request: &str, run: bool) -> String {
    browser::begin();
    let result = execute_inner(request, run);
    let (stdout, exhausted, used) = browser::finish();
    let (status, diagnostics) = match result {
        Ok(()) if !exhausted => ("success", vec![]),
        Ok(()) => ("budget_exceeded", vec![]),
        Err((mut status, err)) => {
            let import_budget = err.domain.as_deref() == Some("import")
                && matches!(
                    err.code.as_deref(),
                    Some(
                        "module_limit"
                            | "depth_limit"
                            | "source_limit"
                            | "edge_limit"
                            | "binding_limit"
                            | "expanded_item_limit"
                            | "expanded_clone_limit"
                    )
                );
            if exhausted
                || import_budget
                || (err.domain.as_deref() == Some("browser")
                    && err.code.as_deref() == Some("budget_exceeded"))
            {
                status = "budget_exceeded";
            } else if err.domain.as_deref() == Some("browser")
                && err.code.as_deref() == Some("unsupported_in_browser")
            {
                status = "unsupported_in_browser";
            }
            let mut limit = err.message.len().min(2048);
            while !err.message.is_char_boundary(limit) {
                limit -= 1;
            }
            let message = err.message[..limit].to_owned();
            let diagnostic = err.diagnostic_data("main.ku", "");
            (
                status,
                vec![
                    json!({"message":message,"severity":"error","file":diagnostic.file,"offset":err.span.start.offset,"endOffset":err.span.end.offset,"line":err.span.start.line.max(1),"column":err.span.start.column.max(1),"offsetUnit":"utf8-bytes","columnUnit":"unicode-scalars"}),
                ],
            )
        }
    };
    json!({"status":status,"stdout":stdout,"stderr":"","diagnostics":diagnostics,"engine":info(),"fuelUsed":used}).to_string()
}

#[expect(
    clippy::result_large_err,
    reason = "browser execution preserves the existing structured diagnostic without another allocation"
)]
fn execute_inner(request: &str, run: bool) -> Result<(), (&'static str, KuError)> {
    let span = Span::default();
    let invalid = |message: &str| ("compile_error", KuError::runtime(message, span));
    if request.len() > 2 * 1024 * 1024 {
        return Err(("budget_exceeded", browser::fail(span)));
    }
    let request: Value =
        serde_json::from_str(request).map_err(|_| invalid("invalid request JSON"))?;
    let source = request
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("source must be text"))?;
    if source.len() > browser::SOURCE_BYTES {
        return Err(("budget_exceeded", browser::fail(span)));
    }
    let mut files = HashMap::new();
    let mut total = 0usize;
    if let Some(items) = request.get("files") {
        let items = items
            .as_array()
            .ok_or_else(|| invalid("files must be an array"))?;
        if items.len() > 16 {
            return Err(("budget_exceeded", browser::fail(span)));
        }
        for item in items {
            let path = item
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("file.path must be text"))?;
            let text = item
                .get("source")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("file.source must be text"))?;
            total = total.saturating_add(text.len());
            if total > browser::FILE_BYTES {
                return Err(("budget_exceeded", browser::fail(span)));
            }
            if files.insert(path.to_owned(), text.to_owned()).is_some() {
                return Err(invalid("duplicate virtual file"));
            }
        }
    }
    browser::charge(source.len() + total, span).map_err(|e| ("budget_exceeded", e))?;
    let program = browser::imports::prepare(source, &files).map_err(|e| ("compile_error", e))?;
    Checker::new()
        .check(&program)
        .map_err(|e| ("compile_error", e))?;
    if run {
        Interpreter::with_base_dir("/")
            .run(program)
            .map_err(|e| ("runtime_error", e))?;
    }
    browser::charge(1, span).map_err(|e| ("budget_exceeded", e))?;
    Ok(())
}
