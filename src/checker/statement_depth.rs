//! Bound statement/body recursion before declaration clones and capture scans.
//! This is not an expression, type, pattern, total-work, or raw-AST memory limit.
use super::stmt_span;
use crate::{
    ast::*,
    error::{KuError, KuResult},
    span::Span,
};

const MAX_STATEMENT_DEPTH: usize = 32;

#[cfg(test)]
mod tests;

enum Pending<'a> {
    Statements(&'a [Stmt], usize),
    Statement(&'a Stmt, usize),
    Expression(&'a Expr, usize),
    Expressions(&'a [Expr], usize),
    Fields(&'a [(String, Expr)], usize),
    Bindings(&'a [ObjectDestructureBinding], usize),
    Arms(&'a [MatchArm], usize),
    Target(&'a AssignTarget, usize),
}

fn enter(depth: usize, span: Span) -> KuResult<usize> {
    if depth >= MAX_STATEMENT_DEPTH {
        return Err(KuError::runtime(
            "maximum check depth exceeded; statement body is too deeply nested",
            span,
        ));
    }
    Ok(depth + 1)
}

pub(super) fn check(program: &Program) -> KuResult<()> {
    let mut pending = Vec::new();
    for item in &program.items {
        let Item::Function(function) = item else {
            continue;
        };
        // Root functions do not consume a level. Slice cursors retain only the
        // unvisited suffix, avoiding an eager worklist allocation per sibling.
        pending.push(Pending::Statements(&function.body, 0));
        while let Some(node) = pending.pop() {
            match node {
                Pending::Statements(statements, depth) => {
                    if let Some((first, rest)) = statements.split_first() {
                        pending.push(Pending::Statements(rest, depth));
                        pending.push(Pending::Statement(first, depth));
                    }
                }
                Pending::Expressions(expressions, depth) => {
                    if let Some((first, rest)) = expressions.split_first() {
                        pending.push(Pending::Expressions(rest, depth));
                        pending.push(Pending::Expression(first, depth));
                    }
                }
                Pending::Fields(fields, depth) => {
                    if let Some(((_, value), rest)) = fields.split_first() {
                        pending.push(Pending::Fields(rest, depth));
                        pending.push(Pending::Expression(value, depth));
                    }
                }
                Pending::Bindings(bindings, depth) => {
                    if let Some((first, rest)) = bindings.split_first() {
                        pending.push(Pending::Bindings(rest, depth));
                        if let Some(default) = &first.default {
                            pending.push(Pending::Expression(default, depth));
                        }
                    }
                }
                Pending::Arms(arms, depth) => {
                    if let Some((first, rest)) = arms.split_first() {
                        pending.push(Pending::Arms(rest, depth));
                        pending.push(Pending::Expression(&first.value, depth));
                        if let Some(guard) = &first.guard {
                            pending.push(Pending::Expression(guard, depth));
                        }
                    }
                }
                Pending::Target(target, depth) => match target {
                    AssignTarget::Variable(_) => {}
                    AssignTarget::Field { target, .. } => {
                        pending.push(Pending::Expression(target, depth));
                    }
                    AssignTarget::Index { target, index } => {
                        pending.push(Pending::Expression(index, depth));
                        pending.push(Pending::Expression(target, depth));
                    }
                },
                Pending::Statement(statement, depth) => match statement {
                    Stmt::If {
                        condition,
                        then_branch,
                        else_branch,
                        ..
                    } => {
                        let depth = enter(depth, stmt_span(statement))?;
                        pending.push(Pending::Statements(else_branch, depth));
                        pending.push(Pending::Statements(then_branch, depth));
                        pending.push(Pending::Expression(condition, depth));
                    }
                    Stmt::While {
                        condition, body, ..
                    } => {
                        let depth = enter(depth, stmt_span(statement))?;
                        pending.push(Pending::Statements(body, depth));
                        pending.push(Pending::Expression(condition, depth));
                    }
                    Stmt::For { iterable, body, .. } => {
                        let depth = enter(depth, stmt_span(statement))?;
                        pending.push(Pending::Statements(body, depth));
                        pending.push(Pending::Expression(iterable, depth));
                    }
                    Stmt::Try {
                        body,
                        catch_body,
                        finally_body,
                        ..
                    } => {
                        let depth = enter(depth, stmt_span(statement))?;
                        pending.push(Pending::Statements(finally_body, depth));
                        pending.push(Pending::Statements(catch_body, depth));
                        pending.push(Pending::Statements(body, depth));
                    }
                    Stmt::Function(function) => {
                        let depth = enter(depth, function.span)?;
                        pending.push(Pending::Statements(&function.body, depth));
                    }
                    Stmt::VarDecl { value, .. }
                    | Stmt::Assign { value, .. }
                    | Stmt::Fail { value, .. }
                    | Stmt::Panic { value, .. }
                    | Stmt::Print { value, .. } => {
                        pending.push(Pending::Expression(value, depth));
                    }
                    Stmt::AssignTarget { target, value, .. }
                    | Stmt::CompoundAssign { target, value, .. } => {
                        pending.push(Pending::Expression(value, depth));
                        pending.push(Pending::Target(target, depth));
                    }
                    Stmt::DestructureAssign { values, .. } => {
                        pending.push(Pending::Expressions(values, depth));
                    }
                    Stmt::ObjectDestructureAssign {
                        bindings, value, ..
                    } => {
                        pending.push(Pending::Expression(value, depth));
                        pending.push(Pending::Bindings(bindings, depth));
                    }
                    Stmt::Return { value, .. } => {
                        if let Some(value) = value {
                            pending.push(Pending::Expression(value, depth));
                        }
                    }
                    Stmt::Expr { expr, .. } => {
                        pending.push(Pending::Expression(expr, depth));
                    }
                    Stmt::Break { .. } | Stmt::Continue { .. } => {}
                },
                Pending::Expression(expression, depth) => match &expression.kind {
                    ExprKind::Function { body, .. } => {
                        let depth = enter(depth, expression.span)?;
                        pending.push(Pending::Statements(body, depth));
                    }
                    ExprKind::Unary { expr, .. }
                    | ExprKind::Await(expr)
                    | ExprKind::TryUnwrap { expr } => {
                        pending.push(Pending::Expression(expr, depth));
                    }
                    ExprKind::Binary { left, right, .. } => {
                        pending.push(Pending::Expression(right, depth));
                        pending.push(Pending::Expression(left, depth));
                    }
                    ExprKind::Call { callee, args } => {
                        pending.push(Pending::Expressions(args, depth));
                        pending.push(Pending::Expression(callee, depth));
                    }
                    ExprKind::Array(values) => {
                        pending.push(Pending::Expressions(values, depth));
                    }
                    ExprKind::Index { target, index } => {
                        pending.push(Pending::Expression(index, depth));
                        pending.push(Pending::Expression(target, depth));
                    }
                    ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
                        pending.push(Pending::Expression(target, depth));
                    }
                    ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                        pending.push(Pending::Fields(fields, depth));
                    }
                    ExprKind::Match { value, arms } => {
                        // Types and match patterns cannot contain statement bodies.
                        pending.push(Pending::Arms(arms, depth));
                        pending.push(Pending::Expression(value, depth));
                    }
                    ExprKind::Literal(_) | ExprKind::Variable(_) => {}
                },
            }
        }
    }
    Ok(())
}
