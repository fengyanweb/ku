//! Read-only virtual imports, sharing the selected release's import semantics.
use crate::{
    ast::*,
    checker::Checker,
    error::{KuError, KuResult},
    lexer::Lexer,
    parser::Parser,
    span::Span,
    stdlib,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_IMPORT_MODULES: usize = 17; // main + at most sixteen user modules
const MAX_IMPORT_DEPTH: usize = 16;
const MAX_IMPORT_SOURCE_BYTES: u64 = 320 * 1024;
const MAX_IMPORT_EXPANDED_ITEMS: usize = 4096;
const MAX_IMPORT_CLONED_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_IMPORT_EDGES: usize = 256;
const MAX_IMPORT_BINDINGS: usize = 1024;

include!(concat!(env!("OUT_DIR"), "/browser_import_support.rs"));

pub fn prepare(source: &str, files: &HashMap<String, String>) -> KuResult<Program> {
    let span = Span::default();
    if source.len() > super::SOURCE_BYTES || files.len() > 16 {
        return Err(import_limit_error(
            "source_limit",
            "browser source/file limit exceeded",
            span,
        ));
    }
    let mut normalized = HashMap::new();
    let mut total = 0usize;
    for (name, text) in files {
        total = total.checked_add(text.len()).ok_or_else(|| {
            import_limit_error("source_limit", "virtual files size overflow", span)
        })?;
        if total > super::FILE_BYTES {
            return Err(import_limit_error(
                "source_limit",
                "virtual files exceed 256 KiB",
                span,
            ));
        }
        let path = canonical_file(Path::new(name), span)?;
        let name = path
            .to_str()
            .ok_or_else(|| import_limit_error("path", "invalid virtual path", span))?
            .to_string();
        if name == "main.ku" || normalized.insert(name, text.clone()).is_some() {
            return Err(import_limit_error(
                "path",
                "duplicate virtual path or reserved main.ku",
                span,
            ));
        }
    }
    let program = parse_source(source)?;
    deny_unused_imports(&program)?;
    let mut loader = ModuleLoader::new(&normalized)?;
    let program = loader.load_entry(Path::new("main.ku"), program, source.len())?;
    for (path, id) in &loader.module_ids {
        if let Some(name) = path.to_str() {
            if let Some(text) = normalized.get(name) {
                let symbols = loader.materialized_modules[*id - 1]
                    .iter()
                    .filter_map(|item| match item {
                        Item::Function(function) => Some(function.name.clone()),
                        _ => None,
                    })
                    .collect::<HashSet<_>>();
                super::register_module(symbols, name, text);
            }
        }
    }
    Ok(program)
}

fn parse_source(source: &str) -> KuResult<Program> {
    let tokens = Lexer::new(source).tokenize()?;
    super::charge(tokens.len(), Span::default())?;
    Parser::new(tokens).parse_program()
}

fn std_import_modules(import: &ImportDecl) -> KuResult<Option<Vec<String>>> {
    let modules = native_std_import_modules(import)?;
    if let Some(names) = &modules {
        for name in names {
            if !matches!(
                name.as_str(),
                "json" | "string" | "array" | "object" | "lexer" | "parser" | "time"
            ) {
                return Err(KuError::structured(
                    crate::error::KuErrorKind::Runtime,
                    "browser",
                    "unsupported_in_browser",
                    format!("unsupported_in_browser: std.{name} is not available"),
                    import.span,
                ));
            }
        }
    }
    Ok(modules)
}

fn canonical_file(path: &Path, span: Span) -> KuResult<PathBuf> {
    let text = path
        .to_str()
        .ok_or_else(|| import_limit_error("path", "invalid virtual path", span))?;
    if text.starts_with('/') || text.contains(['\\', ':', '\0', '?', '#']) || text.len() > 512 {
        return Err(import_limit_error(
            "path",
            "virtual imports reject absolute paths and URLs",
            span,
        ));
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(import_limit_error(
                        "path",
                        "virtual import escapes its root",
                        span,
                    ));
                }
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(import_limit_error("path", "empty virtual path", span));
    }
    let path = PathBuf::from(parts.join("/"));
    if path.extension().and_then(|x| x.to_str()) != Some("ku") {
        return Err(import_limit_error(
            "path",
            "virtual module must be a .ku file",
            span,
        ));
    }
    Ok(path)
}

fn resolve_import_path(current: &Path, import: &str, span: Span) -> KuResult<PathBuf> {
    // Check the raw spelling before joining so an absolute path cannot erase
    // the virtual parent and a Windows path cannot become a normal filename.
    if import.starts_with('/') || import.contains(['\\', ':', '\0', '?', '#']) {
        return Err(import_limit_error(
            "path",
            "virtual imports reject absolute paths and URLs",
            span,
        ));
    }
    let mut path = current
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(import);
    if path.extension().is_none() {
        path.set_extension("ku");
    }
    canonical_file(&path, span)
}

fn read_import_source(
    files: &HashMap<String, String>,
    path: &Path,
    span: Span,
) -> KuResult<String> {
    let name = path
        .to_str()
        .ok_or_else(|| import_limit_error("path", "invalid virtual path", span))?;
    files.get(name).cloned().ok_or_else(|| {
        import_limit_error(
            "not_found",
            format!("virtual module '{name}' was not provided"),
            span,
        )
    })
}
