//! Bind Task-to-ordinary calls to their actual synchronous IR definitions.
//! Only finite computation is admitted on the Task's thread: no recursive or
//! cyclic call graph, callbacks, captures, or blocking external operations.

use std::collections::{HashMap, HashSet};

use super::{
    c_ident, c_symbol, expr_children, unsupported, walk_inst_exprs, walk_terminator_exprs,
};
use crate::ast::ParamMode;
use crate::error::KuResult;
use crate::ir::task::{TaskOp, TaskProgram};
use crate::ir::{
    self, FunctionId, IrCallKind, IrExpr, IrExprKind, IrFunction, IrInst, IrLValue, IrProgram,
    IrTerminator, IrType,
};

fn computation_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "string.len"
            | "string.byte_len"
            | "string.contains"
            | "string.starts_with"
            | "string.ends_with"
            | "json.encode"
            | "json.stringify"
    ) || name.starts_with("time.") && name != "time.sleep"
        || matches!(
            name,
            "ok" | "err"
                | "print"
                | "println"
                | "str"
                | "int"
                | "float"
                | "bool"
                | "__ku_drop_borrow_temp"
                | "__ku_clone"
                | "__ku_clone_nullable"
                | "__ku_optional_select"
                | "__ku_calendar_field_int"
                | "__ku_calendar_field_str"
        )
        || name.starts_with("__ku_nullable_")
        || name.starts_with("__ku_error_")
}

fn allocation_identifier(name: &str) -> bool {
    name.starts_with("__ku_task_sync_")
        || matches!(name, "KuTaskSyncAllocation" | "KuTaskSyncStatusContract")
}

struct Validation<'a> {
    functions: HashMap<FunctionId, &'a IrFunction>,
    layouts: &'a ir::IrLayoutTable,
    asynchronous: HashSet<&'a str>,
    active: HashSet<FunctionId>,
    checked: HashSet<FunctionId>,
    remaining: usize,
}

