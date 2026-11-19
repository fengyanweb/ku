//! Explicitly bounded layouts reused by the native Task frame and adapter.
use super::*;

pub(super) fn record_name(name: &str) -> String {
    format!("__ku_task_record_{name}")
}

fn remap(ty: &mut IrType, names: &std::collections::HashSet<String>) {
    match ty {
        IrType::Named(name) if names.contains(name) => *name = record_name(name),
        IrType::Nullable(inner) => remap(inner, names),
        _ => {}
    }
}

pub(super) fn layouts(program: &Program) -> super::super::IrLayoutTable {
    let mut layouts = super::super::lower_layouts(program);
    let names = layouts
        .structs
        .iter()
        .map(|layout| layout.name.clone())
        .collect();
    layouts.enums.clear();
    for layout in &mut layouts.structs {
        layout.name = record_name(&layout.name);
        for field in &mut layout.fields {
            remap(&mut field.ty, &names);
        }
    }
    // An unused unsupported declaration does not invalidate an otherwise
    // supported Task program. Referencing it yields a type-specific diagnostic.
    let supported: Vec<_> = layouts
        .structs
        .iter()
        .map(|layout| task::record_layout_supported(&layouts, &layout.name))
        .collect();
    let mut supported = supported.into_iter();
    layouts
        .structs
        .retain(|_| supported.next().unwrap_or(false));
    layouts
}

pub(super) fn payload(ty: &TypeName, layouts: &super::super::IrLayoutTable) -> Option<IrType> {
    if let TypeName::Custom(name) = ty {
        let name = record_name(name);
        if layouts.structs.iter().any(|layout| layout.name == name) {
            return Some(IrType::Named(name));
        }
    }
    if let TypeName::Union(parts) = ty {
        if parts.len() == 2 && parts.iter().any(|part| matches!(part, TypeName::Null)) {
            let inner = parts.iter().find(|part| !matches!(part, TypeName::Null))?;
            return Some(super::super::nullable::nullable_type(payload(
                inner, layouts,
            )?));
        }
    }
    result_payload(ty)
}

