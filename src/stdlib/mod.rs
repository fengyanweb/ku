use std::path::Path;

use crate::{
    ast::{Expr, ExprKind},
    error::KuResult,
    span::Span,
    value::Value,
};

pub mod array;
pub mod compiler;
pub mod config;
pub mod core;
pub mod errors;
pub mod fs;
pub mod http;
pub(crate) mod http_client_engine;
pub mod json;
pub(crate) mod metadata;
pub mod object;
pub mod string;
#[cfg(not(target_arch = "wasm32"))]
pub mod time;
#[cfg(target_arch = "wasm32")]
#[path = "../browser/time.rs"]
pub mod time;

pub fn eval_builtin(name: &str, args: &[Value], span: Span) -> KuResult<Option<Value>> {
    core::eval_builtin(name, args, span)
}

pub fn eval_dotted_builtin(
    callee: &Expr,
    args: &[Value],
    span: Span,
    base_dir: &Path,
) -> KuResult<Option<Value>> {
    let Some((module, function)) = dotted_name(callee) else {
        return Ok(None);
    };
    #[cfg(feature = "browser")]
    if crate::browser::active() && matches!(module.as_str(), "fs" | "config" | "http") {
        return Err(crate::browser::unsupported(&module, span));
    }
    #[cfg(feature = "browser")]
    crate::browser::preflight_args(args, span)?;
    match module.as_str() {
        "fs" => fs::eval(&function, args, span, base_dir),
        "lexer" | "parser" => compiler::eval(&module, &function, args, span),
        "string" => string::eval(&function, args, span),
        "array" => array::eval(&function, args, span),
        "config" => config::eval(&function, args, span, base_dir),
        "json" => json::eval(&function, args, span),
        "object" => object::eval(&function, args, span),
        "time" => time::eval(&function, args, span),
        "http" => http::eval(&function, args, span),
        _ => Ok(None),
    }
}

fn dotted_name(expr: &Expr) -> Option<(String, String)> {
    let ExprKind::Field { target, name } = &expr.kind else {
        return None;
    };
    let ExprKind::Variable(module) = &target.kind else {
        return None;
    };
    Some((module.clone(), name.clone()))
}
