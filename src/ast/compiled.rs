//! Shared unsupported-async boundary for compiled consumers of the source AST.
//! These budgets bound this inspection, not parsing, lowering or process RSS.

use super::*;
use crate::error::{KuError, KuResult};

const MAX_INSPECTION_DEPTH: usize = 512;
const MAX_INSPECTION_NODES: usize = 262_144;

pub(crate) fn reject_compiled_async(program: &Program, message: &str) -> KuResult<()> {
    let mut inspection = Inspection::new(message);
    for item in &program.items {
        inspection.push(Node::Item(item), 0, Span::default())?;
    }
    inspection.run()
}

// Templates contain source until lowering parses their interpolation. Inspect
// that actual expression using the same rule, not an async-keyword text search.
pub(crate) fn reject_compiled_async_expression(expr: &Expr, message: &str) -> KuResult<()> {
    let mut inspection = Inspection::new(message);
    inspection.push(Node::Expr(expr), 0, expr.span)?;
    inspection.run()
}

enum Node<'a> {
    Item(&'a Item),
    Function(&'a FnDecl),
    Param(&'a Param),
    FunctionParam(&'a FunctionParam),
    Variant(&'a EnumVariant),
    Stmt(&'a Stmt),
    Target(&'a AssignTarget),
    Default(&'a ObjectDestructureBinding),
    Expr(&'a Expr),
    Arm(&'a MatchArm),
    Pattern(&'a MatchPattern),
    Type(&'a TypeName),
}

struct Inspection<'a> {
    pending: Vec<(Node<'a>, usize, Span)>,
    scheduled: usize,
    message: &'a str,
    max_depth: usize,
    max_nodes: usize,
}

impl<'a> Inspection<'a> {
    fn new(message: &'a str) -> Self {
        Self {
            pending: Vec::new(),
            scheduled: 0,
            message,
            max_depth: MAX_INSPECTION_DEPTH,
            max_nodes: MAX_INSPECTION_NODES,
        }
    }

    fn push(&mut self, node: Node<'a>, depth: usize, span: Span) -> KuResult<()> {
        if depth > self.max_depth {
            return Err(KuError::runtime(
                "compiled AST inspection depth limit exceeded",
                span,
            ));
        }
        if self.scheduled >= self.max_nodes {
            return Err(KuError::runtime(
                "compiled AST inspection node limit exceeded",
                span,
            ));
        }
        // Check before growing the worklist, including wide arrays/item lists.
        self.pending.try_reserve(1).map_err(|_| {
            KuError::runtime(
                "compiled AST inspection could not allocate its worklist",
                span,
            )
        })?;
        self.scheduled += 1;
        self.pending.push((node, depth, span));
        Ok(())
    }

    fn body(&mut self, body: &'a [Stmt], depth: usize, span: Span) -> KuResult<()> {
        for stmt in body {
            self.push(Node::Stmt(stmt), depth, span)?;
        }
        Ok(())
    }

    fn optional_type(
        &mut self,
        ty: &'a Option<TypeName>,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        if let Some(ty) = ty {
            self.push(Node::Type(ty), depth, span)?;
        }
        Ok(())
    }

    fn run(mut self) -> KuResult<()> {
        while let Some((node, depth, span)) = self.pending.pop() {
            let child_depth = depth + 1; // push only admits depths <= 512.
            match node {
                Node::Item(item) => match item {
                    Item::Function(function) => {
                        self.push(Node::Function(function), child_depth, function.span)?
                    }
                    Item::Struct(decl) => {
                        for field in &decl.fields {
                            self.push(Node::Param(field), child_depth, field.span)?;
                        }
                    }
                    Item::Enum(decl) => {
                        for variant in &decl.variants {
                            self.push(Node::Variant(variant), child_depth, variant.span)?;
                        }
                    }
                    Item::Import(_) | Item::Module(_) => {}
                },
                Node::Function(function) => {
                    if function.is_async {
                        return Err(KuError::runtime(self.message, function.span));
                    }
                    for param in &function.params {
                        self.push(Node::Param(param), child_depth, param.span)?;
                    }
                    self.optional_type(&function.return_type, child_depth, function.span)?;
                    self.body(&function.body, child_depth, function.span)?;
                }
                Node::Param(param) => self.optional_type(&param.ty, child_depth, param.span)?,
                Node::FunctionParam(param) => {
                    self.optional_type(&param.ty, child_depth, param.span)?
                }
                Node::Variant(variant) => {
                    for field in &variant.fields {
                        self.push(Node::Param(field), child_depth, field.span)?;
                    }
                }
                Node::Stmt(stmt) => match stmt {
                    Stmt::VarDecl {
                        ty, value, span, ..
                    } => {
                        self.optional_type(ty, child_depth, *span)?;
                        self.push(Node::Expr(value), child_depth, value.span)?;
                    }
                    Stmt::Assign { value, .. }
                    | Stmt::Fail { value, .. }
                    | Stmt::Panic { value, .. }
                    | Stmt::Print { value, .. } => {
                        self.push(Node::Expr(value), child_depth, value.span)?;
                    }
                    Stmt::AssignTarget {
                        target,
                        value,
                        span,
                    }
                    | Stmt::CompoundAssign {
                        target,
                        value,
                        span,
                        ..
                    } => {
                        self.push(Node::Target(target), child_depth, *span)?;
                        self.push(Node::Expr(value), child_depth, value.span)?;
                    }
                    Stmt::DestructureAssign { values, .. } => {
                        for value in values {
                            self.push(Node::Expr(value), child_depth, value.span)?;
                        }
                    }
                    Stmt::ObjectDestructureAssign {
                        bindings, value, ..
                    } => {
                        self.push(Node::Expr(value), child_depth, value.span)?;
                        for binding in bindings {
                            self.push(Node::Default(binding), child_depth, binding.span)?;
                        }
                    }
                    Stmt::If {
                        condition,
                        then_branch,
                        else_branch,
                        span,
                    } => {
                        self.push(Node::Expr(condition), child_depth, condition.span)?;
                        self.body(then_branch, child_depth, *span)?;
                        self.body(else_branch, child_depth, *span)?;
                    }
                    Stmt::While {
                        condition,
                        body,
                        span,
                    } => {
                        self.push(Node::Expr(condition), child_depth, condition.span)?;
                        self.body(body, child_depth, *span)?;
                    }
                    Stmt::For {
                        iterable,
                        body,
                        span,
                        ..
                    } => {
                        self.push(Node::Expr(iterable), child_depth, iterable.span)?;
                        self.body(body, child_depth, *span)?;
                    }
                    Stmt::Function(function) => {
                        self.push(Node::Function(function), child_depth, function.span)?
                    }
                    Stmt::Try {
                        body,
                        catch_body,
                        finally_body,
                        span,
                        ..
                    } => {
                        self.body(body, child_depth, *span)?;
                        self.body(catch_body, child_depth, *span)?;
                        self.body(finally_body, child_depth, *span)?;
                    }
                    Stmt::Return { value, .. } => {
                        if let Some(value) = value {
                            self.push(Node::Expr(value), child_depth, value.span)?;
                        }
                    }
                    Stmt::Expr { expr, .. } => {
                        self.push(Node::Expr(expr), child_depth, expr.span)?
                    }
                    Stmt::Break { .. } | Stmt::Continue { .. } => {}
                },
                Node::Target(target) => match target {
                    AssignTarget::Variable(_) => {}
                    AssignTarget::Index { target, index } => {
                        self.push(Node::Expr(target), child_depth, target.span)?;
                        self.push(Node::Expr(index), child_depth, index.span)?;
                    }
                    AssignTarget::Field { target, .. } => {
                        self.push(Node::Expr(target), child_depth, target.span)?
                    }
                },
                Node::Default(binding) => {
                    if let Some(value) = &binding.default {
                        self.push(Node::Expr(value), child_depth, value.span)?;
                    }
                }
                Node::Expr(expr) => match &expr.kind {
                    ExprKind::Await(_) => return Err(KuError::runtime(self.message, expr.span)),
                    ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => {
                        self.push(Node::Expr(expr), child_depth, expr.span)?
                    }
                    ExprKind::Binary { left, right, .. } => {
                        self.push(Node::Expr(left), child_depth, left.span)?;
                        self.push(Node::Expr(right), child_depth, right.span)?;
                    }
                    ExprKind::Call { callee, args } => {
                        self.push(Node::Expr(callee), child_depth, callee.span)?;
                        for arg in args {
                            self.push(Node::Expr(arg), child_depth, arg.span)?;
                        }
                    }
                    ExprKind::Array(values) => {
                        for value in values {
                            self.push(Node::Expr(value), child_depth, value.span)?;
                        }
                    }
                    ExprKind::Index { target, index } => {
                        self.push(Node::Expr(target), child_depth, target.span)?;
                        self.push(Node::Expr(index), child_depth, index.span)?;
                    }
                    ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
                        self.push(Node::Expr(target), child_depth, target.span)?
                    }
                    ExprKind::StructLiteral { fields, .. } | ExprKind::ObjectLiteral { fields } => {
                        for (_, value) in fields {
                            self.push(Node::Expr(value), child_depth, value.span)?;
                        }
                    }
                    ExprKind::Match { value, arms } => {
                        self.push(Node::Expr(value), child_depth, value.span)?;
                        for arm in arms {
                            self.push(Node::Arm(arm), child_depth, arm.span)?;
                        }
                    }
                    ExprKind::Function {
                        params,
                        return_type,
                        body,
                    } => {
                        for param in params {
                            self.push(Node::FunctionParam(param), child_depth, param.span)?;
                        }
                        self.optional_type(return_type, child_depth, expr.span)?;
                        self.body(body, child_depth, expr.span)?;
                    }
                    ExprKind::Literal(_) | ExprKind::Variable(_) => {}
                },
                Node::Arm(arm) => {
                    self.push(Node::Pattern(&arm.pattern), child_depth, arm.span)?;
                    if let Some(guard) = &arm.guard {
                        self.push(Node::Expr(guard), child_depth, guard.span)?;
                    }
                    self.push(Node::Expr(&arm.value), child_depth, arm.value.span)?;
                }
                Node::Pattern(pattern) => match pattern {
                    MatchPattern::EnumVariant { fields, .. } => {
                        for field in fields {
                            self.push(Node::Pattern(field), child_depth, span)?;
                        }
                    }
                    MatchPattern::Wildcard
                    | MatchPattern::Binding(_)
                    | MatchPattern::Literal(_) => {}
                },
                Node::Type(ty) => match ty {
                    TypeName::Function {
                        is_async,
                        params,
                        return_type,
                        ..
                    } => {
                        if *is_async {
                            return Err(KuError::runtime(self.message, span));
                        }
                        for param in params {
                            self.push(Node::Type(param), child_depth, span)?;
                        }
                        self.push(Node::Type(return_type), child_depth, span)?;
                    }
                    TypeName::Array(inner) | TypeName::Result(inner) => {
                        self.push(Node::Type(inner), child_depth, span)?
                    }
                    TypeName::Union(members) => {
                        for member in members {
                            self.push(Node::Type(member), child_depth, span)?;
                        }
                    }
                    TypeName::Int
                    | TypeName::Float
                    | TypeName::Bool
                    | TypeName::String
                    | TypeName::Null
                    | TypeName::Custom(_) => {}
                },
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &str = "async is not supported in this compiled test";

    fn int() -> Expr {
        Expr::new(ExprKind::Literal(Literal::Int(0)), Span::default())
    }

    fn awaited() -> Expr {
        Expr::new(ExprKind::Await(Box::new(int())), Span::default())
    }

    #[test]
    fn compiled_async_inspection_exact_depth_boundary() {
        let mut expression = int();
        for _ in 0..MAX_INSPECTION_DEPTH {
            expression = Expr::new(
                ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expression),
                },
                Span::default(),
            );
        }
        reject_compiled_async_expression(&expression, MESSAGE).expect("exact inspection depth");
        expression = Expr::new(
            ExprKind::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expression),
            },
            Span::default(),
        );
        let error = reject_compiled_async_expression(&expression, MESSAGE).expect_err("depth + 1");
        assert!(error.message.contains("depth limit"));
    }

    #[test]
    fn compiled_async_inspection_counts_nodes_before_enqueue() {
        // A tiny injected inspection budget exercises the exact production
        // admission logic without constructing hundreds of thousands of nodes.
        let expression = Expr::new(ExprKind::Array(vec![int(), int(), int()]), Span::default());
        let mut exact = Inspection::new(MESSAGE);
        exact.max_nodes = 4;
        exact
            .push(Node::Expr(&expression), 0, expression.span)
            .unwrap();
        exact.run().expect("array plus three elements");
        let mut short = Inspection::new(MESSAGE);
        short.max_nodes = 3;
        short
            .push(Node::Expr(&expression), 0, expression.span)
            .unwrap();
        assert!(short
            .run()
            .expect_err("fourth node rejected")
            .message
            .contains("node limit"));

        let mut admission = Inspection::new(MESSAGE);
        admission.max_nodes = 1;
        admission
            .push(Node::Expr(&expression), 0, expression.span)
            .unwrap();
        assert!(admission
            .push(Node::Expr(&expression), 0, expression.span)
            .is_err());
        assert_eq!(admission.pending.len(), 1);
        assert_eq!(admission.scheduled, 1);
    }

    #[test]
    fn compiled_async_inspection_covers_defaults_targets_match_guards_and_finally() {
        let span = Span::default();
        let statements = [
            Stmt::ObjectDestructureAssign {
                bindings: vec![ObjectDestructureBinding {
                    field: "x".to_string(),
                    local: Some("x".to_string()),
                    default: Some(awaited()),
                    span,
                }],
                rest: None,
                value: int(),
                span,
            },
            Stmt::AssignTarget {
                target: AssignTarget::Index {
                    target: int(),
                    index: awaited(),
                },
                value: int(),
                span,
            },
            Stmt::CompoundAssign {
                target: AssignTarget::Field {
                    target: awaited(),
                    name: "x".to_string(),
                },
                op: BinaryOp::Add,
                value: int(),
                span,
            },
            Stmt::Expr {
                expr: Expr::new(
                    ExprKind::Match {
                        value: Box::new(int()),
                        arms: vec![MatchArm {
                            pattern: MatchPattern::Wildcard,
                            guard: Some(awaited()),
                            value: int(),
                            span,
                        }],
                    },
                    span,
                ),
                span,
            },
            Stmt::Try {
                body: vec![],
                catch_name: None,
                catch_body: vec![],
                finally_body: vec![Stmt::Expr {
                    expr: awaited(),
                    span,
                }],
                span,
            },
        ];
        for statement in &statements {
            let mut inspection = Inspection::new(MESSAGE);
            inspection.push(Node::Stmt(statement), 0, span).unwrap();
            assert_eq!(
                inspection
                    .run()
                    .expect_err("nested async must be visited")
                    .message,
                MESSAGE
            );
        }
    }
}
