use crate::ast::{AssignTarget, Expr, ExprKind, Stmt};

// Checker recursion can skip a method's Field node. Keep this structural
// bound wider than its semantic depth limit, including function bodies.
pub(super) const MAX_EXPRESSION_TREE_DEPTH: usize = 128;

pub(super) fn expression_tree_fits(expr: &Expr) -> bool {
    expression_fits(expr, 0)
}

// Recursion here is explicitly capped before descending. Wide children are
// inspected one at a time, without allocating a second tree or a work Vec.
fn expression_fits(expr: &Expr, depth: usize) -> bool {
    if depth >= MAX_EXPRESSION_TREE_DEPTH {
        return false;
    }
    let next = depth + 1;
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Variable(_) => true,
        ExprKind::Unary { expr, .. }
        | ExprKind::Await(expr)
        | ExprKind::TryUnwrap { expr }
        | ExprKind::Field { target: expr, .. }
        | ExprKind::OptionalField { target: expr, .. } => expression_fits(expr, next),
        ExprKind::Binary { left, right, .. } => {
            expression_fits(left, next) && expression_fits(right, next)
        }
        ExprKind::Index { target, index } => {
            expression_fits(target, next) && expression_fits(index, next)
        }
        ExprKind::Call { callee, args } => {
            expression_fits(callee, next) && args.iter().all(|arg| expression_fits(arg, next))
        }
        ExprKind::Array(values) => values.iter().all(|value| expression_fits(value, next)),
        ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
            fields.iter().all(|(_, value)| expression_fits(value, next))
        }
        ExprKind::Match { value, arms } => {
            expression_fits(value, next)
                && arms.iter().all(|arm| {
                    arm.guard
                        .as_ref()
                        .is_none_or(|guard| expression_fits(guard, next))
                        && expression_fits(&arm.value, next)
                })
        }
        ExprKind::Function { body, .. } => statements_fit(body, next),
    }
}

fn statements_fit(body: &[Stmt], depth: usize) -> bool {
    if depth >= MAX_EXPRESSION_TREE_DEPTH {
        return false;
    }
    let next = depth + 1;
    body.iter().all(|statement| match statement {
        Stmt::VarDecl { value, .. }
        | Stmt::Assign { value, .. }
        | Stmt::Fail { value, .. }
        | Stmt::Panic { value, .. }
        | Stmt::Print { value, .. }
        | Stmt::Expr { expr: value, .. } => expression_fits(value, next),
        Stmt::AssignTarget { target, value, .. } | Stmt::CompoundAssign { target, value, .. } => {
            target_fits(target, next) && expression_fits(value, next)
        }
        Stmt::DestructureAssign { values, .. } => {
            values.iter().all(|value| expression_fits(value, next))
        }
        Stmt::ObjectDestructureAssign {
            bindings, value, ..
        } => {
            expression_fits(value, next)
                && bindings.iter().all(|binding| {
                    binding
                        .default
                        .as_ref()
                        .is_none_or(|value| expression_fits(value, next))
                })
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expression_fits(condition, next)
                && statements_fit(then_branch, next)
                && statements_fit(else_branch, next)
        }
        Stmt::While {
            condition, body, ..
        } => expression_fits(condition, next) && statements_fit(body, next),
        Stmt::For { iterable, body, .. } => {
            expression_fits(iterable, next) && statements_fit(body, next)
        }
        Stmt::Function(function) => statements_fit(&function.body, next),
        Stmt::Try {
            body,
            catch_body,
            finally_body,
            ..
        } => {
            statements_fit(body, next)
                && statements_fit(catch_body, next)
                && statements_fit(finally_body, next)
        }
        Stmt::Return { value, .. } => value
            .as_ref()
            .is_none_or(|value| expression_fits(value, next)),
        Stmt::Break { .. } | Stmt::Continue { .. } => true,
    })
}

fn target_fits(target: &AssignTarget, depth: usize) -> bool {
    match target {
        AssignTarget::Variable(_) => true,
        AssignTarget::Field { target, .. } => expression_fits(target, depth),
        AssignTarget::Index { target, index } => {
            expression_fits(target, depth) && expression_fits(index, depth)
        }
    }
}
