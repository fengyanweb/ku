use super::*;
use crate::ast::{ObjectDestructureBinding, ObjectDestructureRest};

fn tagged_type() -> IrType {
    IrType::Named("__ku_value".into())
}

impl FunctionLowerer<'_> {
    pub(super) fn native_optional_type(&self, expr: &Expr) -> Option<IrType> {
        self.native_fields
            .optional
            .get(&(std::ptr::from_ref(expr) as usize))
            .map(|ty| lower_type(ty, self.layouts))
    }

    pub(super) fn convert_native_object_value(
        &mut self,
        value: IrExpr,
        ty: IrType,
    ) -> KuResult<IrExpr> {
        if ty == tagged_type() {
            return Ok(value);
        }
        self.emit_temp(intrinsic_expr(
            "__ku_object_pattern_convert",
            vec![value],
            ty,
        ))
    }

    pub(super) fn lower_object_pattern(
        &mut self,
        bindings: &[ObjectDestructureBinding],
        rest: Option<&ObjectDestructureRest>,
        value: &Expr,
        span: Span,
    ) -> KuResult<()> {
        let layouts = self.layouts;
        let value = if matches!(self.static_place_type(value), Some(IrType::Named(ref name))
            if layouts.structs.iter().any(|layout| layout.name == *name))
        {
            // This is a consuming pattern, so a nested named field can move
            // from its place instead of cloning as a value-position read.
            self.lower_field_target(value)?
        } else {
            self.lower_expr(value)?
        };
        let named_layout = match &value.ty {
            IrType::Named(name) if !ir_expr_is_borrowed(&value) => {
                layouts.structs.iter().find(|layout| layout.name == *name)
            }
            _ => None,
        };
        if let Some(layout) = named_layout {
            return self.lower_named_object_pattern(bindings, rest, value, &layout.fields);
        }
        if !matches!(&value.ty, IrType::Named(name)
            if matches!(name.as_str(), "__ku_object" | "__ku_http_client_config" | "__ku_http_headers")
                || http_constant_object_table(name).is_some())
            || ir_expr_is_borrowed(&value)
        {
            return Err(KuError::runtime(
                "native object destructuring requires an owned object",
                span,
            ));
        }
        let source = self.emit_temp(value)?;
        let pending_start = self.pending_borrow_temporaries.len();
        self.pending_borrow_temporaries
            .push(PendingBorrowTemporary {
                owner: source.clone(),
                error_block: self.try_handlers.last().map(|handler| handler.error_block),
            });

        for binding in bindings {
            let key = IrExpr {
                kind: IrExprKind::Literal(format!("{:?}", binding.field)),
                ty: IrType::Str,
            };
            let take = intrinsic_expr(
                "__ku_object_take_required",
                vec![source.clone(), key.clone()],
                tagged_type(),
            );
            let ty = self
                .native_fields
                .destructured
                .get(&(std::ptr::from_ref(binding) as usize))
                .map_or_else(tagged_type, |ty| lower_type(ty, self.layouts));
            let field = if let Some(default) = &binding.default {
                self.object_pattern_default(&source, key, take, default)?
            } else {
                self.emit_temp(take)?
            };
            if let Some(local) = &binding.local {
                let value = self.convert_native_object_value(field, ty)?;
                self.store_or_define_name(
                    local,
                    value,
                    binding.span,
                    self.binding_is_boxed(local, binding.span),
                )?;
            } else {
                self.emit_borrow_temporary_drop(field)?;
            }
        }
        self.pending_borrow_temporaries.truncate(pending_start);
        if let Some((rest, local)) =
            rest.and_then(|rest| rest.local.as_ref().map(|name| (rest, name)))
        {
            // Drop/move operations for object pointers are identical for the
            // specialized object markers and the ordinary map type.
            let source = IrExpr {
                ty: IrType::Named("__ku_object".into()),
                ..source
            };
            self.store_or_define_name(
                local,
                source,
                rest.span,
                self.binding_is_boxed(local, rest.span),
            )
        } else {
            self.emit_borrow_temporary_drop(source)
        }
    }

    fn lower_named_object_pattern(
        &mut self,
        bindings: &[ObjectDestructureBinding],
        rest: Option<&ObjectDestructureRest>,
        value: IrExpr,
        fields: &[IrFieldLayout],
    ) -> KuResult<()> {
        let source = self.emit_temp(value)?;
        let pending_start = self.pending_borrow_temporaries.len();
        self.pending_borrow_temporaries
            .push(PendingBorrowTemporary {
                owner: source.clone(),
                error_block: self.try_handlers.last().map(|handler| handler.error_block),
            });
        // The pattern consumes the record. Project fields from the owned
        // temporary so the backend moves owned payloads and clears their slots.
        let projection = |field: &IrFieldLayout| IrExpr {
            ty: field.ty.clone(),
            kind: IrExprKind::Field {
                target: Box::new(source.clone()),
                name: field.name.clone(),
            },
        };
        let mut remaining = fields
            .iter()
            .map(|field| (field.name.as_str(), field))
            .collect::<HashMap<_, _>>();
        for binding in bindings {
            let value = if let Some(field) = remaining.remove(binding.field.as_str()) {
                projection(field)
            } else if let Some(default) = &binding.default {
                self.lower_expr(default)?
            } else {
                return Err(KuError::runtime(
                    format!("object has no field '{}'", binding.field),
                    binding.span,
                ));
            };
            if let Some(local) = &binding.local {
                self.store_or_define_name(
                    local,
                    value,
                    binding.span,
                    self.binding_is_boxed(local, binding.span),
                )?;
            } else if ir_type_is_owned(&value.ty) {
                let temporary = self.emit_temp(value)?;
                self.emit_borrow_temporary_drop(temporary)?;
            } else if value.ty == IrType::Void {
                emit_ir_instruction!(self, IrInst::Expr(value));
            }
        }
        if let Some((rest, local)) =
            rest.and_then(|rest| rest.local.as_ref().map(|name| (rest, name)))
        {
            let mut args = Vec::new();
            for field in fields
                .iter()
                .filter(|field| remaining.contains_key(field.name.as_str()))
            {
                args.push(IrExpr {
                    kind: IrExprKind::Literal(format!("{:?}", field.name)),
                    ty: IrType::Str,
                });
                args.push(projection(field));
            }
            let rest_value = self.emit_temp(intrinsic_expr(
                "__ku_object",
                args,
                IrType::Named("__ku_object".into()),
            ))?;
            self.store_or_define_name(
                local,
                rest_value,
                rest.span,
                self.binding_is_boxed(local, rest.span),
            )?;
        }
        self.pending_borrow_temporaries.truncate(pending_start);
        self.emit_borrow_temporary_drop(source)
    }

    fn object_pattern_default(
        &mut self,
        source: &IrExpr,
        key: IrExpr,
        take: IrExpr,
        default: &Expr,
    ) -> KuResult<IrExpr> {
        let name = format!("__ku_object_pattern_{}", self.next_temp_id);
        self.next_temp_id += 1;
        let ty = tagged_type();
        self.locals.insert(name.clone(), ty.clone());
        emit_ir_instruction!(
            self,
            IrInst::Let {
                name: name.clone(),
                ty: ty.clone(),
                value: intrinsic_expr(
                    "__ku_object_pattern_box",
                    vec![IrExpr {
                        kind: IrExprKind::Literal("null".into()),
                        ty: IrType::Null,
                    }],
                    ty.clone()
                ),
            }
        );
        let present = self.next_block("object_key_present")?;
        let missing = self.next_block("object_key_missing")?;
        let join = self.next_block("object_key_ready")?;
        self.current.terminator = IrTerminator::Branch {
            condition: intrinsic_expr(
                "__ku_object_has_key",
                vec![source.clone(), key],
                IrType::Bool,
            ),
            then_block: present,
            else_block: missing,
        };
        self.finish_current()?;

        self.start_block(present, "object_key_present");
        let value = self.emit_temp(take)?;
        emit_ir_instruction!(
            self,
            IrInst::Store {
                target: IrLValue::Local(name.clone()),
                value
            }
        );
        self.current.terminator = IrTerminator::Jump(join);
        self.finish_current()?;

        self.start_block(missing, "object_key_missing");
        let value = self.lower_expr(default)?;
        let value = self.emit_temp(intrinsic_expr(
            "__ku_object_pattern_box",
            vec![value],
            ty.clone(),
        ))?;
        emit_ir_instruction!(
            self,
            IrInst::Store {
                target: IrLValue::Local(name.clone()),
                value
            }
        );
        self.current.terminator = IrTerminator::Jump(join);
        self.finish_current()?;

        self.start_block(join, "object_key_ready");
        Ok(IrExpr {
            kind: IrExprKind::Local(name),
            ty,
        })
    }
}
