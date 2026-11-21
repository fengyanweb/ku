//! Explicitly bounded layouts reused by the native Task frame and adapter.
use super::*;
use crate::ast::{MatchArm, MatchPattern};

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
    layouts.enums.retain(task::enum_variant_layout_supported);
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
        task::enum_name(match ty {
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

pub(super) fn enum_type(ty: &TypeName, layouts: &super::super::IrLayoutTable) -> Option<IrType> {
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
    pub(super) fn enum_literal(
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
            .find(|candidate| candidate.name == variant && candidate.fields.is_empty())
            .and_then(|candidate| i32::try_from(candidate.tag).ok())
            .ok_or_else(|| unsupported("unknown payload-free Task enum alternative", span))?;
        Ok(Some(self.constant(TaskConstant::UnitEnum { tag }, ty)?))
    }

    pub(super) fn int_enum_constructor(
        &mut self,
        target: &Expr,
        name: &str,
        arguments: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<Option<SlotId>> {
        let ExprKind::Variable(enum_name) = &target.kind else {
            return Ok(None);
        };
        if self.local_binding(enum_name).is_some() {
            return Ok(None);
        }
        let Some(variant) = self
            .layouts
            .enums
            .iter()
            .find(|layout| &layout.name == enum_name)
            .and_then(task::enum_int_variant)
            .filter(|variant| variant.name == name)
        else {
            return Ok(None);
        };
        let [argument] = arguments else {
            return Err(unsupported(
                "Task enum constructor arity other than one",
                span,
            ));
        };
        let tag = i32::try_from(variant.tag)
            .map_err(|_| unsupported("Task enum constructor tag out of range", span))?;
        let src = self.expression(argument, depth + 1)?;
        if self.function.slots[src.0].ty != value_slot(IrType::Int) {
            return Err(unsupported("non-int Task enum payload", argument.span));
        }
        let dst = self.slot(value_slot(super::super::enum_ir_type(enum_name)))?;
        self.emit(TaskOp::Unary {
            dst,
            op: TaskUnaryOp::EnumFromInt { tag },
            src,
        })?;
        Ok(Some(dst))
    }

    pub(super) fn unit_enum_equality(
        &mut self,
        left: SlotId,
        op: BinaryOp,
        right: &Expr,
        depth: usize,
    ) -> KuResult<SlotId> {
        let ty = self.function.slots[left.0].ty.clone();
        if !matches!(&ty, TaskSlotType::Value { ty, .. }
            if task::unit_enum_layout(self.layouts, ty).is_some())
        {
            return Err(unsupported(
                "Task equality on a payload-bearing enum",
                right.span,
            ));
        }
        let left_tag = self.unit_enum_tag(left)?;
        let right_value = self.expression(right, depth + 1)?;
        if self.function.slots[right_value.0].ty != ty {
            return Err(unsupported(
                "Task equality between different enum types",
                right.span,
            ));
        }
        let right_tag = self.unit_enum_tag(right_value)?;
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

    fn unit_enum_tag(&mut self, source: SlotId) -> KuResult<SlotId> {
        let tag = self.slot(value_slot(IrType::Int))?;
        self.emit(TaskOp::Unary {
            dst: tag,
            op: TaskUnaryOp::EnumTag,
            src: source,
        })?;
        Ok(tag)
    }

    #[inline(never)]
    pub(super) fn enum_match(
        &mut self,
        value: &Expr,
        arms: &[MatchArm],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        let source = self.expression(value, depth + 1)?;
        let TaskSlotType::Value {
            ty,
            borrowed: false,
        } = &self.function.slots[source.0].ty
        else {
            return Err(unsupported(
                "match without an owned Task enum subject",
                value.span,
            ));
        };
        let layout = task::enum_layout(self.layouts, ty).ok_or_else(|| {
            unsupported(
                "match on a type other than a supported Task enum",
                value.span,
            )
        })?;
        self.budget.analysis(layout.variants.len(), span)?;
        self.budget.analysis(arms.len(), span)?;
        let int_variant = task::enum_int_variant(layout).map(|variant| variant.tag);
        if int_variant.is_some() && arms.iter().any(|arm| arm.guard.is_some()) {
            return Err(unsupported(
                "a guard in a Task match on a payload-bearing enum",
                span,
            ));
        }
        let variants: HashMap<_, _> = layout
            .variants
            .iter()
            .map(|v| (v.name.as_str(), v.tag))
            .collect();
        let variant_count = variants.len();
        let mut covered = std::collections::HashSet::new();
        let mut tags = Vec::with_capacity(arms.len());
        let mut payload_binding = None;
        for (index, arm) in arms.iter().enumerate() {
            let tag = match &arm.pattern {
                MatchPattern::EnumVariant {
                    enum_name,
                    variant,
                    fields,
                } if enum_name == &layout.name => {
                    let tag = variants.get(variant.as_str()).copied().ok_or_else(|| {
                        unsupported("an unknown Task enum match variant", arm.span)
                    })?;
                    match (int_variant == Some(tag), fields.as_slice()) {
                        (true, [MatchPattern::Binding(name)]) => {
                            let tag = i32::try_from(tag).map_err(|_| {
                                unsupported("Task enum match tag out of range", arm.span)
                            })?;
                            payload_binding = Some((index, tag, name.as_str()));
                        }
                        (true, [MatchPattern::Wildcard]) | (false, []) => {}
                        _ => return Err(unsupported(
                            "Task enum patterns other than unit variants and int payload bindings or wildcards",
                            arm.span,
                        )),
                    }
                    let tag = i64::try_from(tag)
                        .map_err(|_| unsupported("Task enum match tag out of range", arm.span))?;
                    if covered.contains(&tag) {
                        return Err(unsupported("a repeated Task enum match variant", arm.span));
                    }
                    if arm.guard.is_none() {
                        covered.insert(tag);
                    }
                    Some(tag)
                }
                MatchPattern::Wildcard if arm.guard.is_some() => {
                    return Err(unsupported(
                        "a guarded wildcard in a native Task match",
                        arm.span,
                    ));
                }
                MatchPattern::Wildcard if index + 1 == arms.len() => None,
                _ => return Err(unsupported(
                    "Task match patterns other than same-enum unit variants or a final wildcard",
                    arm.span,
                )),
            };
            tags.push(tag);
        }
        if tags.is_empty() || (tags.last() != Some(&None) && covered.len() != variant_count) {
            return Err(unsupported("a nonexhaustive Task enum match", span));
        }
        // A tag-only pattern never consumes a named enum. Anonymous
        // scrutinees can retire once the tag has been read.
        let tag = if arms.len() > 1 {
            Some(self.unit_enum_tag(source)?)
        } else {
            self.emit(TaskOp::Read { slot: source })?;
            None
        };
        let retire_subject = !self.stable_slots[source.0];
        if retire_subject && payload_binding.is_none() {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        if let [arm] = arms {
            return self.enum_match_value(arm, source, payload_binding, 0, depth);
        }
        let tag = tag.expect("multiple arms read the subject tag");
        if arms.iter().any(|arm| arm.guard.is_some()) {
            return self.guarded_unit_enum_match(tag, arms, &tags, variant_count, depth);
        }
        let join = self.state(TaskTerminator::Terminate)?;
        let before_facts = self.nullable_facts.clone();
        let mut common_facts = before_facts.clone();
        let mut result = None;
        for (index, (arm, expected_tag)) in arms.iter().zip(tags).enumerate() {
            // The last arm covers every remaining tag of the checked layout.
            // Earlier alternatives branch before any of their values run.
            let next = if index + 1 < arms.len() {
                let condition =
                    self.unit_enum_matches_tag(tag, expected_tag.expect("nonfinal arm has a tag"))?;
                let selected = self.state(TaskTerminator::Terminate)?;
                let next = self.state(TaskTerminator::Terminate)?;
                self.terminate(TaskTerminator::Branch {
                    condition,
                    then_state: selected,
                    else_state: next,
                })?;
                self.current = Some(selected);
                Some(next)
            } else {
                None
            };
            self.nullable_facts = before_facts.clone();
            let value = self.enum_match_value(arm, source, payload_binding, index, depth)?;
            self.store_enum_match_value(value, arm.span, &mut result)?;
            common_facts.retain(|slot, fact| self.nullable_facts.get(slot) == Some(fact));
            self.terminate(TaskTerminator::Jump { target: join })?;
            self.current = next;
        }
        self.current = Some(join);
        self.nullable_facts = common_facts;
        Ok(result.expect("a matched arm emits its result"))
    }

    fn guarded_unit_enum_match(
        &mut self,
        tag: SlotId,
        arms: &[MatchArm],
        tags: &[Option<i64>],
        variant_count: usize,
        depth: usize,
    ) -> KuResult<SlotId> {
        let fallback = (tags.last() == Some(&None)).then_some(arms.len() - 1);
        let mut first = HashMap::new();
        let mut following = vec![None; arms.len()];
        for (index, tag) in tags.iter().enumerate().rev() {
            if let Some(tag) = tag {
                following[index] = first.insert(*tag, index).or(fallback);
            }
        }
        let mut heads: Vec<_> = tags
            .iter()
            .enumerate()
            .filter_map(|(index, tag)| {
                tag.filter(|tag| first[tag] == index)
                    .map(|tag| (index, tag))
            })
            .collect();
        let entries: Vec<_> = arms
            .iter()
            .map(|_| self.state(TaskTerminator::Terminate))
            .collect::<KuResult<_>>()?;
        let join = self.state(TaskTerminator::Terminate)?;
        let before_facts = self.nullable_facts.clone();
        let mut common_facts = before_facts.clone();
        let mut incoming = vec![None; arms.len()];
        for &(index, _) in &heads {
            incoming[index] = Some(before_facts.clone());
        }
        // Dispatch on the tag once. A false guard can then enter only a
        // continuation for that same tag, or the shared wildcard.
        let landing = if heads.len() == variant_count {
            heads.pop().expect("an enum match has alternatives").0
        } else {
            let fallback = fallback.expect("unlisted tags require a wildcard");
            incoming[fallback] = Some(before_facts.clone());
            fallback
        };
        for (index, expected_tag) in heads {
            let condition = self.unit_enum_matches_tag(tag, expected_tag)?;
            let next = self.state(TaskTerminator::Terminate)?;
            self.terminate(TaskTerminator::Branch {
                condition,
                then_state: entries[index],
                else_state: next,
            })?;
            self.current = Some(next);
        }
        self.terminate(TaskTerminator::Jump {
            target: entries[landing],
        })?;

        let mut result = None;
        for (index, arm) in arms.iter().enumerate() {
            self.current = Some(entries[index]);
            self.nullable_facts = incoming[index]
                .take()
                .unwrap_or_else(|| before_facts.clone());
            if let Some(guard) = &arm.guard {
                let condition = self.expression(guard, depth + 1)?;
                if self.function.slots[condition.0].ty != value_slot(IrType::Bool) {
                    return Err(unsupported("a non-bool Task match guard", guard.span));
                }
                self.end_nullable_view(condition)?;
                let next = following[index].expect("a guard has a following match arm");
                if let Some(facts) = incoming[next].as_mut() {
                    facts.retain(|slot, fact| self.nullable_facts.get(slot) == Some(fact));
                } else {
                    incoming[next] = Some(self.nullable_facts.clone());
                }
                let selected = self.state(TaskTerminator::Terminate)?;
                self.terminate(TaskTerminator::Branch {
                    condition,
                    then_state: selected,
                    else_state: entries[next],
                })?;
                self.current = Some(selected);
            }
            let value = self.unit_enum_match_value(arm, depth)?;
            self.store_enum_match_value(value, arm.span, &mut result)?;
            common_facts.retain(|slot, fact| self.nullable_facts.get(slot) == Some(fact));
            self.terminate(TaskTerminator::Jump { target: join })?;
        }
        self.current = Some(join);
        self.nullable_facts = common_facts;
        Ok(result.expect("a matched arm emits its result"))
    }

    fn unit_enum_matches_tag(&mut self, tag: SlotId, expected: i64) -> KuResult<SlotId> {
        let alternative = self.constant(TaskConstant::Int(expected), IrType::Int)?;
        let condition = self.slot(value_slot(IrType::Bool))?;
        self.emit(TaskOp::Binary {
            dst: condition,
            op: TaskBinaryOp::Equal,
            left: tag,
            right: alternative,
        })?;
        Ok(condition)
    }

    fn enum_match_value(
        &mut self,
        arm: &MatchArm,
        subject: SlotId,
        payload_binding: Option<(usize, i32, &str)>,
        index: usize,
        depth: usize,
    ) -> KuResult<SlotId> {
        let Some((selected, tag, name)) = payload_binding else {
            return self.unit_enum_match_value(arm, depth);
        };
        let local = if selected == index {
            self.budget.text(name.len(), arm.span)?;
            let dst = self.slot(value_slot(IrType::Int))?;
            self.emit(TaskOp::Unary {
                dst,
                op: TaskUnaryOp::EnumPayloadInt { tag },
                src: subject,
            })?;
            Some(dst)
        } else {
            None
        };
        if !self.stable_slots[subject.0] {
            self.emit(TaskOp::Drop { slot: subject })?;
        }
        if let Some(slot) = local {
            let mut locals = LocalScope::default();
            locals.insert(name.to_owned(), slot);
            self.locals.push(locals);
            let value = self.unit_enum_match_value(arm, depth);
            self.locals.pop();
            value
        } else {
            self.unit_enum_match_value(arm, depth)
        }
    }

    fn store_enum_match_value(
        &mut self,
        source: SlotId,
        span: Span,
        result: &mut Option<SlotId>,
    ) -> KuResult<()> {
        let ty = self.function.slots[source.0].ty.clone();
        let dst = if let Some(dst) = *result {
            if ty != self.function.slots[dst.0].ty {
                return Err(unsupported("mismatched Task match arm result types", span));
            }
            dst
        } else {
            let dst = self.slot(ty.clone())?;
            *result = Some(dst);
            dst
        };
        self.emit(if copy_value(&ty) {
            TaskOp::Copy { dst, src: source }
        } else {
            TaskOp::Move { dst, src: source }
        })
    }

    fn unit_enum_match_value(&mut self, arm: &MatchArm, depth: usize) -> KuResult<SlotId> {
        let source = self.expression(&arm.value, depth + 1)?;
        if !matches!(
            &self.function.slots[source.0].ty,
            TaskSlotType::Value {
                ty: IrType::Int | IrType::Bool | IrType::Null | IrType::Str,
                borrowed: false,
            }
        ) {
            return Err(unsupported(
                "Task match arm result types other than int, bool, null or owned str",
                arm.value.span,
            ));
        }
        Ok(source)
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
        if guarded && task::enum_name(&ty).is_some() {
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
