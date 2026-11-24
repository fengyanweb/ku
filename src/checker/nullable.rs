use super::*;

pub(super) fn contains_null(ty: &Type) -> bool {
    match ty {
        Type::Null => true,
        Type::Union(parts) => parts.iter().any(contains_null),
        _ => false,
    }
}

pub(super) fn make_nullable(ty: Type) -> Type {
    fn append(ty: Type, parts: &mut Vec<Type>) {
        if let Type::Union(nested) = ty {
            for part in nested {
                append(part, parts);
            }
        } else if !parts.contains(&ty) {
            parts.push(ty);
        }
    }
    let mut parts = Vec::new();
    append(ty, &mut parts);
    append(Type::Null, &mut parts);
    if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        Type::Union(parts)
    }
}

fn non_null(ty: &Type) -> Option<Type> {
    match ty {
        Type::Null => None,
        Type::Union(parts) => {
            let parts: Vec<_> = parts.iter().filter_map(non_null).collect();
            match parts.len() {
                0 => None,
                1 => parts.into_iter().next(),
                _ => Some(Type::Union(parts)),
            }
        }
        _ => Some(ty.clone()),
    }
}

impl Checker {
    /// Facts affect reads only. The binding retains its declared T|null storage
    /// type, so writing null after a successful check remains legal.
    pub(super) fn apply_nullable_condition(&mut self, condition: &Expr, truth: bool) {
        match &condition.kind {
            ExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } => {
                self.apply_nullable_condition(expr, !truth);
            }
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
                let name = match (&left.kind, &right.kind) {
                    (ExprKind::Variable(name), ExprKind::Literal(Literal::Null))
                    | (ExprKind::Literal(Literal::Null), ExprKind::Variable(name)) => name,
                    _ => return,
                };
                let wants_null = truth == (*op == BinaryOp::Equal);
                for scope in self.scopes.iter_mut().rev() {
                    if let Some(var) = scope.get_mut(name) {
                        if contains_null(&var.ty) {
                            var.narrowed_type = if wants_null {
                                Some(Type::Null)
                            } else {
                                non_null(&var.ty)
                            };
                        }
                        return;
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn nullable_facts(&self) -> Vec<(BindingId, Option<Type>)> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.values())
            .filter(|var| contains_null(&var.ty))
            .map(|var| (var.binding_id, var.narrowed_type.clone()))
            .collect()
    }

    pub(super) fn clear_nullable_facts(&mut self) {
        for var in self.scopes.iter_mut().flat_map(|scope| scope.values_mut()) {
            var.narrowed_type = None;
        }
    }

    pub(super) fn restore_nullable_facts(&mut self, facts: &[(BindingId, Option<Type>)]) {
        for var in self.scopes.iter_mut().flat_map(|scope| scope.values_mut()) {
            if let Some((_, fact)) = facts.iter().find(|(id, _)| *id == var.binding_id) {
                var.narrowed_type = fact.clone();
            }
        }
    }

    /// A short-circuit RHS is conditional. A proof established only while
    /// checking it cannot escape to the path where that RHS was skipped.
    pub(super) fn join_nullable_facts(&mut self, facts: &[(BindingId, Option<Type>)]) {
        for var in self.scopes.iter_mut().flat_map(|scope| scope.values_mut()) {
            if let Some((_, fact)) = facts.iter().find(|(id, _)| *id == var.binding_id) {
                if &var.narrowed_type != fact {
                    var.narrowed_type = None;
                }
            }
        }
    }

    pub(super) fn invalidate_captured_nullable_facts(&mut self) {
        for var in self.scopes.iter_mut().flat_map(|scope| scope.values_mut()) {
            if var.captured {
                var.narrowed_type = None;
            }
        }
    }

    pub(super) fn nullable_call_may_mutate(&self, callee: &Expr) -> bool {
        match &callee.kind {
            ExprKind::Variable(name) => {
                self.contains(name) || metadata::builtin_signature(name).is_none()
            }
            ExprKind::Field { target, name } => {
                if let ExprKind::Variable(module) = &target.kind {
                    if !self.contains(module) && metadata::dotted_signature(module, name).is_some()
                    {
                        return !Self::closure_effect_noninvoking_dotted(module, name);
                    }
                }
                let receiver = match &target.kind {
                    ExprKind::Literal(Literal::String(_) | Literal::TemplateString(_)) => {
                        Some(Type::String)
                    }
                    ExprKind::Array(_) => Some(Type::Array(Box::new(Type::Unknown))),
                    ExprKind::Variable(name) => self
                        .get_allow_moved(name, target.span)
                        .ok()
                        .and_then(|v| non_null(&v.ty)),
                    _ => None,
                };
                let module = match receiver {
                    Some(Type::String) => "string",
                    Some(Type::Array(_)) => "array",
                    _ => return true,
                };
                name != "clone" && !Self::closure_effect_noninvoking_dotted(module, name)
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
            ExprKind::Await(_) => true,
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
            ExprKind::Match { .. } => true,
            ExprKind::Literal(Literal::TemplateString(_)) => true,
            ExprKind::Literal(_) | ExprKind::Variable(_) | ExprKind::Function { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nullable_result_flattens_without_erasing_the_null_arm() {
        assert_eq!(make_nullable(Type::Null), Type::Null);
        assert_eq!(
            make_nullable(Type::Union(vec![Type::Int, Type::Null])),
            Type::Union(vec![Type::Int, Type::Null])
        );
        for payload in [Type::Unknown, Type::Generic("T".into())] {
            assert_eq!(
                make_nullable(payload.clone()),
                Type::Union(vec![payload, Type::Null])
            );
        }
    }
}
