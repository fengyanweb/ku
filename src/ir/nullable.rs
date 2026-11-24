use super::*;

pub(super) fn nullable_type(inner: IrType) -> IrType {
    match inner {
        IrType::Null | IrType::Nullable(_) => inner,
        other => IrType::Nullable(Box::new(other)),
    }
}

pub(super) fn coerce(value: IrExpr, expected: Option<&IrType>) -> IrExpr {
    match (expected, &value.ty) {
        (Some(IrType::Nullable(inner)), IrType::Null) => intrinsic_expr(
            "__ku_nullable_null",
            vec![],
            IrType::Nullable(inner.clone()),
        ),
        (Some(IrType::Nullable(inner)), actual) if inner.as_ref() == actual => intrinsic_expr(
            "__ku_nullable_some",
            vec![value],
            IrType::Nullable(inner.clone()),
        ),
        _ => value,
    }
}

impl FunctionLowerer<'_> {
    pub(super) fn mark_nullable_clone(&self, mut value: IrExpr) -> IrExpr {
        fn contains(ty: &IrType, layouts: &IrLayoutTable, seen: &mut HashSet<String>) -> bool {
            match ty {
                IrType::Nullable(_) => true,
                IrType::Array(inner) | IrType::Result(inner) | IrType::Cell(inner) => {
                    contains(inner, layouts, seen)
                }
                IrType::Named(name) if seen.insert(name.clone()) => {
                    layouts
                        .structs
                        .iter()
                        .find(|layout| &layout.name == name)
                        .is_some_and(|layout| {
                            layout
                                .fields
                                .iter()
                                .any(|field| contains(&field.ty, layouts, seen))
                        })
                        || enum_type_name(name)
                            .and_then(|name| {
                                layouts.enums.iter().find(|layout| layout.name == name)
                            })
                            .is_some_and(|layout| {
                                layout.variants.iter().any(|variant| {
                                    variant
                                        .fields
                                        .iter()
                                        .any(|field| contains(&field.ty, layouts, seen))
                                })
                            })
                }
                _ => false,
            }
        }
        if let IrExprKind::Call {
            callee,
            kind: IrCallKind::Intrinsic(name),
            ..
        } = &mut value.kind
        {
            if name == "__ku_clone" && contains(&value.ty, self.layouts, &mut HashSet::new()) {
                *name = "__ku_clone_nullable".into();
                callee.kind = IrExprKind::Local(name.clone());
            }
        }
        value
    }

    pub(super) fn nullable_read(&self, source: &Expr, value: IrExpr) -> IrExpr {
        let ExprKind::Variable(name) = &source.kind else {
            return value;
        };
        let name = self.local_ir_name(name);
        match (self.nullable_facts.get(name), &value.ty) {
            (Some(true), IrType::Nullable(inner)) => {
                let ty = *inner.clone();
                intrinsic_expr("__ku_nullable_get", vec![value], ty)
            }
            (Some(false), IrType::Nullable(_)) => zero_expr(IrType::Null),
            _ => value,
        }
    }

    pub(super) fn apply_nullable_condition(&mut self, expr: &Expr, truth: bool) {
        match &expr.kind {
            ExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } => self.apply_nullable_condition(expr, !truth),
            ExprKind::Binary {
                left,
                op: BinaryOp::And,
                right,
            } if truth => {
                if !self.nullable_expression_may_mutate(right) {
                    self.apply_nullable_condition(left, true);
                }
                self.apply_nullable_condition(right, true);
            }
            ExprKind::Binary {
                left,
                op: BinaryOp::Or,
                right,
            } if !truth => {
                if !self.nullable_expression_may_mutate(right) {
                    self.apply_nullable_condition(left, false);
                }
                self.apply_nullable_condition(right, false);
            }
            ExprKind::Binary { left, op, right }
                if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
            {
                let source_name = match (&left.kind, &right.kind) {
                    (ExprKind::Variable(name), ExprKind::Literal(Literal::Null))
                    | (ExprKind::Literal(Literal::Null), ExprKind::Variable(name)) => name,
                    _ => return,
                };
                let name = self.local_ir_name(source_name).to_string();
                let storage = self.locals.get(&name).or_else(|| self.captures.get(&name));
                let nullable = matches!(storage, Some(IrType::Nullable(_)))
                    || matches!(storage, Some(IrType::Cell(inner)) if matches!(inner.as_ref(), IrType::Nullable(_)));
                if nullable {
                    self.nullable_facts
                        .insert(name, truth == (*op == BinaryOp::NotEqual));
                }
            }
            _ => {}
        }
    }

    pub(super) fn invalidate_nullable_call(&mut self, expr: &Expr) {
        if matches!(&expr.kind, ExprKind::Await(_))
            || matches!(&expr.kind, ExprKind::Call { callee, .. } if self.nullable_call_may_mutate(callee))
        {
            self.nullable_facts.retain(|name, _| {
                !self.captures.contains_key(name)
                    && !matches!(self.locals.get(name), Some(IrType::Cell(_)))
            });
        }
    }

    fn nullable_call_may_mutate(&self, callee: &Expr) -> bool {
        match &callee.kind {
            ExprKind::Variable(name) => {
                self.static_place_type(callee).is_some()
                    || metadata::builtin_signature(name).is_none()
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Variable(module) = &target.kind {
                    if self.static_place_type(target).is_none()
                        && metadata::dotted_signature(module, name).is_some()
                    {
                        return (module == "array" && name == "map")
                            || matches!(module.as_str(), "task" | "http");
                    }
                }
                let ty = self.static_place_type(target);
                let ty = match ty {
                    Some(IrType::Nullable(inner)) => Some(*inner),
                    other => other,
                };
                !matches!(ty, Some(IrType::Str | IrType::Array(_))) || name == "map"
            }
            _ => true,
        }
    }

    fn nullable_expression_may_mutate(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Call { callee, args } => {
                self.nullable_call_may_mutate(callee)
                    || self.nullable_expression_may_mutate(callee)
                    || args
                        .iter()
                        .any(|arg| self.nullable_expression_may_mutate(arg))
            }
            ExprKind::Await(_) | ExprKind::Match { .. } => true,
            ExprKind::Unary { expr, .. } | ExprKind::TryUnwrap { expr } => {
                self.nullable_expression_may_mutate(expr)
            }
            ExprKind::Binary { left, right, .. }
            | ExprKind::Index {
                target: left,
                index: right,
            } => {
                self.nullable_expression_may_mutate(left)
                    || self.nullable_expression_may_mutate(right)
            }
            ExprKind::Field { target, .. } | ExprKind::OptionalField { target, .. } => {
                self.nullable_expression_may_mutate(target)
            }
            ExprKind::Array(values) => values
                .iter()
                .any(|value| self.nullable_expression_may_mutate(value)),
            ExprKind::ObjectLiteral { fields } | ExprKind::StructLiteral { fields, .. } => fields
                .iter()
                .any(|(_, value)| self.nullable_expression_may_mutate(value)),
            ExprKind::Literal(Literal::TemplateString(_)) => true,
            _ => false,
        }
    }
}
