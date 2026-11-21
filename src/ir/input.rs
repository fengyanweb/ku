//! Lower builtin input through the existing typed Result and owner paths.

use super::*;
use crate::ast::input::{self as input_syntax, Field, Template};

pub(crate) const READ_LINE: &str = "__ku_input";
pub(crate) const READ_TEMPLATE: &str = "__ku_input_template";
const FIELDS_PREFIX: &str = "__ku_input_fields_";

fn field_type(field: &Field) -> IrType {
    match field {
        Field::Text => IrType::Str,
        Field::Int => IrType::Int,
    }
}

fn payload_type(fields: &[Field]) -> IrType {
    if let [field] = fields {
        return field_type(field);
    }
    let mut name = String::from(FIELDS_PREFIX);
    name.extend(fields.iter().map(|field| match field {
        Field::Text => 's',
        Field::Int => 'i',
    }));
    IrType::Named(name)
}

pub(crate) fn field_name(index: usize) -> String {
    format!("field_{index}")
}

/// The layout name encodes only compiler-validated types, never input data.
/// One internal struct per shape lets the native backend reuse named ownership.
pub(crate) fn template_field_types(payload: &IrType) -> Option<Vec<IrType>> {
    match payload {
        IrType::Str | IrType::Int => Some(vec![payload.clone()]),
        IrType::Named(name) => name
            .strip_prefix(FIELDS_PREFIX)
            .filter(|shape| shape.len() >= 2)?
            .bytes()
            .map(|kind| match kind {
                b's' => Some(IrType::Str),
                b'i' => Some(IrType::Int),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

pub(super) fn register_layouts(functions: &[IrFunction], layouts: &mut IrLayoutTable) {
    let mut seen = HashSet::new();
    for inst in functions
        .iter()
        .flat_map(|function| &function.blocks)
        .flat_map(|block| &block.instructions)
    {
        let IrInst::Temp { value, .. } = inst else {
            continue;
        };
        let (
            IrExprKind::Call {
                kind: IrCallKind::Intrinsic(intrinsic),
                ..
            },
            IrType::Result(inner),
        ) = (&value.kind, &value.ty)
        else {
            continue;
        };
        let IrType::Named(name) = inner.as_ref() else {
            continue;
        };
        if intrinsic != READ_TEMPLATE || !seen.insert(name) {
            continue;
        }
        if let Some(fields) = template_field_types(inner) {
            layouts.structs.push(IrStructLayout {
                name: name.clone(),
                fields: fields
                    .into_iter()
                    .enumerate()
                    .map(|(offset, ty)| IrFieldLayout {
                        name: field_name(offset),
                        ty,
                        offset,
                        is_async_callable: false,
                    })
                    .collect(),
            });
        }
    }
}

impl FunctionLowerer<'_> {
    pub(super) fn is_builtin_input(&self, callee: &Expr) -> bool {
        matches!(&callee.kind, ExprKind::Variable(name) if name == "input")
            && !self.signatures.contains_key("input")
            && self.self_recurse_target("input").is_none()
            && self.static_place_type(callee).is_none()
    }

    pub(super) fn lower_input_call(
        &mut self,
        args: &[Expr],
        span: Span,
        deferred_safepoint: Option<&mut bool>,
    ) -> KuResult<IrExpr> {
        match args {
            [] => {}
            [arg] => {
                if let Some(template) = input_syntax::template(arg)? {
                    if template.fields.len() != 1 {
                        return Err(KuError::runtime(
                            "input with multiple fields must be destructured with ?",
                            span,
                        ));
                    }
                    return self.lower_input_template(template, span);
                }
            }
            _ => return Err(KuError::runtime("input accepts at most one prompt", span)),
        }

        let first_argument_temp = self.next_temp_id;
        let args = self.lower_call_arguments(
            args,
            Some(&[IrType::Str]),
            Some(&[ParamMode::View]),
            false,
            false,
            false,
        )?;
        self.finish_borrowing_call(
            intrinsic_expr(READ_LINE, args, IrType::Result(Box::new(IrType::Str))),
            first_argument_temp,
            deferred_safepoint,
        )
    }

    pub(super) fn lower_input_assignment(
        &mut self,
        names: &[Option<String>],
        values: &[Expr],
        span: Span,
    ) -> KuResult<bool> {
        let Some(call) = input_syntax::destructure_call(values) else {
            return Ok(false);
        };
        let ExprKind::Call { callee, args } = &call.kind else {
            return Ok(false);
        };
        let [arg] = args.as_slice() else {
            return Ok(false);
        };
        if !self.is_builtin_input(callee) {
            return Ok(false);
        }
        let Some(template) = input_syntax::template(arg)? else {
            return Ok(false);
        };
        if template.fields.len() == 1 && names.len() == 1 {
            return Ok(false);
        }
        if names.len() != template.fields.len() {
            return Err(KuError::runtime(
                format!(
                    "input has {} fields but assignment has {} targets",
                    template.fields.len(),
                    names.len()
                ),
                span,
            ));
        }

        let result = self.lower_input_template(template, span)?;
        // The normal `?` CFG establishes success before any existing local is
        // replaced. Field stores move owned strings out of the temporary.
        let carrier = self.emit_try_unwrap(result, payload_type(&template.fields))?;
        for (index, (name, field)) in names.iter().zip(&template.fields).enumerate() {
            if let Some(name) = name {
                let value = IrExpr {
                    kind: IrExprKind::Field {
                        target: Box::new(carrier.clone()),
                        name: field_name(index),
                    },
                    ty: field_type(field),
                };
                let boxed = self.binding_is_boxed(name, span);
                self.store_or_define_name(name, value, span, boxed)?;
            }
        }
        // Discarded slots still belong to the carrier after the moves.
        self.emit_borrow_temporary_drop(carrier)?;
        Ok(true)
    }

    fn lower_input_template(&mut self, template: &Template, span: Span) -> KuResult<IrExpr> {
        self.budget
            .borrow_mut()
            .spend(template.segments.len(), span)?;
        let segments = template
            .segments
            .iter()
            .map(|segment| IrExpr {
                kind: IrExprKind::Literal(format!("{segment:?}")),
                ty: IrType::Str,
            })
            .collect();
        self.emit_temp(intrinsic_expr(
            READ_TEMPLATE,
            segments,
            IrType::Result(Box::new(payload_type(&template.fields))),
        ))
    }
}
