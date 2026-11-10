//! Per-invocation browser host. Native execution does not enter this profile.
use crate::{
    error::{KuError, KuResult},
    span::Span,
    value::Value,
};
use std::{cell::RefCell, collections::HashSet, fmt::Write, sync::Arc};

pub mod imports;

pub const SOURCE_BYTES: usize = 64 * 1024;
pub const FILE_BYTES: usize = 256 * 1024;
pub const OUTPUT_BYTES: usize = 64 * 1024;
pub const FUEL: usize = 1_000_000;
pub const VALUE_BYTES: usize = 2 * 1024 * 1024;
pub const VALUE_DEPTH: usize = 16;

#[derive(Default)]
struct Host {
    remaining: usize,
    failed: bool,
    output: String,
    modules: Vec<Arc<ModuleContext>>,
}
struct ModuleContext {
    symbols: HashSet<String>,
    file: String,
    source: String,
}
thread_local! { static HOST: RefCell<Option<Host>> = const { RefCell::new(None) }; }

pub fn active() -> bool {
    HOST.with(|host| host.borrow().is_some())
}
pub fn register_module(symbols: HashSet<String>, file: &str, source: &str) {
    HOST.with(|host| {
        if let Some(state) = host.borrow_mut().as_mut() {
            if state.modules.iter().any(|module| module.file == file) {
                return;
            }
            let used: usize = state.modules.iter().map(|module| module.source.len()).sum();
            let count: usize = state
                .modules
                .iter()
                .map(|module| module.symbols.len())
                .sum();
            let symbol_bytes = symbols
                .iter()
                .fold(0usize, |sum, name| sum.saturating_add(name.len()));
            if state.modules.len() >= 17
                || used.saturating_add(source.len()) > FILE_BYTES + SOURCE_BYTES
                || count.saturating_add(symbols.len()) > 4096
                || symbol_bytes > state.remaining
            {
                state.failed = true;
                return;
            }
            state.remaining -= symbol_bytes;
            state.modules.push(Arc::new(ModuleContext {
                symbols,
                file: file.to_owned(),
                source: source.to_owned(),
            }));
        }
    });
}
pub fn contextualize(name: &str, error: KuError) -> KuError {
    let context = HOST.with(|host| {
        host.borrow()
            .as_ref()
            .and_then(|host| {
                host.modules
                    .iter()
                    .find(|module| module.symbols.contains(name))
            })
            .map(Arc::clone)
    });
    match context {
        Some(module) => error.with_diagnostic_context(module.file.clone(), module.source.clone()),
        None => error,
    }
}
pub fn begin() {
    HOST.with(|host| {
        *host.borrow_mut() = Some(Host {
            remaining: FUEL,
            ..Host::default()
        })
    });
}
pub fn finish() -> (String, bool, usize) {
    HOST.with(|host| {
        let state = host.borrow_mut().take().unwrap_or_default();
        (
            state.output,
            state.failed,
            FUEL.saturating_sub(state.remaining),
        )
    })
}
pub fn unsupported(capability: &str, span: Span) -> KuError {
    KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "browser",
        "unsupported_in_browser",
        format!("unsupported_in_browser: {capability}"),
        span,
    )
}
fn exhausted(span: Span) -> KuError {
    KuError::structured(
        crate::error::KuErrorKind::Runtime,
        "browser",
        "budget_exceeded",
        "budget_exceeded: browser resource limit",
        span,
    )
}
pub fn fail(span: Span) -> KuError {
    HOST.with(|host| {
        if let Some(state) = host.borrow_mut().as_mut() {
            state.failed = true;
        }
    });
    exhausted(span)
}
pub fn charge(units: usize, span: Span) -> KuResult<()> {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        let Some(state) = host.as_mut() else {
            return Ok(());
        };
        if state.failed || units > state.remaining {
            state.failed = true;
            return Err(exhausted(span));
        }
        state.remaining -= units;
        Ok(())
    })
}
/// Check before cloning, displaying, JSON encoding or extending an owned tree.
pub fn value_cost(value: &Value, depth: usize, span: Span) -> KuResult<usize> {
    if depth > VALUE_DEPTH {
        return Err(fail(span));
    }
    let mut bytes = 32usize;
    let mut add = |n: usize| -> KuResult<()> {
        bytes = bytes.checked_add(n).ok_or_else(|| fail(span))?;
        if bytes > VALUE_BYTES {
            return Err(fail(span));
        }
        Ok(())
    };
    match value {
        Value::String(s) => add(s.len())?,
        Value::Array(values) | Value::Enum { fields: values, .. } => {
            for item in values {
                add(value_cost(item, depth + 1, span)?)?;
            }
        }
        Value::Object(fields) | Value::Struct { fields, .. } => {
            for (name, item) in fields {
                add(name.len())?;
                add(value_cost(item, depth + 1, span)?)?;
            }
        }
        Value::Result { value, .. } => add(value_cost(value, depth + 1, span)?)?,
        Value::Borrowed(value) => {
            return value.with_read(span, |value| value_cost(value, depth, span))
        }
        Value::Function { .. } => add(SOURCE_BYTES)?,
        Value::Task(_) | Value::HttpListenerLease(_) => {
            return Err(unsupported("native resource", span))
        }
        _ => {}
    }
    Ok(bytes)
}
pub fn preflight_args(args: &[Value], span: Span) -> KuResult<()> {
    if !active() {
        return Ok(());
    }
    for value in args {
        charge(value_cost(value, 0, span)?, span)?;
    }
    Ok(())
}
pub fn allocation(bytes: usize, span: Span) -> KuResult<()> {
    if !active() {
        return Ok(());
    }
    if bytes > VALUE_BYTES {
        return Err(fail(span));
    }
    charge(bytes, span)
}
pub fn write_output(value: &Value, newline: bool, span: Span) -> KuResult<()> {
    preflight_args(std::slice::from_ref(value), span)?;
    struct Limited<'a> {
        host: &'a mut Host,
    }
    impl std::fmt::Write for Limited<'_> {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            if self.host.failed
                || self.host.output.len().saturating_add(text.len()) > OUTPUT_BYTES
                || text.len() > self.host.remaining
            {
                self.host.failed = true;
                return Err(std::fmt::Error);
            }
            self.host.remaining -= text.len();
            self.host.output.push_str(text);
            Ok(())
        }
    }
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        let state = host.as_mut().expect("active browser host");
        let mut writer = Limited { host: state };
        write!(writer, "{value}").map_err(|_| exhausted(span))?;
        if newline {
            writer.write_str("\n").map_err(|_| exhausted(span))?;
        }
        Ok(())
    })
}
