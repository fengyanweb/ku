//! Conservative, bounded eligibility check for Task-to-ordinary call bodies.

use super::{Budget, Expr, ExprKind, FnDecl, KuResult, Literal, Stmt};
use crate::ast::AssignTarget;

enum Node<'a> {
    Body(&'a [Stmt]),
    Statement(&'a Stmt),
    Expression(&'a Expr),
    Target(&'a AssignTarget),
}

pub(super) fn sync_candidate(
    declaration: &FnDecl,
    budget: &mut Budget,
    is_sync: impl Fn(&str) -> bool,
) -> KuResult<bool> {
    let mut pending = vec![Node::Body(&declaration.body)];
    let mut visited = 0usize;
    while let Some(node) = pending.pop() {
        // Unknown syntax or an inspection boundary is conservatively eligible.
        // This gate must never miss a call; it does not admit new source syntax.
        visited += 1;
        if visited > 1024 || pending.len() > 1020 {
            return Ok(true);
        }
        budget.analysis(1, declaration.span)?;
        match node {
            Node::Body(body) => {
                if pending.len() + body.len() > 1024 {
                    return Ok(true);
                }
                pending.extend(body.iter().map(Node::Statement));
            }
            Node::Statement(statement) => match statement {
                Stmt::VarDecl { value, .. }
                | Stmt::Assign { value, .. }
                | Stmt::Fail { value, .. }
                | Stmt::Panic { value, .. }
                | Stmt::Print { value, .. } => pending.push(Node::Expression(value)),
                Stmt::Expr { expr, .. } => pending.push(Node::Expression(expr)),
                Stmt::AssignTarget { target, value, .. }
                | Stmt::CompoundAssign { target, value, .. } => {
                    pending.push(Node::Target(target));
                    pending.push(Node::Expression(value));
                }
                Stmt::DestructureAssign { values, .. } => {
                    if pending.len() + values.len() > 1024 {
                        return Ok(true);
                    }
                    pending.extend(values.iter().map(Node::Expression));
                }
                Stmt::ObjectDestructureAssign {
                    bindings, value, ..
                } => {
                    if pending.len() + bindings.len() + 1 > 1024 {
                        return Ok(true);
                    }
                    pending.push(Node::Expression(value));
                    pending.extend(
                        bindings
                            .iter()
                            .filter_map(|binding| binding.default.as_ref())
                            .map(Node::Expression),
                    );
                }
                Stmt::Return { value, .. } => {
                    pending.extend(value.iter().map(Node::Expression));
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    pending.push(Node::Expression(condition));
                    pending.push(Node::Body(then_branch));
                    pending.push(Node::Body(else_branch));
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    pending.push(Node::Expression(condition));
                    pending.push(Node::Body(body));
                }
                Stmt::For { iterable, body, .. } => {
                    pending.push(Node::Expression(iterable));
                    pending.push(Node::Body(body));
                }
                Stmt::Try {
                    body,
                    catch_body,
                    finally_body,
                    ..
                } => {
                    pending.push(Node::Body(body));
                    pending.push(Node::Body(catch_body));
                    pending.push(Node::Body(finally_body));
                }
                Stmt::Function(function) => pending.push(Node::Body(&function.body)),
                Stmt::Break { .. } | Stmt::Continue { .. } => {}
            },
            Node::Target(target) => match target {
                AssignTarget::Variable(_) => {}
                AssignTarget::Field { target, .. } => pending.push(Node::Expression(target)),
                AssignTarget::Index { target, index } => {
                    pending.push(Node::Expression(target));
                    pending.push(Node::Expression(index));
                }
            },
            Node::Expression(expression) => match &expression.kind {
                ExprKind::Call { callee, args } => {
                    if matches!(&callee.kind, ExprKind::Variable(name) if is_sync(name)) {
                        return Ok(true);
                    }
                    if pending.len() + args.len() + 1 > 1024 {
                        return Ok(true);
                    }
                    pending.push(Node::Expression(callee));
                    pending.extend(args.iter().map(Node::Expression));
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::TryUnwrap { expr }
                | ExprKind::Await(expr)
                | ExprKind::Field { target: expr, .. }
                | ExprKind::OptionalField { target: expr, .. } => {
                    pending.push(Node::Expression(expr));
                }
                ExprKind::Binary { left, right, .. }
                | ExprKind::Index {
                    target: left,
                    index: right,
                } => {
                    pending.push(Node::Expression(left));
                    pending.push(Node::Expression(right));
                }
                ExprKind::Array(elements) => {
                    if pending.len() + elements.len() > 1024 {
                        return Ok(true);
                    }
                    pending.extend(elements.iter().map(Node::Expression));
                }
                ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                    if pending.len() + fields.len() > 1024 {
                        return Ok(true);
                    }
                    pending.extend(fields.iter().map(|(_, value)| Node::Expression(value)));
                }
                ExprKind::Match { value, arms } => {
                    if pending.len() + arms.len() * 2 + 1 > 1024 {
                        return Ok(true);
                    }
                    pending.push(Node::Expression(value));
                    for arm in arms {
                        pending.extend(arm.guard.iter().map(Node::Expression));
                        pending.push(Node::Expression(&arm.value));
                    }
                }
                ExprKind::Function { body, .. } => pending.push(Node::Body(body)),
                ExprKind::Variable(_)
                | ExprKind::Literal(
                    Literal::Int(_)
                    | Literal::Float(_)
                    | Literal::Bool(_)
                    | Literal::Null
                    | Literal::String(_)
                    | Literal::InputTemplate(_),
                ) => {}
                ExprKind::Literal(Literal::TemplateString(_)) => return Ok(true),
            },
        }
        if pending.len() > 1024 {
            return Ok(true);
        }
    }
    Ok(false)
}
