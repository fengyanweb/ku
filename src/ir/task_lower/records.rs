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
    layouts.enums.retain(|layout| {
        layout
            .variants
            .iter()
            .all(|variant| variant.fields.is_empty())
    });
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

/// The mixed service already emits its synchronous layouts. Include only the
/// private Task layouts used by frames, signatures or their nested fields.
pub(super) fn retain_used_layouts(layouts: &mut super::super::IrLayoutTable, tasks: &TaskProgram) {
    fn name(mut ty: &IrType) -> Option<&str> {
        while let IrType::Nullable(inner) | IrType::Result(inner) = ty {
            ty = inner;
        }
        match ty {
            IrType::Named(name) if task::record_type(ty) => Some(name),
            _ => None,
        }
    }

    let mut pending = Vec::new();
    for function in &tasks.functions {
        pending.extend(name(&function.result));
        for slot in &function.slots {
            match &slot.ty {
                TaskSlotType::Value { ty, .. } | TaskSlotType::Task { result: ty } => {
                    pending.extend(name(ty));
                }
                TaskSlotType::Callable { parameters, result } => {
                    pending.extend(parameters.iter().filter_map(name));
                    pending.extend(name(result));
                }
            }
        }
    }
    if pending.is_empty() {
        layouts.structs.clear();
        return;
    }

    let by_name: HashMap<_, _> = layouts
        .structs
        .iter()
        .map(|layout| (layout.name.as_str(), layout))
        .collect();
    let mut used = std::collections::HashSet::new();
    while let Some(record) = pending.pop() {
        if used.insert(record) {
            if let Some(layout) = by_name.get(record) {
                pending.extend(layout.fields.iter().filter_map(|field| name(&field.ty)));
            }
        }
    }
    let keep: Vec<_> = layouts
        .structs
        .iter()
        .map(|layout| used.contains(layout.name.as_str()))
        .collect();
    let mut keep = keep.into_iter();
    layouts.structs.retain(|_| keep.next().unwrap_or(false));
}

pub(super) fn retain_used_enums(layouts: &mut super::super::IrLayoutTable, tasks: &TaskProgram) {
    if layouts.enums.is_empty() {
        return;
    }
    fn name(ty: &IrType) -> Option<&str> {
        task::unit_enum_name(match ty {
            IrType::Result(inner) => inner,
            ty => ty,
        })
    }
    let mut used = std::collections::HashSet::new();
    for layout in &layouts.structs {
        used.extend(layout.fields.iter().filter_map(|field| name(&field.ty)));
    }
    for function in &tasks.functions {
        used.extend(name(&function.result));
        for slot in &function.slots {
            match &slot.ty {
                TaskSlotType::Value { ty, .. } | TaskSlotType::Task { result: ty } => {
                    used.extend(name(ty));
                }
                TaskSlotType::Callable { parameters, result } => {
                    used.extend(parameters.iter().filter_map(name));
                    used.extend(name(result));
                }
            }
        }
    }
    layouts
        .enums
        .retain(|layout| used.contains(layout.name.as_str()));
}

pub(super) fn unit_enum_type(
    ty: &TypeName,
    layouts: &super::super::IrLayoutTable,
) -> Option<IrType> {
    let TypeName::Custom(name) = ty else {
        return None;
    };
    layouts
        .enums
        .iter()
        .any(|decl| &decl.name == name)
        .then(|| super::super::enum_ir_type(name))
}

