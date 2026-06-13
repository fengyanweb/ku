use std::path::Path;

use crate::{
    ast::{Expr, ExprKind},
    error::KuResult,
    span::Span,
    value::Value,
};

pub mod array;
pub mod compiler;
pub mod core;
pub mod fs;
pub mod json;
pub(crate) mod metadata;
pub mod string;
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
    match module.as_str() {
        "fs" => fs::eval(&function, args, span, base_dir),
        "lexer" | "parser" => compiler::eval(&module, &function, args, span),
        "string" => string::eval(&function, args, span),
        "array" => array::eval(&function, args, span),
        "json" => json::eval(&function, args, span),
        "time" => time::eval(&function, args, span),
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