impl FunctionLowerer<'_> {
    pub(super) fn record_literal(
        &mut self,
        name: &str,
        fields: &[(String, Expr)],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        let name = record_name(name);
        let layout = self
            .layouts
            .structs
            .iter()
            .find(|layout| layout.name == name)
            .ok_or_else(|| unsupported(&format!("record layout {}", public_name(&name)), span))?;
        let fields_in_layout = layout.fields.clone();
        let mut sources = vec![None; fields_in_layout.len()];
        for (name, value) in fields {
            self.budget.text(name.len(), value.span)?;
            let field = fields_in_layout
                .iter()
                .find(|field| field.name == *name)
                .ok_or_else(|| unsupported(&format!("unknown record field {name}"), value.span))?;
            if sources[field.offset].is_some() {
                return Err(unsupported("duplicate Task record initializer", value.span));
            }
            let value_slot_id = self.expression(value, depth + 1)?;
            let expected = value_slot(field.ty.clone());
            let source = self.nullable_coerce(value_slot_id, &expected)?;
            if !self.nullable_argument_matches(source, &expected) {
                return Err(unsupported(
                    &format!("initializer for record field {name}"),
                    value.span,
                ));
            }
            // Snapshot this field now, before evaluation of the next initializer.
            sources[field.offset] = Some(self.copy_or_move(source)?);
        }
        let elements = sources
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| unsupported("missing Task record initializer", span))?;
        let dst = self.slot(value_slot(IrType::Named(name)))?;
        self.emit(TaskOp::RecordMake { dst, elements })?;
        Ok(dst)
    }

    pub(super) fn record_field_expression(
        &mut self,
        target: &Expr,
        name: &str,
        optional: bool,
        span: Span,
        depth: usize,
    ) -> KuResult<Option<SlotId>> {
        let mut path = vec![(name.to_owned(), optional)];
        let mut base = target;
        while let ExprKind::Field { target, name } | ExprKind::OptionalField { target, name } =
            &base.kind
        {
            if path.len() == 8 {
                return Err(unsupported(
                    "Task record field paths deeper than 8 fields",
                    span,
                ));
            }
            path.push((
                name.clone(),
                matches!(base.kind, ExprKind::OptionalField { .. }),
            ));
            base = target;
        }
        let source = match &base.kind {
            ExprKind::Variable(name) => {
                let Some(slot) = self.local(name) else {
                    return Ok(None);
                };
                if !matches!(&self.function.slots[slot.0].ty, TaskSlotType::Value { ty, .. }
                    if task::record_type(ty) || task::nullable_record(ty))
                {
                    return Ok(None);
                }
                slot
            }
            ExprKind::StructLiteral { .. } => self.expression(base, depth + path.len())?,
            _ if optional => self.expression(base, depth + path.len())?,
            _ => return Ok(None),
        };
        self.budget.analysis(path.len(), span)?;
        path.reverse();
        let mut ty = match &self.function.slots[source.0].ty {
            TaskSlotType::Value { ty, .. } => ty.clone(),
            _ => return Ok(None),
        };
        let mut guarded = false;
        for (name, optional) in &path {
            if let IrType::Nullable(inner) = ty {
                if !optional {
                    return Err(unsupported("a nullable record projection without ?.", span));
                }
                guarded = true;
                ty = *inner;
            }
            if ty == IrType::Null && *optional {
                return Ok(Some(self.constant(TaskConstant::Null, IrType::Null)?));
            }
            let IrType::Named(ref record) = ty else {
                return Err(unsupported(
                    "field path through a non-record Task value",
                    span,
                ));
            };
            let field = self
                .layouts
                .structs
                .iter()
                .find(|layout| layout.name == *record)
                .and_then(|layout| layout.fields.iter().find(|field| field.name == *name));
            match field {
                Some(field) => ty = field.ty.clone(),
                None if *optional => {
                    return Ok(Some(self.constant(TaskConstant::Null, IrType::Null)?))
                }
                None => {
                    return Err(unsupported(
                        &format!("field {name} of {}", public_name(record)),
                        span,
                    ))
                }
            }
        }
        if guarded {
            ty = super::super::nullable::nullable_type(ty);
        }
        let dst = self.slot(value_slot(ty))?;
        self.emit(TaskOp::RecordField {
            dst,
            src: source,
            path,
            take: false,
        })?;
        if !matches!(base.kind, ExprKind::Variable(_)) {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        Ok(Some(dst))
    }

    pub(super) fn record_pattern(
        &mut self,
        bindings: &[crate::ast::ObjectDestructureBinding],
        rest: Option<&crate::ast::ObjectDestructureRest>,
        value: &Expr,
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        if bindings.len() > 64 {
            return Err(unsupported(
                "more than 64 record destructure bindings",
                span,
            ));
        }
        self.budget.analysis(bindings.len(), span)?;
        let source = self.expression(value, depth + 1)?;
        let TaskSlotType::Value {
            ty: IrType::Named(name),
            borrowed: false,
        } = &self.function.slots[source.0].ty
        else {
            return Err(unsupported(
                "object pattern source without a named Task layout",
                span,
            ));
        };
        let fields = self
            .layouts
            .structs
            .iter()
            .find(|layout| layout.name == *name)
            .ok_or_else(|| unsupported("object pattern source without a named Task layout", span))?
            .fields
            .clone();
        let source = self.copy_or_move(source)?;
        let mut selected = std::collections::HashSet::new();
        for binding in bindings {
            let name = &binding.field;
            if !selected.insert(name.as_str()) {
                return Err(unsupported(
                    "duplicate field in Task record pattern",
                    binding.span,
                ));
            }
            let dst = if let Some(field) = fields.iter().find(|field| field.name == *name) {
                if binding.local.is_none() && copy_value(&value_slot(field.ty.clone())) {
                    continue;
                }
                let dst = self.slot(value_slot(field.ty.clone()))?;
                self.emit(TaskOp::RecordField {
                    dst,
                    src: source,
                    path: vec![(name.clone(), false)],
                    take: true,
                })?;
                dst
            } else if let Some(default) = &binding.default {
                let dst = self.expression(default, depth + 1)?;
                self.copy_or_move(dst)?
            } else {
                return Err(unsupported(
                    &format!("missing record field {name} without a default"),
                    binding.span,
                ));
            };
            if let Some(local) = &binding.local {
                self.record_pattern_bind(local, dst, binding.span)?;
            } else if owned_value(&self.function.slots[dst.0].ty) {
                self.emit(TaskOp::Drop { slot: dst })?;
            }
        }
        if let Some(rest) = rest {
            if let Some(local) = &rest.local {
                let remaining = fields
                    .into_iter()
                    .filter(|field| !selected.contains(field.name.as_str()))
                    .collect::<Vec<_>>();
                let dst = self.record_rest(source, remaining)?;
                self.record_pattern_bind(local, dst, rest.span)?;
            }
        }
        self.emit(TaskOp::Drop { slot: source })
    }

    fn record_rest(
        &mut self,
        source: SlotId,
        mut fields: Vec<super::super::IrFieldLayout>,
    ) -> KuResult<SlotId> {
        let nested = fields
            .iter()
            .any(|field| task::record_type(&field.ty) || task::nullable_record(&field.ty));
        let mut elements = Vec::with_capacity(fields.len());
        for field in &fields {
            let dst = self.slot(value_slot(field.ty.clone()))?;
            self.emit(TaskOp::RecordField {
                dst,
                src: source,
                path: vec![(field.name.clone(), false)],
                take: true,
            })?;
            elements.push(dst);
        }
        if nested {
            // A record subset preserves the checked source's bounds. Keep the
            // fields typed so owned payloads move into the frame's new owner.
            // A digit after the prefix cannot start a source struct name.
            let name = format!("__ku_task_record_{}_rest", self.layouts.structs.len());
            for (offset, field) in fields.iter_mut().enumerate() {
                field.offset = offset;
            }
            self.layouts.structs.push(super::super::IrStructLayout {
                name: name.clone(),
                fields,
            });
            let dst = self.slot(value_slot(IrType::Named(name)))?;
            self.emit(TaskOp::RecordMake { dst, elements })?;
            Ok(dst)
        } else {
            let dst = self.slot(value_slot(IrType::Named("__ku_value".into())))?;
            self.emit(TaskOp::JsonObject {
                dst,
                fields: fields
                    .into_iter()
                    .zip(elements)
                    .map(|(field, element)| (field.name, element))
                    .collect(),
            })?;
            Ok(dst)
        }
    }

    fn record_pattern_bind(&mut self, name: &str, dst: SlotId, span: Span) -> KuResult<()> {
        self.budget.text(name.len(), span)?;
        if self.local(name).is_some() {
            return Err(unsupported(
                "reassignment through a native Task record pattern",
                span,
            ));
        }
        self.stable_slots[dst.0] = true;
        self.locals
            .last_mut()
            .expect("root Task scope")
            .insert(name.to_owned(), dst);
        Ok(())
    }
}

fn public_name(name: &str) -> &str {
    name.strip_prefix("__ku_task_record_").unwrap_or(name)
}