impl FunctionLowerer<'_> {
    pub(super) fn unit_enum_literal(
        &mut self,
        target: &Expr,
        variant: &str,
        span: Span,
    ) -> KuResult<Option<SlotId>> {
        let ExprKind::Variable(name) = &target.kind else {
            return Ok(None);
        };
        if self.local_binding(name).is_some() {
            return Ok(None);
        }
        let Some(layout) = self
            .layouts
            .enums
            .iter()
            .find(|layout| &layout.name == name)
        else {
            return Ok(None);
        };
        let ty = super::super::enum_ir_type(name);
        let tag = layout
            .variants
            .iter()
            .find(|candidate| candidate.name == variant)
            .and_then(|candidate| i32::try_from(candidate.tag).ok())
            .ok_or_else(|| unsupported("unknown payload-free Task enum alternative", span))?;
        Ok(Some(self.constant(TaskConstant::UnitEnum { tag }, ty)?))
    }

    pub(super) fn unit_enum_equality(
        &mut self,
        left: SlotId,
        op: BinaryOp,
        right: &Expr,
        depth: usize,
    ) -> KuResult<SlotId> {
        let ty = self.function.slots[left.0].ty.clone();
        let left_tag = self.slot(value_slot(IrType::Int))?;
        self.emit(TaskOp::Unary {
            dst: left_tag,
            op: TaskUnaryOp::EnumTag,
            src: left,
        })?;
        let right_value = self.expression(right, depth + 1)?;
        if self.function.slots[right_value.0].ty != ty {
            return Err(unsupported(
                "Task equality between different enum types",
                right.span,
            ));
        }
        let right_tag = self.slot(value_slot(IrType::Int))?;
        self.emit(TaskOp::Unary {
            dst: right_tag,
            op: TaskUnaryOp::EnumTag,
            src: right_value,
        })?;
        let dst = self.slot(value_slot(IrType::Bool))?;
        self.emit(TaskOp::Binary {
            dst,
            op: if op == BinaryOp::Equal {
                TaskBinaryOp::Equal
            } else {
                TaskBinaryOp::NotEqual
            },
            left: left_tag,
            right: right_tag,
        })?;
        Ok(dst)
    }
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
        path.reverse();
        self.record_field_slot(
            source,
            path,
            span,
            false,
            !matches!(base.kind, ExprKind::Variable(_)),
        )
    }

    pub(super) fn temporary_record_projection(
        &mut self,
        source: SlotId,
        fields: &[&str],
        span: Span,
    ) -> KuResult<Option<SlotId>> {
        if self.stable_slots[source.0]
            || self.slot_owners[source.0] != self.scopes.last().copied()
            || !matches!(&self.function.slots[source.0].ty, TaskSlotType::Value { ty, borrowed: false }
                if task::record_type(ty))
        {
            return Ok(None);
        }
        // The current expression owns this anonymous record. Move the selected
        // field before dropping the rest; named owners keep their existing read.
        let path = fields
            .iter()
            .map(|field| (field.to_string(), false))
            .collect();
        self.record_field_slot(source, path, span, true, true)
    }

    fn record_field_slot(
        &mut self,
        source: SlotId,
        path: Vec<(String, bool)>,
        span: Span,
        take: bool,
        drop_source: bool,
    ) -> KuResult<Option<SlotId>> {
        self.budget.analysis(path.len(), span)?;
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
                return self.null_record_projection(source, drop_source);
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
                None if *optional => return self.null_record_projection(source, drop_source),
                None => {
                    return Err(unsupported(
                        &format!("field {name} of {}", public_name(record)),
                        span,
                    ))
                }
            }
        }
        if guarded && task::unit_enum_name(&ty).is_some() {
            return Err(unsupported("nullable Task enum projection", span));
        }
        if guarded {
            ty = super::super::nullable::nullable_type(ty);
        }
        let dst = self.slot(value_slot(ty))?;
        self.emit(TaskOp::RecordField {
            dst,
            src: source,
            path,
            take,
        })?;
        if drop_source {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        Ok(Some(dst))
    }

    fn null_record_projection(
        &mut self,
        source: SlotId,
        drop_source: bool,
    ) -> KuResult<Option<SlotId>> {
        // No field will be read, but the receiver was already evaluated.
        if drop_source && owned_value(&self.function.slots[source.0].ty) {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        Ok(Some(self.constant(TaskConstant::Null, IrType::Null)?))
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