impl Validation<'_> {
    fn safe_clone_type(&mut self, ty: &IrType) -> KuResult<bool> {
        let layouts = self.layouts;
        let mut pending = vec![ty];
        let mut seen = HashSet::new();
        while let Some(ty) = pending.pop() {
            self.spend(1)?;
            match ty {
                IrType::Int
                | IrType::Bool
                | IrType::Float
                | IrType::Null
                | IrType::Void
                | IrType::Str => {}
                IrType::Nullable(inner) | IrType::Result(inner) => pending.push(inner),
                IrType::Named(name) if name == "__ku_time" || name == "__ku_error_type" => {}
                IrType::Named(name) => {
                    if !seen.insert(name) {
                        continue;
                    }
                    let Some(layout) = layouts.structs.iter().find(|layout| layout.name == *name)
                    else {
                        return Ok(false);
                    };
                    pending.extend(layout.fields.iter().map(|field| &field.ty));
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
    fn spend(&mut self, amount: usize) -> KuResult<()> {
        self.remaining = self
            .remaining
            .checked_sub(amount)
            .ok_or_else(|| unsupported("synchronous Task helper analysis budget exceeded"))?;
        Ok(())
    }

    fn expression(&mut self, expression: &IrExpr) -> KuResult<()> {
        let mut pending = vec![expression];
        while let Some(value) = pending.pop() {
            self.spend(1)?;
            if matches!(&value.ty, IrType::Array(_)) {
                return Err(unsupported("arrays inside a synchronous Task helper require an allocation-aware async helper"));
            }
            if super::is_c_owned_type(&value.ty) && !self.safe_clone_type(&value.ty)? {
                return Err(unsupported("dynamic objects, arrays or external owner layouts inside a synchronous Task helper require an allocation-aware async helper"));
            }
            match &value.kind {
                IrExprKind::Call { callee, args, kind: IrCallKind::Direct(id), .. } => {
                    let declaration = self.functions.get(id).ok_or_else(||
                        unsupported("synchronous Task helper references a missing function"))?;
                    if !matches!(&callee.kind, IrExprKind::Local(name) if *name == declaration.name)
                        || args.len() != declaration.params.len()
                        || args.iter().zip(&declaration.params).any(|(argument, parameter)| argument.ty != parameter.ty)
                        || value.ty != declaration.return_type
                    {
                        return Err(unsupported("synchronous Task helper call does not match its ordinary IR identity/signature"));
                    }
                    self.function(*id)?;
                }
                IrExprKind::Call { kind: IrCallKind::Indirect, .. }
                | IrExprKind::MakeClosure { .. } | IrExprKind::CapturedCell(_)
                | IrExprKind::CellLoad(_) => return Err(unsupported(
                    "callbacks or captured state inside a synchronous Task helper")),
                IrExprKind::Call { kind: IrCallKind::Intrinsic(name), .. }
                    if !computation_intrinsic(name) => return Err(unsupported(format!(
                        "intrinsic `{name}` inside a synchronous Task helper; blocking or allocation-unsafe operations need an async helper"))),
                _ => {}
            }
            pending.extend(expr_children(value));
        }
        Ok(())
    }

    fn function(&mut self, id: FunctionId) -> KuResult<()> {
        if self.checked.contains(&id) {
            return Ok(());
        }
        self.spend(1)?;
        let function = *self
            .functions
            .get(&id)
            .ok_or_else(|| unsupported("synchronous Task helper references a missing function"))?;
        if self.asynchronous.contains(function.name.as_str())
            || function.is_closure_body
            || !function.captures.is_empty()
        {
            return Err(unsupported(format!(
                "synchronous Task helper `{}` calls an async function or captures state",
                function.name
            )));
        }
        if function
            .params
            .iter()
            .any(|param| allocation_identifier(&c_ident(&param.name)))
        {
            return Err(unsupported(
                "synchronous Task helper parameter collides with its allocation context",
            ));
        }
        if self.active.len() >= 32 || !self.active.insert(id) {
            return Err(unsupported(format!(
                "recursive or more than 32 nested synchronous Task helpers at `{}`",
                function.name
            )));
        }
        let mut edges = HashMap::new();
        for (index, block) in function.blocks.iter().enumerate() {
            self.spend(block.instructions.len() + 1)?;
            let mut targets = Vec::new();
            for instruction in &block.instructions {
                if matches!(instruction, IrInst::Let { name, .. } | IrInst::BindError { name, .. }
                    if allocation_identifier(&c_ident(name)))
                {
                    return Err(unsupported(
                        "synchronous Task helper local collides with its allocation context",
                    ));
                }
                match instruction {
                    IrInst::Print(value) if !matches!(value.ty, IrType::Int | IrType::Bool | IrType::Float | IrType::Null | IrType::Str) =>
                        return Err(unsupported("printing aggregate values inside a synchronous Task helper requires an async helper")),
                    IrInst::CellNew { .. } | IrInst::CellStore { .. } | IrInst::CellRelease(_)
                    | IrInst::DefineClosure { .. } | IrInst::Unsupported { .. }
                    | IrInst::Panic(_) => return Err(unsupported(format!(
                        "captured state, panic or unsupported statement in synchronous Task helper `{}`", function.name))),
                    IrInst::BeginTry { catch_block, finally_block, after_block } => {
                        targets.extend(catch_block.iter().copied()); targets.extend(finally_block.iter().copied()); targets.push(*after_block);
                    }
                    IrInst::Store { target, .. } => match target {
                        IrLValue::Index { target, index } => { self.expression(target)?; self.expression(index)?; }
                        IrLValue::Field { target, .. } => self.expression(target)?,
                        _ => {}
                    },
                    _ => {}
                }
                let mut result = Ok(());
                walk_inst_exprs(instruction, &mut |expression| {
                    if result.is_ok() {
                        result = self.expression(expression);
                    }
                });
                result?;
            }
            let mut result = Ok(());
            walk_terminator_exprs(&block.terminator, &mut |expression| {
                if result.is_ok() {
                    result = self.expression(expression);
                }
            });
            result?;
            match &block.terminator {
                IrTerminator::Next => { if let Some(next) = function.blocks.get(index + 1) { targets.push(next.id); } }
                IrTerminator::Jump(target) | IrTerminator::JumpErr { target, .. } => targets.push(*target),
                IrTerminator::Branch { then_block, else_block, .. } => targets.extend([*then_block, *else_block]),
                IrTerminator::ResultBranch { ok_block, err_block, .. } => targets.extend([*ok_block, *err_block]),
                IrTerminator::Safepoint { continue_block, timeout_block } => targets.extend([*continue_block, *timeout_block]),
                IrTerminator::SyncGuard { continue_block, cleanup_block } => targets.extend([*continue_block, *cleanup_block]),
                IrTerminator::ForEach { .. } => return Err(unsupported(format!(
                    "a loop in synchronous Task helper `{}`; use an async helper for iterative work", function.name))),
                _ => {}
            }
            edges.insert(block.id, targets);
        }
        // Explicit DFS bounds stack usage even for adversarial ordinary IR.
        let mut colors = HashMap::new();
        for block in &function.blocks {
            let mut pending = vec![(block.id, false)];
            while let Some((block, leaving)) = pending.pop() {
                self.spend(1)?;
                if leaving {
                    colors.insert(block, 2);
                    continue;
                }
                match colors.get(&block) {
                    Some(2) => continue,
                    Some(1) => return Err(unsupported(format!(
                        "a loop in synchronous Task helper `{}`; use an async helper for iterative work", function.name))),
                    _ => {}
                }
                colors.insert(block, 1);
                pending.push((block, true));
                for target in edges
                    .get(&block)
                    .ok_or_else(|| unsupported("missing synchronous helper block"))?
                {
                    pending.push((*target, false));
                }
            }
        }
        self.active.remove(&id);
        self.checked.insert(id);
        Ok(())
    }
}

pub(super) fn used_functions(
    program: &IrProgram,
    tasks: &TaskProgram,
) -> KuResult<HashSet<FunctionId>> {
    let mut validation = Validation {
        layouts: &program.layouts,
        functions: program
            .functions
            .iter()
            .map(|function| (function.id, function))
            .collect(),
        asynchronous: tasks
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect(),
        active: HashSet::new(),
        checked: HashSet::new(),
        remaining: 100_000,
    };
    for operation in tasks
        .functions
        .iter()
        .flat_map(|function| &function.states)
        .flat_map(|state| &state.operations)
    {
        let TaskOp::SyncCall {
            function,
            callsite,
            name,
            parameters,
            result,
            ..
        } = operation
        else {
            continue;
        };
        let declaration = validation.functions.get(function).ok_or_else(|| {
            unsupported("synchronous Task call has no ordinary function definition")
        })?;
        if declaration.name != *name
            || declaration.return_type != *result
            || declaration.params.len() != parameters.len()
            || declaration
                .params
                .iter()
                .zip(parameters)
                .any(|(actual, expected)| actual.mode != ParamMode::Owned || actual.ty != *expected)
            || !parameters.iter().all(ir::task::sync_call_value_type)
            || !ir::task::sync_call_result_type(result)
        {
            return Err(unsupported(format!(
                "synchronous Task call `{name}` does not match its ordinary function signature"
            )));
        }
        validation.function(*function).map_err(|mut error| {
            error.span = *callsite;
            error.message = format!("synchronous helper `{name}`: {}", error.message);
            error
        })?;
    }
    if !validation.checked.is_empty()
        && program
            .functions
            .iter()
            .any(|function| allocation_identifier(&c_symbol(&function.name)))
    {
        return Err(unsupported(
            "synchronous function collides with the Task allocation bridge",
        ));
    }
    Ok(validation.checked)
}
