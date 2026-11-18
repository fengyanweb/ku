use super::*;

pub(super) fn string_like(ty: &TaskSlotType) -> bool {
    matches!(
        ty,
        TaskSlotType::Value {
            ty: IrType::Str,
            ..
        }
    ) || matches!(ty, TaskSlotType::Value { ty:IrType::Nullable(inner),.. } if inner.as_ref()==&IrType::Str)
}

impl FunctionLowerer<'_> {
    pub(super) fn end_nullable_view(&mut self, view: SlotId) -> KuResult<()> {
        if self.nullable_views.remove(&view).is_some()
            && matches!(
                self.function.slots[view.0].ty,
                TaskSlotType::Value { borrowed: true, .. }
            )
        {
            self.emit(TaskOp::Collection {
                dst: view,
                source: Some(view),
                other: None,
                operation: TaskCollectionOperation::NullableViewEnd,
            })?;
        }
        Ok(())
    }
    pub(super) fn nullable_argument_matches(
        &self,
        source: SlotId,
        expected: &TaskSlotType,
    ) -> bool {
        let actual = &self.function.slots[source.0].ty;
        actual == expected
            || self.nullable_views.contains_key(&source)
                && matches!((actual,expected), (TaskSlotType::Value{ty:actual,..},TaskSlotType::Value{ty:expected,borrowed:false}) if actual == expected)
    }
    pub(super) fn nullable_variable_read(&mut self, source: SlotId) -> KuResult<SlotId> {
        let TaskSlotType::Value {
            ty: IrType::Nullable(inner),
            borrowed: false,
        } = &self.function.slots[source.0].ty
        else {
            return Ok(source);
        };
        let payload = *inner.clone();
        match self.nullable_facts.get(&source) {
            Some(true) => {
                let borrowed = !copy_value(&value_slot(payload.clone()));
                let dst = self.slot(TaskSlotType::Value {
                    ty: payload,
                    borrowed,
                })?;
                self.emit(TaskOp::Collection {
                    dst,
                    source: Some(source),
                    other: None,
                    operation: TaskCollectionOperation::NullableView,
                })?;
                self.nullable_views.insert(dst, source);
                Ok(dst)
            }
            Some(false) => self.constant(TaskConstant::Null, IrType::Null),
            None => Ok(source),
        }
    }

    pub(super) fn nullable_coerce(
        &mut self,
        source: SlotId,
        expected: &TaskSlotType,
    ) -> KuResult<SlotId> {
        let TaskSlotType::Value {
            ty: IrType::Nullable(inner),
            borrowed: false,
        } = expected
        else {
            return Ok(source);
        };
        let actual = &self.function.slots[source.0].ty;
        let operation = match actual {
            TaskSlotType::Value {
                ty: IrType::Null, ..
            } => TaskCollectionOperation::NullableNull,
            TaskSlotType::Value { ty, .. } if ty == inner.as_ref() => {
                TaskCollectionOperation::NullableSome
            }
            _ => return Ok(source),
        };
        let input = if operation == TaskCollectionOperation::NullableNull {
            None
        } else {
            Some(self.copy_or_move(source)?)
        };
        let dst = self.slot(expected.clone())?;
        self.emit(TaskOp::Collection {
            dst,
            source: input,
            other: None,
            operation,
        })?;
        Ok(dst)
    }

    pub(super) fn nullable_condition(&mut self, expr: &Expr, truth: bool) {
        match &expr.kind {
            ExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } => self.nullable_condition(expr, !truth),
            ExprKind::Binary {
                left,
                op: BinaryOp::And,
                right,
            } if truth => {
                self.nullable_condition(left, true);
                self.nullable_condition(right, true);
            }
            ExprKind::Binary {
                left,
                op: BinaryOp::Or,
                right,
            } if !truth => {
                self.nullable_condition(left, false);
                self.nullable_condition(right, false);
            }
            ExprKind::Binary { left, op, right }
                if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
            {
                let name = match (&left.kind, &right.kind) {
                    (ExprKind::Variable(name), ExprKind::Literal(Literal::Null))
                    | (ExprKind::Literal(Literal::Null), ExprKind::Variable(name)) => name,
                    _ => return,
                };
                if let Some(slot) = self.local(name) {
                    if matches!(
                        self.function.slots[slot.0].ty,
                        TaskSlotType::Value {
                            ty: IrType::Nullable(_),
                            ..
                        }
                    ) {
                        self.nullable_facts
                            .insert(slot, truth == (*op == BinaryOp::NotEqual));
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn nullable_null_comparison(
        &mut self,
        left: &Expr,
        op: BinaryOp,
        right: &Expr,
        depth: usize,
    ) -> KuResult<Option<SlotId>> {
        if !matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) {
            return Ok(None);
        }
        let tested = if matches!(right.kind, ExprKind::Literal(Literal::Null)) {
            left
        } else if matches!(left.kind, ExprKind::Literal(Literal::Null)) {
            right
        } else {
            return Ok(None);
        };
        let source = match &tested.kind {
            ExprKind::Variable(name) => self
                .local(name)
                .map(Ok)
                .unwrap_or_else(|| self.expression(tested, depth + 1)),
            _ => self.expression(tested, depth + 1),
        }?;
        if !matches!(
            self.function.slots[source.0].ty,
            TaskSlotType::Value {
                ty: IrType::Nullable(_),
                ..
            }
        ) {
            return self
                .constant(
                    TaskConstant::Bool(
                        matches!(
                            self.function.slots[source.0].ty,
                            TaskSlotType::Value {
                                ty: IrType::Null,
                                ..
                            }
                        ) == (op == BinaryOp::Equal),
                    ),
                    IrType::Bool,
                )
                .map(Some);
        }
        let dst = self.slot(value_slot(IrType::Bool))?;
        self.emit(TaskOp::Collection {
            dst,
            source: Some(source),
            other: None,
            operation: TaskCollectionOperation::NullableIsNull,
        })?;
        if op == BinaryOp::Equal {
            return Ok(Some(dst));
        }
        let inverse = self.slot(value_slot(IrType::Bool))?;
        self.emit(TaskOp::Unary {
            dst: inverse,
            src: dst,
            op: TaskUnaryOp::Not,
        })?;
        Ok(Some(inverse))
    }

    pub(super) fn array_collection_call(
        &mut self,
        object: &Expr,
        name: &str,
        args: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<Option<SlotId>> {
        if !matches!(name, "concat" | "first" | "last" | "clone") {
            return Ok(None);
        }
        let module = matches!(&object.kind, ExprKind::Variable(module) if module == "array" && self.local_binding(module).is_none());
        let (input, extra) = if module {
            let Some((input, extra)) = args.split_first() else {
                return Err(unsupported("array arguments", span));
            };
            (input, extra)
        } else {
            (object, args)
        };
        if (name == "concat" && extra.len() != 1) || (name != "concat" && !extra.is_empty()) {
            return Err(unsupported("array collection argument count", span));
        }
        if name == "clone" && module {
            return Ok(None);
        }
        let known = match &input.kind {
            ExprKind::Variable(name) => self.local(name),
            _ => None,
        };
        if name == "clone" {
            if let Some(source) = known {
                match self.nullable_facts.get(&source) {
                    Some(true) => return Ok(None),
                    Some(false) => {
                        return self.constant(TaskConstant::Null, IrType::Null).map(Some)
                    }
                    None => {}
                }
            }
        }
        if name == "clone"
            && !known.is_some_and(|slot| {
                matches!(
                    self.function.slots[slot.0].ty,
                    TaskSlotType::Value {
                        ty: IrType::Nullable(_),
                        ..
                    }
                )
            })
        {
            return Ok(None);
        }
        let source = if name == "clone" {
            known.unwrap()
        } else {
            self.expression(input, depth + 1)?
        };
        let ty = match &self.function.slots[source.0].ty {
            TaskSlotType::Value {
                ty,
                borrowed: false,
            } => ty.clone(),
            _ => return Err(unsupported("array collection receiver type", span)),
        };
        let (operation, result) = match (&ty, name) {
            (IrType::Array(_), "concat") if task::flat_array(&ty) => {
                (TaskCollectionOperation::Concat, ty.clone())
            }
            (IrType::Array(inner), "first" | "last") if task::flat_array(&ty) => (
                if name == "first" {
                    TaskCollectionOperation::First
                } else {
                    TaskCollectionOperation::Last
                },
                crate::ir::nullable::nullable_type(*inner.clone()),
            ),
            (IrType::Nullable(_), "clone") => (TaskCollectionOperation::NullableClone, ty.clone()),
            _ => return Err(unsupported("array collection element type", span)),
        };
        let other = if operation == TaskCollectionOperation::Concat {
            let snapshot = if matches!(input.kind, ExprKind::Variable(_))
                && !crate::ast::is_pure_append_argument(&extra[0], "")
            {
                let snapshot = self.slot(value_slot(ty.clone()))?;
                self.emit(TaskOp::ArrayClone {
                    dst: snapshot,
                    src: source,
                })?;
                snapshot
            } else {
                source
            };
            let right = self.expression(&extra[0], depth + 1)?;
            if self.function.slots[right.0].ty != value_slot(ty.clone()) {
                return Err(unsupported("concat with different array types", span));
            }
            Some((snapshot, right))
        } else {
            None
        };
        let dst = self.slot(value_slot(result))?;
        let left = other.map_or(source, |(left, _)| left);
        self.emit(TaskOp::Collection {
            dst,
            source: Some(left),
            other: other.map(|(_, right)| right),
            operation,
        })?;
        if left != source {
            self.emit(TaskOp::Drop { slot: left })?;
        }
        if !matches!(input.kind, ExprKind::Variable(_)) {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        if let Some((_, right)) = other {
            if !matches!(extra[0].kind, ExprKind::Variable(_)) {
                self.emit(TaskOp::Drop { slot: right })?;
            }
        }
        Ok(Some(dst))
    }
}
