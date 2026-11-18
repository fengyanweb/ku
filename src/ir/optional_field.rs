use super::*;

impl FunctionLowerer<'_> {
    pub(super) fn lower_optional_field(
        &mut self,
        expression: &Expr,
        target: &Expr,
        name: &str,
    ) -> KuResult<IrExpr> {
        let receiver = self.lower_field_target(target)?;
        if receiver.ty == IrType::Null {
            return Ok(zero_expr(IrType::Null));
        }
        // Calls have already been evaluated. Materialize a fresh literal once
        // as well, so both the tag check and the projection refer to one value.
        let receiver = if ir_expr_is_place(&receiver) || ir_expr_is_borrowed(&receiver) {
            receiver
        } else {
            self.emit_temp(receiver)?
        };
        let nullable = match &receiver.ty {
            IrType::Nullable(inner) => Some((**inner).clone()),
            _ => None,
        };
        let base = match &nullable {
            Some(inner) => {
                intrinsic_expr("__ku_nullable_get", vec![receiver.clone()], inner.clone())
            }
            None => receiver.clone(),
        };
        let field = self.optional_field_projection(base, name, expression.span)?;
        if field.ty == IrType::Null {
            return Ok(field);
        }
        let value = if nullable.is_none() {
            self.emit_temp(field)?
        } else {
            let result_type = if field.ty == IrType::Named("__ku_value".into()) {
                // KuValue has its own real null tag.
                field.ty.clone()
            } else {
                nullable::nullable_type(field.ty.clone())
            };
            // Keep the payload expression inside the conditional. In
            // particular, its owned clone or lookup only runs when present.
            self.emit_temp(intrinsic_expr(
                "__ku_optional_select",
                vec![receiver, field],
                result_type,
            ))?
        };
        if let Some(ty) = self.native_optional_type(expression).filter(|ty| {
            value.ty == IrType::Named("__ku_value".into())
                && !ir_expr_is_borrowed(&value)
                && *ty != value.ty
                && *ty != IrType::Unknown
        }) {
            return self.convert_native_object_value(value, ty);
        }
        Ok(value)
    }

    fn optional_field_projection(&self, base: IrExpr, name: &str, span: Span) -> KuResult<IrExpr> {
        let IrType::Named(kind) = &base.ty else {
            return Err(KuError::runtime(
                format!(
                    "native optional field requires an object or struct, got {}",
                    base.ty
                ),
                span,
            ));
        };
        let key = || IrExpr {
            kind: IrExprKind::Literal(format!("{name:?}")),
            ty: IrType::Str,
        };
        let borrowed = ir_expr_is_borrowed(&base);
        let dynamic = match (kind.as_str(), borrowed) {
            ("__ku_object" | "__ku_http_headers", false) => Some("__ku_object_optional_field"),
            ("__ku_object" | "__ku_http_headers", true) => Some("__ku_object_optional_field_view"),
            ("__ku_value", false) => Some("__ku_value_optional_field"),
            ("__ku_value", true) => Some("__ku_value_optional_field_view"),
            _ => None,
        };
        if let Some(intrinsic) = dynamic {
            return Ok(intrinsic_expr(
                intrinsic,
                vec![base, key()],
                IrType::Named("__ku_value".into()),
            ));
        }
        let ty = self.field_type(&base.ty, name);
        if ty == IrType::Unknown {
            return Ok(zero_expr(IrType::Null));
        }
        if http_constant_object_table(kind).is_some() {
            return Ok(intrinsic_expr(
                "__ku_http_constant_get",
                vec![base, key()],
                IrType::Int,
            ));
        }
        if kind == HTTP_RESPONSE_TYPE && name == "headers" {
            return Ok(intrinsic_expr(
                "__ku_http_response_headers_read",
                vec![base],
                ty,
            ));
        }
        if let Some(field) = self.object_record_field_read(&base, name) {
            return Ok(field);
        }
        let field = IrExpr {
            kind: IrExprKind::Field {
                target: Box::new(base),
                name: name.into(),
            },
            ty: ty.clone(),
        };
        if ir_type_is_owned(&ty) && !borrowed {
            Ok(self.mark_nullable_clone(intrinsic_expr("__ku_clone", vec![field], ty)))
        } else {
            Ok(field)
        }
    }
}
