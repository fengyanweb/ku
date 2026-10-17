//! Bounded source lowering for the first native Task subset.
//!
//! This is separate from synchronous IR: no async flag is erased, and every
//! unsupported construct is rejected before an artifact can be written.

use std::collections::HashMap;

use crate::{
    ast::{
        BinaryOp, Expr, ExprKind, FnDecl, ImportKind, Item, Literal, ParamMode, Program, Stmt,
        TypeName, UnaryOp,
    },
    error::{KuError, KuResult},
    span::Span,
};

use super::{
    task::{
        self, SlotId, StateId, TaskBinaryOp, TaskConstant, TaskFunction, TaskFunctionId,
        TaskLimits, TaskOp, TaskProgram, TaskScopeExitId, TaskScopeId, TaskSlot, TaskSlotType,
        TaskState, TaskTerminator, TaskUnaryOp,
    },
    IrType,
};

#[derive(Debug)]
pub struct NativeTaskProgram {
    pub tasks: TaskProgram,
    pub entry: TaskFunctionId,
}

#[derive(Clone)]
struct Signature {
    id: TaskFunctionId,
    parameters: Vec<TaskSlotType>,
    result: IrType,
}

fn unsupported(message: &str, span: Span) -> KuError {
    KuError::runtime(
        format!("native async subset does not support {message}"),
        span,
    )
}

fn primitive(ty: &TypeName) -> Option<IrType> {
    match ty {
        TypeName::Int => Some(IrType::Int),
        TypeName::Bool => Some(IrType::Bool),
        TypeName::Null => Some(IrType::Null),
        TypeName::String => Some(IrType::Str),
        _ => None,
    }
}

fn result_payload(ty: &TypeName) -> Option<IrType> {
    if let Some(primitive) = primitive(ty) {
        return Some(primitive);
    }
    if let TypeName::Array(inner) = ty {
        if let Some(element) =
            primitive(inner).filter(|ty| matches!(ty, IrType::Int | IrType::Bool | IrType::Str))
        {
            return Some(IrType::Array(Box::new(element)));
        }
    }
    None
}

fn value_type(ty: &TypeName, span: Span) -> KuResult<IrType> {
    if let Some(ty) = result_payload(ty) {
        return Ok(ty);
    }
    if let TypeName::Result(inner) = ty {
        if let Some(inner) = result_payload(inner) {
            return Ok(IrType::Result(Box::new(inner)));
        }
    }
    Err(unsupported("this parameter or local type", span))
}

fn value_slot(ty: IrType) -> TaskSlotType {
    TaskSlotType::Value {
        ty,
        borrowed: false,
    }
}

fn owned_value(ty: &TaskSlotType) -> bool {
    matches!(ty, TaskSlotType::Callable { .. })
        || matches!(ty, TaskSlotType::Value { ty, borrowed: false }
        if matches!(ty, IrType::Str | IrType::Result(_)) || task::flat_array(ty) || task::error_type(ty))
}

fn local_type(ty: &TypeName, span: Span) -> KuResult<TaskSlotType> {
    if let TypeName::Function {
        params,
        param_modes,
        return_type,
        is_async: true,
    } = ty
    {
        if params.len() != param_modes.len()
            || params.len() > 64
            || param_modes.iter().any(|mode| *mode != ParamMode::Owned)
        {
            return Err(unsupported(
                "borrowed or oversized async callable signatures",
                span,
            ));
        }
        let result = value_type(return_type, span)?;
        if !matches!(result, IrType::Result(_)) {
            return Err(unsupported(
                "async callable signatures without a Result return",
                span,
            ));
        }
        return Ok(TaskSlotType::Callable {
            parameters: params
                .iter()
                .map(|ty| value_type(ty, span))
                .collect::<KuResult<_>>()?,
            result,
        });
    }
    Ok(value_slot(value_type(ty, span)?))
}

fn copy_value(ty: &TaskSlotType) -> bool {
    matches!(
        ty,
        TaskSlotType::Value {
            ty: IrType::Int | IrType::Bool | IrType::Null,
            ..
        }
    )
}

/// Only an explicitly async entry selects this experimental source path. Other
/// forms still pass through the existing exhaustive compiled-async rejection.
pub fn has_async_entry(program: &Program) -> bool {
    program.items.iter().any(|item| {
        matches!(item, Item::Function(function) if function.name == "main" && function.is_async)
    })
}

struct Budget {
    operations: usize,
    names_and_literals: usize,
    expressions: usize,
}

impl Budget {
    fn analysis(&mut self, units: usize, span: Span) -> KuResult<()> {
        self.expressions = self
            .expressions
            .checked_add(units)
            .filter(|&total| total <= TaskLimits::default().max_analysis_work)
            .ok_or_else(|| unsupported("work beyond the Task analysis budget", span))?;
        Ok(())
    }

    fn text(&mut self, bytes: usize, span: Span) -> KuResult<()> {
        self.names_and_literals = self
            .names_and_literals
            .checked_add(bytes)
            .filter(|&total| total <= TaskLimits::default().max_literal_bytes)
            .ok_or_else(|| unsupported("source text beyond the Task literal budget", span))?;
        Ok(())
    }

    fn expression(&mut self, depth: usize, span: Span) -> KuResult<()> {
        if depth > 64 || self.expressions >= TaskLimits::default().max_analysis_work {
            return Err(unsupported(
                "expressions beyond the Task analysis budget",
                span,
            ));
        }
        self.expressions += 1;
        Ok(())
    }
}

/// The caller runs the ordinary checker first. The Task verifier is also run
/// here: malformed types, use-after-move or edge facts cannot bypass it through
/// this internal API. No source runtime/runner is included in the result.
pub fn lower_program(program: &Program) -> KuResult<NativeTaskProgram> {
    let limits = TaskLimits::default();
    if program
        .items
        .iter()
        .filter(|item| matches!(item, Item::Function(_)))
        .count()
        > limits.max_functions
    {
        return Err(unsupported(
            "more than 64 expanded top-level functions",
            Span::default(),
        ));
    }
    let mut budget = Budget {
        operations: 0,
        names_and_literals: 0,
        expressions: 0,
    };
    let mut signatures = HashMap::new();
    let mut declarations = Vec::new();
    let mut entry = None;
    let mut std_time = false;
    let mut std_fs = false;
    for item in &program.items {
        budget.analysis(1, Span::default())?;
        match item {
            Item::Module(module) if module.name == "std:fs" => {
                std_fs = true;
                continue;
            }
            Item::Import(import)
                if (import.path == "std.fs"
                    && (matches!(&import.kind, ImportKind::Glob)
                        || matches!(&import.kind, ImportKind::Namespace(name) if name == "fs")))
                    || (import.path == "std"
                        && matches!(&import.kind, ImportKind::Named(names) if names.len() == 1 && names[0].source == "fs" && names[0].alias.is_none())) =>
            {
                std_fs = true;
                continue;
            }
            Item::Module(module) if module.name == "std:time" => {
                std_time = true;
                continue;
            }
            Item::Import(import)
                if import.path == "std.time"
                    && (matches!(&import.kind, ImportKind::Glob)
                        || matches!(&import.kind, ImportKind::Namespace(name) if name == "time")) =>
            {
                std_time = true;
                continue;
            }
            Item::Import(import)
                if import.path == "std"
                    && matches!(&import.kind, ImportKind::Named(names) if names.len() == 1 && names[0].source == "time" && names[0].alias.is_none()) =>
            {
                std_time = true;
                continue;
            }
            _ => {}
        }

        let Item::Function(function) = item else {
            return Err(unsupported(
                "non-function items after import expansion",
                Span::default(),
            ));
        };
        if !function.is_async || !function.type_params.is_empty() {
            return Err(unsupported(
                "synchronous or generic functions in a native async program",
                function.span,
            ));
        }
        if function.params.len() > limits.max_slots || function.body.len() > limits.max_operations {
            return Err(unsupported(
                "function parameters or statements beyond the Task budget",
                function.span,
            ));
        }
        budget.text(function.name.len(), function.span)?;
        let Some(TypeName::Result(inner)) = &function.return_type else {
            return Err(unsupported(
                "an async function without an explicit supported Result return type",
                function.span,
            ));
        };
        let result =
            IrType::Result(Box::new(result_payload(inner).ok_or_else(|| {
                unsupported("this async Result payload", function.span)
            })?));
        let mut parameters = Vec::new();
        for parameter in &function.params {
            if parameter.mode != ParamMode::Owned {
                return Err(unsupported("borrowed async parameters", parameter.span));
            }
            budget.text(parameter.name.len(), parameter.span)?;
            let parameter_type = local_type(
                parameter
                    .ty
                    .as_ref()
                    .ok_or_else(|| unsupported("untyped async parameters", parameter.span))?,
                parameter.span,
            )?;
            parameters.push(parameter_type);
        }
        let id = TaskFunctionId(declarations.len());
        if function.name == "main" {
            if !parameters.is_empty() || result != IrType::Result(Box::new(IrType::Null)) {
                return Err(unsupported(
                    "an entry other than async fn main(): null!",
                    function.span,
                ));
            }
            entry = Some(id);
        }
        if signatures
            .insert(
                function.name.clone(),
                Signature {
                    id,
                    parameters,
                    result,
                },
            )
            .is_some()
        {
            return Err(unsupported(
                "duplicate expanded function names",
                function.span,
            ));
        }
        declarations.push(function);
    }
    let entry = entry
        .ok_or_else(|| unsupported("a program without async fn main(): null!", Span::default()))?;
    let mut functions = Vec::new();
    for declaration in declarations {
        functions.push(
            FunctionLowerer::new(declaration, &signatures, &mut budget, std_time, std_fs)?
                .lower(declaration)?,
        );
    }
    let tasks = TaskProgram { functions };
    task::verify_and_plan(&tasks, limits)?;
    Ok(NativeTaskProgram { tasks, entry })
}

struct FunctionLowerer<'a> {
    signatures: &'a HashMap<String, Signature>,
    std_time: bool,
    std_fs: bool,
    budget: &'a mut Budget,
    function: TaskFunction,
    locals: Vec<HashMap<String, SlotId>>,
    // ROOT is None. Slots never change their lexical birth scope; Task moves
    // across these owners are deliberately outside this first scoped subset.
    slot_owners: [Option<TaskScopeId>; 64],
    scopes: Vec<TaskScopeId>,
    next_scope: usize,
    loops: Vec<LoopContext>,
    handlers: Vec<ErrorHandler>,
    next_scope_exit: usize,
    finalies: Vec<FinallyContext<'a>>,
    protected: bool,
    in_finally_body: bool,
    statement_depth: usize,
    finally_body_handler_depth: usize,
    pending_primaries: Vec<SlotId>,
    protected_finish: Option<StateId>,
    unwinding: bool,
    scope_drop_states: Vec<(StateId, TaskScopeId)>,
    current: Option<StateId>,
    // Suspend/Await/ScopeDrain exits receive synthetic Value cleanup. Normal
    // Exit leaves its current owners for generated all-Task handoff/finish glue.
    // Fill these finite cleanup regions after every slot is known.
    cleanup_exits: Vec<StateId>,
    span: Span,
}

struct LoopContext {
    break_target: StateId,
    continue_target: StateId,
    retained_depth: usize,
    has_continue: bool,
    finally_depth: usize,
}

#[derive(Clone, Copy)]
struct ErrorHandler {
    target: StateId,
    error: SlotId,
    retained_depth: usize,
    used: bool,
    finally_depth: usize,
}

#[derive(Clone, Copy)]
struct FinallyContext<'a> {
    body: &'a [Stmt],
    cleanup: StateId,
    locals_depth: usize,
    scope_depth: usize,
    handlers_depth: usize,
}

#[derive(Clone, Copy)]
enum FinallyExit {
    Return(SlotId),
    Jump(StateId),
}

impl<'a> FunctionLowerer<'a> {
    fn new(
        declaration: &'a FnDecl,
        signatures: &'a HashMap<String, Signature>,
        budget: &'a mut Budget,
        std_time: bool,
        std_fs: bool,
    ) -> KuResult<Self> {
        let signature = &signatures[&declaration.name];
        let mut lowerer = Self {
            signatures,
            std_time,
            std_fs,
            budget,
            function: TaskFunction {
                id: signature.id,
                name: declaration.name.clone(),
                slots: Vec::new(),
                parameters: Vec::new(),
                entry: StateId(0),
                states: Vec::new(),
                result: signature.result.clone(),
            },
            locals: vec![HashMap::new()],
            slot_owners: [None; 64],
            scopes: Vec::new(),
            next_scope: 0,
            loops: Vec::new(),
            handlers: Vec::new(),
            next_scope_exit: 0,
            finalies: Vec::new(),
            protected: false,
            in_finally_body: false,
            statement_depth: 0,
            finally_body_handler_depth: 0,
            pending_primaries: Vec::new(),
            protected_finish: None,
            unwinding: false,
            scope_drop_states: Vec::new(),
            current: None,
            cleanup_exits: Vec::new(),
            span: declaration.span,
        };
        lowerer.current = Some(lowerer.state(TaskTerminator::Terminate)?);
        for (parameter, ty) in declaration.params.iter().zip(&signature.parameters) {
            if let TaskSlotType::Callable { parameters, .. } = ty {
                lowerer.budget.analysis(parameters.len(), parameter.span)?;
            }
            let slot = lowerer.slot(ty.clone())?;
            if lowerer.locals[0]
                .insert(parameter.name.clone(), slot)
                .is_some()
            {
                return Err(unsupported("duplicate parameter names", parameter.span));
            }
            lowerer.function.parameters.push(slot);
        }
        Ok(lowerer)
    }

    fn slot(&mut self, ty: TaskSlotType) -> KuResult<SlotId> {
        if self.function.slots.len() >= TaskLimits::default().max_slots {
            return Err(unsupported(
                "more than 64 generated Task frame slots",
                self.span,
            ));
        }
        let slot = SlotId(self.function.slots.len());
        self.budget.analysis(1, self.span)?;
        self.slot_owners[slot.0] = self.scopes.last().copied();
        self.function.slots.push(TaskSlot { ty });
        Ok(slot)
    }

    fn state(&mut self, terminator: TaskTerminator) -> KuResult<StateId> {
        if self.function.states.len() >= TaskLimits::default().max_states {
            return Err(unsupported(
                "more than 256 generated Task states",
                self.span,
            ));
        }
        let state = StateId(self.function.states.len());
        self.budget.analysis(1, self.span)?;
        self.function.states.push(TaskState {
            operations: Vec::new(),
            terminator,
        });
        Ok(state)
    }

    fn emit_at(&mut self, state: StateId, operation: TaskOp) -> KuResult<()> {
        if self.budget.operations >= TaskLimits::default().max_operations {
            return Err(unsupported(
                "instructions beyond the Task operation budget",
                self.span,
            ));
        }
        self.budget.operations += 1;
        self.function.states[state.0].operations.push(operation);
        Ok(())
    }

    fn emit(&mut self, operation: TaskOp) -> KuResult<()> {
        self.emit_at(
            self.current
                .ok_or_else(|| unsupported("statements after unconditional exit", self.span))?,
            operation,
        )
    }

    fn terminate(&mut self, terminator: TaskTerminator) -> KuResult<StateId> {
        let state = self
            .current
            .take()
            .ok_or_else(|| unsupported("statements after unconditional exit", self.span))?;
        self.function.states[state.0].terminator = terminator;
        Ok(state)
    }

    fn copy_or_move(&mut self, source: SlotId) -> KuResult<SlotId> {
        let ty = self.function.slots[source.0].ty.clone();
        if self.in_finally_body && matches!(ty, TaskSlotType::Task { .. }) {
            return Err(unsupported(
                "moving a Task inside a native Task finally body",
                self.span,
            ));
        }
        if matches!(ty, TaskSlotType::Task { .. })
            && self.slot_owners[source.0] != self.scopes.last().copied()
        {
            return Err(unsupported(
                "moving a Task across lexical scopes",
                self.span,
            ));
        }
        let destination = self.slot(ty.clone())?;
        self.emit(if copy_value(&ty) {
            TaskOp::Copy {
                dst: destination,
                src: source,
            }
        } else {
            TaskOp::Move {
                dst: destination,
                src: source,
            }
        })?;
        Ok(destination)
    }

    fn exit_result(&mut self, value: SlotId) -> KuResult<()> {
        if self.function.slots[value.0].ty != value_slot(self.function.result.clone()) {
            return Err(unsupported(
                "return values that do not match the declared Result",
                self.span,
            ));
        }
        if self.in_finally_body && !self.protected {
            self.discard_finally_primaries()?;
        }
        if self.protected {
            self.terminate(TaskTerminator::Jump {
                target: self
                    .protected_finish
                    .expect("protected finally continuation"),
            })?;
        } else if !self.finalies.is_empty() {
            // A saved return must survive the lexical drops before finally.
            let saved = self.slot(value_slot(self.function.result.clone()))?;
            self.slot_owners[saved.0] = None;
            self.emit(TaskOp::Move {
                dst: saved,
                src: value,
            })?;
            self.pending_primaries.push(saved);
            self.lower_finally_exit(FinallyExit::Return(saved), 0, 0, self.statement_depth)?;
            self.pending_primaries.pop();
        } else {
            self.terminate(TaskTerminator::Exit { value })?;
        }
        Ok(())
    }

    fn discard_finally_primaries(&mut self) -> KuResult<()> {
        for index in (0..self.pending_primaries.len()).rev() {
            self.emit(TaskOp::DropIfInit {
                slot: self.pending_primaries[index],
            })?;
        }
        Ok(())
    }

    fn cleanup_state(&mut self) -> KuResult<StateId> {
        if let Some(context) = self.finalies.last() {
            return Ok(context.cleanup);
        }
        let state = self.state(TaskTerminator::Terminate)?;
        self.cleanup_exits.push(state);
        Ok(state)
    }

    fn marker(&mut self, operation: TaskOp) -> KuResult<()> {
        let entry = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: entry })?;
        self.current = Some(entry);
        self.emit(operation)?;
        let after = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: after })?;
        self.current = Some(after);
        Ok(())
    }

    fn raise_error(&mut self, error: SlotId) -> KuResult<()> {
        let leaves_finally =
            self.in_finally_body && self.handlers.len() <= self.finally_body_handler_depth;
        if leaves_finally && !self.protected {
            self.discard_finally_primaries()?;
        }
        if let Some(handler) = self.handlers.last().copied() {
            self.emit(TaskOp::Move {
                dst: handler.error,
                src: error,
            })?;
            if self.protected {
                self.handlers.last_mut().expect("active handler").used = true;
                self.terminate(TaskTerminator::Jump {
                    target: handler.target,
                })?;
                Ok(())
            } else if self.finalies.len() > handler.finally_depth
                || (self.unwinding && leaves_finally)
            {
                self.pending_primaries.push(handler.error);
                let result = self.lower_finally_exit(
                    FinallyExit::Jump(handler.target),
                    handler.finally_depth,
                    handler.retained_depth,
                    self.statement_depth,
                );
                self.pending_primaries.pop();
                result
            } else {
                self.handlers.last_mut().expect("active handler").used = true;
                self.lower_scoped_jump(handler.target, handler.retained_depth)
            }
        } else if self.protected {
            self.terminate(TaskTerminator::Jump {
                target: self
                    .protected_finish
                    .expect("protected finally continuation"),
            })?;
            Ok(())
        } else {
            let result = self.slot(value_slot(self.function.result.clone()))?;
            self.emit(TaskOp::WrapError {
                dst: result,
                src: error,
            })?;
            self.exit_result(result)
        }
    }

    fn constant(&mut self, value: TaskConstant, ty: IrType) -> KuResult<SlotId> {
        let slot = self.slot(value_slot(ty))?;
        self.emit(TaskOp::Init { dst: slot, value })?;
        Ok(slot)
    }

    fn expression(&mut self, expression: &Expr, depth: usize) -> KuResult<SlotId> {
        self.budget.expression(depth, expression.span)?;
        match &expression.kind {
            ExprKind::Unary { op, expr } => {
                let source = self.expression(expr, depth + 1)?;
                let (op, ty) = match op {
                    UnaryOp::Negate => (TaskUnaryOp::Negate, IrType::Int),
                    UnaryOp::Not => (TaskUnaryOp::Not, IrType::Bool),
                };
                if self.function.slots[source.0].ty != value_slot(ty.clone()) {
                    return Err(unsupported(
                        "unary operands other than int for - or bool for !",
                        expression.span,
                    ));
                }
                let destination = self.slot(value_slot(ty))?;
                self.emit(TaskOp::Unary {
                    dst: destination,
                    op,
                    src: source,
                })?;
                Ok(destination)
            }
            ExprKind::Binary { left, op, right } if matches!(op, BinaryOp::And | BinaryOp::Or) => {
                self.logical_expression(left, *op, right, depth, expression.span)
            }
            ExprKind::Binary { left, op, right } => {
                let source = self.expression(left, depth + 1)?;
                if *op == BinaryOp::Add
                    && self.function.slots[source.0].ty == value_slot(IrType::Str)
                {
                    return self.string_concat_expression(
                        source,
                        !matches!(&left.kind, ExprKind::Variable(_)),
                        right,
                        expression.span,
                        depth,
                    );
                }
                let equality = matches!(op, BinaryOp::Equal | BinaryOp::NotEqual);
                let input_type = match &self.function.slots[source.0].ty {
                    TaskSlotType::Value {
                        ty: IrType::Int,
                        borrowed: false,
                    } => IrType::Int,
                    TaskSlotType::Value {
                        ty: IrType::Bool,
                        borrowed: false,
                    } if equality => IrType::Bool,
                    _ => {
                        return Err(unsupported(
                            "binary operands other than int or same-type bool equality",
                            left.span,
                        ))
                    }
                };
                // Freeze the completed LHS before any RHS Start/Await/TryResult.
                // Do not rely on C operand order or reread a live source slot.
                let left = self.copy_or_move(source)?;
                let right = self.expression(right, depth + 1)?;
                if self.function.slots[right.0].ty != value_slot(input_type) {
                    return Err(unsupported(
                        "mixed binary operand types in a Task frame",
                        expression.span,
                    ));
                }
                let (op, output_type) = match op {
                    BinaryOp::Add => (TaskBinaryOp::Add, IrType::Int),
                    BinaryOp::Subtract => (TaskBinaryOp::Subtract, IrType::Int),
                    BinaryOp::Multiply => (TaskBinaryOp::Multiply, IrType::Int),
                    BinaryOp::Divide => (TaskBinaryOp::Divide, IrType::Int),
                    BinaryOp::Remainder => (TaskBinaryOp::Remainder, IrType::Int),
                    BinaryOp::Equal => (TaskBinaryOp::Equal, IrType::Bool),
                    BinaryOp::NotEqual => (TaskBinaryOp::NotEqual, IrType::Bool),
                    BinaryOp::Less => (TaskBinaryOp::Less, IrType::Bool),
                    BinaryOp::LessEqual => (TaskBinaryOp::LessEqual, IrType::Bool),
                    BinaryOp::Greater => (TaskBinaryOp::Greater, IrType::Bool),
                    BinaryOp::GreaterEqual => (TaskBinaryOp::GreaterEqual, IrType::Bool),
                    BinaryOp::And | BinaryOp::Or => {
                        unreachable!("logical expressions lower through branches")
                    }
                };
                let destination = self.slot(value_slot(output_type))?;
                self.emit(TaskOp::Binary {
                    dst: destination,
                    op,
                    left,
                    right,
                })?;
                Ok(destination)
            }
            ExprKind::Literal(literal) => match literal {
                Literal::Int(value) => self.constant(TaskConstant::Int(*value), IrType::Int),
                Literal::Bool(value) => self.constant(TaskConstant::Bool(*value), IrType::Bool),
                Literal::Null => self.constant(TaskConstant::Null, IrType::Null),
                Literal::String(value) => {
                    self.budget.text(value.len(), expression.span)?;
                    self.constant(TaskConstant::Str(value.clone()), IrType::Str)
                }
                _ => Err(unsupported("this literal in a Task frame", expression.span)),
            },
            ExprKind::Array(elements) => {
                self.array_literal_expression(elements, expression.span, depth)
            }
            ExprKind::Variable(name) => self.variable_expression(name, expression.span),
            ExprKind::Field { target, name } => {
                let source = self.expression(target, depth + 1)?;
                if !matches!(&self.function.slots[source.0].ty,
                    TaskSlotType::Value { ty, .. } if task::error_type(ty))
                    || !matches!(name.as_str(), "domain" | "code" | "message")
                {
                    return Err(unsupported("this field projection", expression.span));
                }
                let destination = self.slot(value_slot(IrType::Str))?;
                self.emit(TaskOp::ErrorField {
                    dst: destination,
                    src: source,
                    field: name.clone(),
                })?;
                Ok(destination)
            }
            ExprKind::Call { callee, args } => {
                self.call_expression(callee, args, expression.span, depth)
            }
            ExprKind::Await(expression) => {
                if self.in_finally_body {
                    return Err(unsupported(
                        "Await inside a native Task finally body",
                        expression.span,
                    ));
                }
                let source = self.expression(expression, depth + 1)?;
                let TaskSlotType::Task { result } = &self.function.slots[source.0].ty else {
                    return Err(unsupported("await of a non-Task value", expression.span));
                };
                let result = result.clone();
                // Await consumes the existing outer owner directly. Introducing
                // an inner hidden Move would change its cancellation duty.
                let hidden = if self.slot_owners[source.0] != self.scopes.last().copied() {
                    source
                } else {
                    self.copy_or_move(source)?
                };
                let destination = self.slot(value_slot(result))?;
                let poll = self.state(TaskTerminator::Terminate)?;
                let ready = self.state(TaskTerminator::Terminate)?;
                let cleanup = self.cleanup_state()?;
                self.terminate(TaskTerminator::Jump { target: poll })?;
                self.function.states[poll.0].terminator = TaskTerminator::Await {
                    task: hidden,
                    dst: destination,
                    ready,
                    cleanup,
                };
                self.current = Some(ready);
                Ok(destination)
            }
            ExprKind::TryUnwrap { expr } => {
                let source = self.expression(expr, depth + 1)?;
                let TaskSlotType::Value {
                    ty: IrType::Result(inner),
                    ..
                } = &self.function.slots[source.0].ty
                else {
                    return Err(unsupported("? on a non-Result value", expression.span));
                };
                let destination = self.slot(value_slot(inner.as_ref().clone()))?;
                let catches_error = !self.handlers.is_empty();
                let unwind_error = !self.finalies.is_empty() || self.protected;
                let error = self.slot(value_slot(if catches_error {
                    super::error_ir_type()
                } else {
                    self.function.result.clone()
                }))?;
                let ok = self.state(TaskTerminator::Terminate)?;
                let err = self.state(if catches_error || unwind_error {
                    TaskTerminator::Terminate
                } else {
                    TaskTerminator::Exit { value: error }
                })?;
                self.terminate(TaskTerminator::TryResult {
                    src: source,
                    ok_value: destination,
                    err_result: error,
                    ok,
                    err,
                })?;
                if catches_error || unwind_error {
                    self.current = Some(err);
                    if catches_error {
                        self.raise_error(error)?;
                    } else {
                        self.exit_result(error)?;
                    }
                }
                self.current = Some(ok);
                Ok(destination)
            }
            _ => Err(unsupported(
                "this expression in the bounded primitive Task subset",
                expression.span,
            )),
        }
    }

    // String owners and allocation temporaries stay outside ordinary recursive
    // expression frames. Preserve the completed left value before RHS effects.
    #[inline(never)]
    fn string_concat_expression(
        &mut self,
        source: SlotId,
        temporary_left: bool,
        right: &Expr,
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        let left = self.slot(value_slot(IrType::Str))?;
        self.emit(TaskOp::StringClone {
            dst: left,
            src: source,
        })?;
        if temporary_left {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        let temporary_right = !matches!(&right.kind, ExprKind::Variable(_));
        let right = self.expression(right, depth + 1)?;
        if self.function.slots[right.0].ty != value_slot(IrType::Str) {
            return Err(unsupported(
                "mixed string concatenation operand types",
                span,
            ));
        }
        let destination = self.slot(value_slot(IrType::Str))?;
        self.emit(TaskOp::StringConcat {
            dst: destination,
            left,
            right,
        })?;
        // This owner is a lowering-only snapshot. Both input expressions retain
        // their ordinary ownership, including any explicit move in the RHS.
        self.emit(TaskOp::Drop { slot: left })?;
        if temporary_right {
            self.emit(TaskOp::Drop { slot: right })?;
        }
        Ok(destination)
    }

    // Keep the large Option<IrType> argument out of every recursive Unary
    // frame, including frames which never evaluate an array expression.
    #[inline(never)]
    fn array_literal_expression(
        &mut self,
        elements: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        self.array_expression(elements, None, span, depth)
    }

    #[inline(never)]
    fn array_expression(
        &mut self,
        elements: &[Expr],
        expected: Option<IrType>,
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        if elements.len() > TaskLimits::default().max_slots {
            return Err(unsupported(
                "array elements beyond the Task slot limit",
                span,
            ));
        }
        self.budget.analysis(elements.len(), span)?;
        let mut element_type = expected;
        let mut sources = Vec::with_capacity(elements.len());
        for expression in elements {
            let value = self.expression(expression, depth + 1)?;
            let TaskSlotType::Value {
                ty,
                borrowed: false,
            } = &self.function.slots[value.0].ty
            else {
                return Err(unsupported(
                    "array element other than int, bool or str",
                    span,
                ));
            };
            if !matches!(ty, IrType::Int | IrType::Bool | IrType::Str)
                || element_type.as_ref().is_some_and(|expected| expected != ty)
            {
                return Err(unsupported("mixed or nested array elements", span));
            }
            element_type = Some(ty.clone());
            sources.push(self.copy_or_move(value)?);
        }
        let ty = element_type.ok_or_else(|| {
            unsupported(
                "empty array without an explicit flat array annotation",
                span,
            )
        })?;
        let dst = self.slot(value_slot(IrType::Array(Box::new(ty))))?;
        self.emit(TaskOp::ArrayMake {
            dst,
            elements: sources,
        })?;
        Ok(dst)
    }

    // Keep function signatures and Callable temporaries outside each recursive
    // expression frame. Even a Unary-only AST must fit the original depth gate.
    #[inline(never)]
    fn variable_expression(&mut self, name: &str, span: Span) -> KuResult<SlotId> {
        if let Some(local) = self.local(name) {
            return Ok(local);
        }
        let Some(signature) = self.signatures.get(name) else {
            return Err(unsupported(
                "an unknown local or first-class function value",
                span,
            ));
        };
        let work = signature
            .parameters
            .iter()
            .try_fold(1usize, |work, parameter| {
                work.checked_add(match parameter {
                    TaskSlotType::Callable { parameters, .. } => parameters.len() + 1,
                    _ => 1,
                })
            })
            .ok_or_else(|| unsupported("signature work beyond the Task analysis budget", span))?;
        self.budget.analysis(work, span)?;
        let signature = signature.clone();
        // Callable identities remain first-order. A helper that accepts a
        // callable may be called directly, but is not itself a callable value.
        let mut parameters = Vec::with_capacity(signature.parameters.len());
        for parameter in signature.parameters {
            self.budget.analysis(1, span)?;
            let TaskSlotType::Value {
                ty,
                borrowed: false,
            } = parameter
            else {
                return Err(unsupported(
                    "a higher-order helper as a function value",
                    span,
                ));
            };
            parameters.push(ty);
        }
        let slot = self.slot(TaskSlotType::Callable {
            parameters,
            result: signature.result,
        })?;
        self.emit(TaskOp::InitCallable {
            dst: slot,
            function: signature.id,
        })?;
        Ok(slot)
    }

    // Array type construction and slot temporaries must not enlarge every
    // recursive ordinary-call frame on the original nested-call depth route.
    #[inline(never)]
    fn array_method_expression(&mut self, object: &Expr, name: &str) -> KuResult<Option<SlotId>> {
        if let Some(source) = match &object.kind {
            ExprKind::Variable(name) => self.local(name),
            _ => None,
        } {
            if matches!(&self.function.slots[source.0].ty,TaskSlotType::Value {ty,..} if task::flat_array(ty))
            {
                let ty = if name == "clone" {
                    self.function.slots[source.0].ty.clone()
                } else {
                    value_slot(IrType::Int)
                };
                let dst = self.slot(ty)?;
                self.emit(if name == "clone" {
                    TaskOp::ArrayClone { dst, src: source }
                } else {
                    TaskOp::ArrayLen { dst, src: source }
                })?;
                if name == "is_empty" {
                    let zero = self.constant(TaskConstant::Int(0), IrType::Int)?;
                    let empty = self.slot(value_slot(IrType::Bool))?;
                    self.emit(TaskOp::Binary {
                        dst: empty,
                        op: TaskBinaryOp::Equal,
                        left: dst,
                        right: zero,
                    })?;
                    return Ok(Some(empty));
                }
                return Ok(Some(dst));
            }
        }
        Ok(None)
    }

    // This precheck never recursively walks the source or grows its worklist.
    // Unknown/effectful syntax and the 64-node boundary conservatively snapshot.
    #[inline(never)]
    fn pure_array_index(&mut self, expression: &Expr) -> KuResult<bool> {
        let mut pending = [None; 64];
        pending[0] = Some(expression);
        let mut count = 1usize;
        let mut visited = 0usize;
        while count != 0 {
            if visited == pending.len() {
                return Ok(false);
            }
            count -= 1;
            let expression = pending[count].take().expect("bounded index worklist");
            visited += 1;
            self.budget.analysis(1, expression.span)?;
            match &expression.kind {
                ExprKind::Literal(_) | ExprKind::Variable(_) => {}
                ExprKind::Unary { expr, .. } => {
                    if count == pending.len() {
                        return Ok(false);
                    }
                    pending[count] = Some(expr.as_ref());
                    count += 1;
                }
                ExprKind::Binary { left, right, .. } => {
                    if count + 2 > pending.len() {
                        return Ok(false);
                    }
                    pending[count] = Some(right.as_ref());
                    pending[count + 1] = Some(left.as_ref());
                    count += 2;
                }
                ExprKind::Call { callee, args } if args.is_empty() => {
                    let ExprKind::Field { target, name } = &callee.kind else {
                        return Ok(false);
                    };
                    let ExprKind::Variable(binding) = &target.kind else {
                        return Ok(false);
                    };
                    let Some(source) = self.local(binding) else {
                        return Ok(false);
                    };
                    let TaskSlotType::Value {
                        ty,
                        borrowed: false,
                    } = &self.function.slots[source.0].ty
                    else {
                        return Ok(false);
                    };
                    let readonly = match name.as_str() {
                        "len" => ty == &IrType::Str || task::flat_array(ty),
                        "byte_len" => ty == &IrType::Str,
                        "is_empty" => task::flat_array(ty),
                        _ => false,
                    };
                    if !readonly || visited + 2 > pending.len() {
                        return Ok(false);
                    }
                    // The direct Field and Variable nodes are known without
                    // visiting another subtree, but still pay for their work.
                    self.budget.analysis(2, expression.span)?;
                    visited += 2;
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    #[inline(never)]
    fn array_try_get_expression(
        &mut self,
        object: &Expr,
        index: &Expr,
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        let source = self.expression(object, depth + 1)?;
        let TaskSlotType::Value {
            ty,
            borrowed: false,
        } = &self.function.slots[source.0].ty
        else {
            return Err(unsupported("try_get on a non-owned flat array", span));
        };
        let IrType::Array(element) = ty else {
            return Err(unsupported("try_get on a non-array value", span));
        };
        if !task::flat_array(ty) {
            return Err(unsupported(
                "try_get on a nested or unsupported array",
                span,
            ));
        }
        let array_type = ty.clone();
        let result_type = IrType::Result(element.clone());
        let temporary = !matches!(&object.kind, ExprKind::Variable(_));
        let array = if !temporary && !self.pure_array_index(index)? {
            let snapshot = self.slot(value_slot(array_type))?;
            self.emit(TaskOp::ArrayClone {
                dst: snapshot,
                src: source,
            })?;
            snapshot
        } else {
            source
        };
        let index = self.expression(index, depth + 1)?;
        if self.function.slots[index.0].ty != value_slot(IrType::Int) {
            return Err(unsupported("try_get with a non-int index", span));
        }
        let destination = self.slot(value_slot(result_type))?;
        self.emit(TaskOp::ArrayTryGet {
            dst: destination,
            array,
            index,
        })?;
        if temporary || array != source {
            self.emit(TaskOp::Drop { slot: array })?;
        }
        Ok(destination)
    }

    #[inline(never)]
    fn string_or_callable_method_expression(
        &mut self,
        object: &Expr,
        name: &str,
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        let source = self.expression(object, depth + 1)?;
        let ty = self.function.slots[source.0].ty.clone();
        if ty == value_slot(IrType::Str) {
            let destination = self.slot(value_slot(if name == "clone" {
                IrType::Str
            } else {
                IrType::Int
            }))?;
            self.emit(if name == "clone" {
                TaskOp::StringClone {
                    dst: destination,
                    src: source,
                }
            } else {
                TaskOp::StringLen {
                    dst: destination,
                    src: source,
                    bytes: name == "byte_len",
                }
            })?;
            // Anonymous receivers have completed their last View use. Clear
            // them here, including in a loop condition reusing the same slots.
            // Named variables retain their existing owner and lexical lifetime.
            if !matches!(&object.kind, ExprKind::Variable(_)) {
                self.emit(TaskOp::Drop { slot: source })?;
            }
            return Ok(destination);
        }
        if name == "clone" && matches!(ty, TaskSlotType::Callable { .. }) {
            let destination = self.slot(ty)?;
            self.emit(TaskOp::CloneCallable {
                dst: destination,
                src: source,
            })?;
            return Ok(destination);
        }
        Err(unsupported(
            "this receiver for a Task string or callable method",
            span,
        ))
    }

    // Ordinary nested calls do not reserve the field-call branch's type,
    // timer and ownership temporaries on every recursive frame.
    #[inline(never)]
    fn field_call_expression(
        &mut self,
        object: &Expr,
        name: &str,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        if matches!(&object.kind, ExprKind::Variable(module) if module == "fs")
            && matches!(name, "write" | "try_write" | "read" | "try_read" | "exists")
            && self.std_fs
            && self.local("fs").is_none()
            && !self.signatures.contains_key("fs")
        {
            return if name == "exists" {
                self.blocking_exists(args, span, depth)
            } else if matches!(name, "read" | "try_read") {
                self.blocking_read(args, span, depth)
            } else {
                self.blocking_write(args, span, depth)
            };
        }
        if matches!(&object.kind, ExprKind::Variable(module) if module == "time")
            && name == "sleep"
            && self.std_time
            && self.local("time").is_none()
            && !self.signatures.contains_key("time")
        {
            if self.in_finally_body {
                return Err(unsupported(
                    "time.sleep inside a native Task finally body",
                    span,
                ));
            }
            if args.len() != 1 {
                return Err(unsupported("time.sleep with other than one argument", span));
            }
            let source = self.expression(&args[0], depth + 1)?;
            if self.function.slots[source.0].ty != value_slot(IrType::Int) {
                return Err(unsupported(
                    "time.sleep arguments other than integer milliseconds",
                    args[0].span,
                ));
            }
            let millis = self.copy_or_move(source)?;
            let destination = self.slot(value_slot(IrType::Result(Box::new(IrType::Null))))?;
            let poll = self.state(TaskTerminator::Terminate)?;
            let ready = self.state(TaskTerminator::Terminate)?;
            let cleanup = self.cleanup_state()?;
            self.terminate(TaskTerminator::Jump { target: poll })?;
            self.function.states[poll.0].terminator = TaskTerminator::Timer {
                millis,
                dst: destination,
                ready,
                cleanup,
            };
            self.current = Some(ready);
            return Ok(destination);
        }
        if name == "try_get" {
            if args.len() != 1 {
                return Err(unsupported(
                    "try_get with other than one index argument",
                    span,
                ));
            }
            return self.array_try_get_expression(object, &args[0], span, depth);
        }
        if args.is_empty() && matches!(name, "len" | "is_empty" | "clone") {
            if let Some(destination) = self.array_method_expression(object, name)? {
                return Ok(destination);
            }
        }
        if args.is_empty() && matches!(name, "len" | "byte_len" | "clone") {
            return self.string_or_callable_method_expression(object, name, span, depth);
        }
        self.indirect_call(callee, args, span, depth)
    }

    // Calls carry signatures, argument vectors and ownership temporaries. Do
    // not reserve that storage on every recursive Unary/Binary expression.
    #[inline(never)]
    fn call_expression(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        if let ExprKind::Field {
            target: object,
            name,
        } = &callee.kind
        {
            return self.field_call_expression(object, name, callee, args, span, depth);
        }
        let ExprKind::Variable(name) = &callee.kind else {
            return self.indirect_call(callee, args, span, depth);
        };
        if let Some(local) = self.local(name) {
            if matches!(
                self.function.slots[local.0].ty,
                TaskSlotType::Callable { .. }
            ) {
                return self.indirect_call(callee, args, span, depth);
            }
            return Err(unsupported("a call through a local value", span));
        }
        // The ordinary checker resolves declared functions before
        // builtin fallback. Native lowering must preserve that choice.
        let declared_function = self.signatures.contains_key(name);
        if !declared_function && name == "ok" {
            return self.ok_expression(args, span, depth);
        }
        if !declared_function && (name == "println" || name == "print") {
            if args.len() != 1 {
                return Err(unsupported("print with other than one argument", span));
            }
            let value = self.expression(&args[0], depth + 1)?;
            self.emit(TaskOp::Print {
                value,
                newline: name == "println",
            })?;
            if !matches!(&args[0].kind, ExprKind::Variable(_))
                && owned_value(&self.function.slots[value.0].ty)
            {
                self.emit(TaskOp::Drop { slot: value })?;
            }
            return self.constant(TaskConstant::Null, IrType::Null);
        }
        if self.in_finally_body {
            return Err(unsupported(
                "starting a Task inside a native Task finally body",
                span,
            ));
        }
        let signature = self.signatures.get(name).ok_or_else(|| {
            unsupported("a call other than a known top-level async function", span)
        })?;
        let work = signature
            .parameters
            .iter()
            .try_fold(1usize, |work, parameter| {
                work.checked_add(match parameter {
                    TaskSlotType::Callable { parameters, .. } => parameters.len() + 1,
                    _ => 1,
                })
            })
            .ok_or_else(|| unsupported("signature work beyond the Task analysis budget", span))?;
        self.budget.analysis(work, span)?;
        let signature = signature.clone();
        if args.len() != signature.parameters.len() || args.len() > 64 {
            return Err(unsupported("mismatched async call arguments", span));
        }
        let mut arguments = Vec::new();
        for (argument, expected) in args.iter().zip(&signature.parameters) {
            let source = if matches!(&argument.kind, ExprKind::Array(_)) {
                self.direct_argument_expression(argument, expected, depth + 1)?
            } else {
                self.expression(argument, depth + 1)?
            };
            if &self.function.slots[source.0].ty != expected {
                return Err(unsupported(
                    "mismatched async argument types",
                    argument.span,
                ));
            }
            // Snapshot each argument before evaluating the next one,
            // including when that later expression itself awaits.
            arguments.push(self.copy_or_move(source)?);
        }
        let destination = self.slot(TaskSlotType::Task {
            result: signature.result,
        })?;
        self.emit(TaskOp::Start {
            dst: destination,
            function: signature.id,
            arguments,
        })?;
        Ok(destination)
    }

    // Result payload type construction stays outside recursive ordinary-call
    // frames, including when the payload is a flat array.
    #[inline(never)]
    fn ok_expression(&mut self, args: &[Expr], span: Span, depth: usize) -> KuResult<SlotId> {
        if args.len() != 1 {
            return Err(unsupported("ok with other than one argument", span));
        }
        let source = self.expression(&args[0], depth + 1)?;
        let TaskSlotType::Value { ty, .. } = &self.function.slots[source.0].ty else {
            return Err(unsupported("Task payloads inside Result", span));
        };
        if !matches!(ty, IrType::Int | IrType::Bool | IrType::Null | IrType::Str)
            && !task::flat_array(ty)
        {
            return Err(unsupported("nested Result values", span));
        }
        let destination = self.slot(value_slot(IrType::Result(Box::new(ty.clone()))))?;
        self.emit(TaskOp::WrapOk {
            dst: destination,
            src: source,
        })?;
        Ok(destination)
    }

    // Expected array element construction stays outside recursive ordinary
    // call frames. Match the ordinary expression entrance's budget exactly.
    #[inline(never)]
    fn direct_argument_expression(
        &mut self,
        argument: &Expr,
        expected: &TaskSlotType,
        depth: usize,
    ) -> KuResult<SlotId> {
        if let (
            ExprKind::Array(elements),
            TaskSlotType::Value {
                ty: IrType::Array(inner),
                borrowed: false,
            },
        ) = (&argument.kind, expected)
        {
            self.budget.expression(depth, argument.span)?;
            return self.array_expression(
                elements,
                Some(inner.as_ref().clone()),
                argument.span,
                depth,
            );
        }
        self.expression(argument, depth)
    }

    #[inline(never)]
    fn blocking_write(&mut self, args: &[Expr], span: Span, depth: usize) -> KuResult<SlotId> {
        if self.in_finally_body {
            return Err(unsupported(
                "fs.write inside a native Task finally body",
                span,
            ));
        }
        if args.len() != 2 {
            return Err(unsupported("fs.write with other than two arguments", span));
        }
        // The ordinary checker rejects conflicting mutation/moves across View
        // arguments. Neither source is consumed; the pool owns independent data.
        let path = self.expression(&args[0], depth + 1)?;
        let content = self.expression(&args[1], depth + 1)?;
        if self.function.slots[path.0].ty != value_slot(IrType::Str)
            || self.function.slots[content.0].ty != value_slot(IrType::Str)
        {
            return Err(unsupported("fs.write arguments other than strings", span));
        }
        let dst = self.slot(value_slot(IrType::Result(Box::new(IrType::Null))))?;
        let poll = self.state(TaskTerminator::Terminate)?;
        let ready = self.state(TaskTerminator::Terminate)?;
        let cleanup = self.cleanup_state()?;
        self.terminate(TaskTerminator::Jump { target: poll })?;
        self.function.states[poll.0].terminator = TaskTerminator::BlockingWrite {
            path,
            content,
            dst,
            ready,
            cleanup,
        };
        self.current = Some(ready);
        for (argument, source) in args.iter().zip([path, content]) {
            if !matches!(argument.kind, ExprKind::Variable(_)) {
                self.emit(TaskOp::Drop { slot: source })?;
            }
        }
        Ok(dst)
    }

    fn blocking_read(&mut self, args: &[Expr], span: Span, depth: usize) -> KuResult<SlotId> {
        if self.in_finally_body {
            return Err(unsupported(
                "fs.read inside a native Task finally body",
                span,
            ));
        }
        if args.len() != 1 {
            return Err(unsupported("fs.read with other than one argument", span));
        }
        let path = self.expression(&args[0], depth + 1)?;
        if self.function.slots[path.0].ty != value_slot(IrType::Str) {
            return Err(unsupported("fs.read argument other than string", span));
        }
        let dst = self.slot(value_slot(IrType::Result(Box::new(IrType::Str))))?;
        let poll = self.state(TaskTerminator::Terminate)?;
        let ready = self.state(TaskTerminator::Terminate)?;
        let cleanup = self.cleanup_state()?;
        self.terminate(TaskTerminator::Jump { target: poll })?;
        self.function.states[poll.0].terminator = TaskTerminator::BlockingRead {
            path,
            dst,
            ready,
            cleanup,
        };
        self.current = Some(ready);
        if !matches!(args[0].kind, ExprKind::Variable(_)) {
            self.emit(TaskOp::Drop { slot: path })?;
        }
        Ok(dst)
    }

    #[inline(never)]
    fn blocking_exists(&mut self, args: &[Expr], span: Span, depth: usize) -> KuResult<SlotId> {
        if self.in_finally_body {
            return Err(unsupported(
                "fs.exists inside a native Task finally body",
                span,
            ));
        }
        if args.len() != 1 {
            return Err(unsupported("fs.exists with other than one argument", span));
        }
        let path = self.expression(&args[0], depth + 1)?;
        if self.function.slots[path.0].ty != value_slot(IrType::Str) {
            return Err(unsupported("fs.exists argument other than string", span));
        }
        let dst = self.slot(value_slot(IrType::Bool))?;
        let poll = self.state(TaskTerminator::Terminate)?;
        let ready = self.state(TaskTerminator::Terminate)?;
        let cleanup = self.cleanup_state()?;
        self.terminate(TaskTerminator::Jump { target: poll })?;
        self.function.states[poll.0].terminator = TaskTerminator::BlockingExists {
            path,
            dst,
            ready,
            cleanup,
        };
        self.current = Some(ready);
        if !matches!(args[0].kind, ExprKind::Variable(_)) {
            self.emit(TaskOp::Drop { slot: path })?;
        }
        Ok(dst)
    }

    fn indirect_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        depth: usize,
    ) -> KuResult<SlotId> {
        if self.in_finally_body {
            return Err(unsupported(
                "starting a Task inside a native Task finally body",
                span,
            ));
        }
        // Retain the chosen identity before the first argument can suspend.
        // Calls read the binding; assigning the same value still moves it.
        let source = self.expression(callee, depth + 1)?;
        let ty = self.function.slots[source.0].ty.clone();
        let TaskSlotType::Callable { parameters, result } = &ty else {
            return Err(unsupported(
                "indirect or method calls through a non-callable",
                span,
            ));
        };
        if args.len() != parameters.len() || args.len() > 64 {
            return Err(unsupported("mismatched async call arguments", span));
        }
        let frozen = self.slot(ty.clone())?;
        self.emit(TaskOp::CloneCallable {
            dst: frozen,
            src: source,
        })?;
        if !matches!(callee.kind, ExprKind::Variable(_)) {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        let mut arguments = Vec::new();
        for (argument, expected) in args.iter().zip(parameters) {
            let expected = value_slot(expected.clone());
            let value = self.direct_argument_expression(argument, &expected, depth + 1)?;
            if self.function.slots[value.0].ty != expected {
                return Err(unsupported(
                    "mismatched async argument types",
                    argument.span,
                ));
            }
            arguments.push(self.copy_or_move(value)?);
        }
        let destination = self.slot(TaskSlotType::Task {
            result: result.clone(),
        })?;
        self.emit(TaskOp::StartIndirect {
            dst: destination,
            callee: frozen,
            arguments,
        })?;
        self.emit(TaskOp::Drop { slot: frozen })?;
        Ok(destination)
    }

    fn logical_expression(
        &mut self,
        left: &Expr,
        op: BinaryOp,
        right: &Expr,
        depth: usize,
        span: Span,
    ) -> KuResult<SlotId> {
        let condition = self.expression(left, depth + 1)?;
        if self.function.slots[condition.0].ty != value_slot(IrType::Bool) {
            return Err(unsupported("non-bool logical operands", left.span));
        }
        // Dominates both paths. The skipped path owns no RHS Task/header.
        let destination = self.constant(TaskConstant::Bool(op == BinaryOp::Or), IrType::Bool)?;
        let right_state = self.state(TaskTerminator::Terminate)?;
        let join = self.state(TaskTerminator::Terminate)?;
        let (then_state, else_state) = match op {
            BinaryOp::And => (right_state, join),
            BinaryOp::Or => (join, right_state),
            _ => return Err(unsupported("a non-logical short-circuit operation", span)),
        };
        self.terminate(TaskTerminator::Branch {
            condition,
            then_state,
            else_state,
        })?;
        self.current = Some(right_state);
        // Always lower/validate the RHS, even for a constant deciding LHS.
        // Nested Await/TryResult/logical expressions may replace current state.
        let right = self.expression(right, depth + 1)?;
        if self.function.slots[right.0].ty != value_slot(IrType::Bool) {
            return Err(unsupported("non-bool logical operands", span));
        }
        self.emit(TaskOp::Copy {
            dst: destination,
            src: right,
        })?;
        self.terminate(TaskTerminator::Jump { target: join })?;
        self.current = Some(join);
        Ok(destination)
    }

    fn local(&self, name: &str) -> Option<SlotId> {
        self.locals
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    // An existing flat array binding supplies the missing element type for
    // `items = []`. Keep expected-type materialization out of recursive
    // expression frames and do not extend inference to Result constructors.
    #[inline(never)]
    fn reassignment_expression(
        &mut self,
        value: &Expr,
        expected: &TaskSlotType,
        depth: usize,
    ) -> KuResult<SlotId> {
        if let (
            ExprKind::Array(elements),
            TaskSlotType::Value {
                ty: IrType::Array(element),
                borrowed: false,
            },
        ) = (&value.kind, expected)
        {
            if elements.is_empty()
                && matches!(element.as_ref(), IrType::Int | IrType::Bool | IrType::Str)
            {
                self.budget.expression(depth, value.span)?;
                return self.array_expression(
                    elements,
                    Some(element.as_ref().clone()),
                    value.span,
                    depth,
                );
            }
        }
        self.expression(value, depth)
    }

    fn bind(
        &mut self,
        name: &str,
        annotation: Option<&TypeName>,
        value: &Expr,
        span: Span,
        declaration: bool,
        depth: usize,
    ) -> KuResult<()> {
        if !declaration {
            if let Some(destination) = self.local(name) {
                let expected = self.function.slots[destination.0].ty.clone();
                let copy = copy_value(&expected);
                let owned = matches!(
                    expected,
                    TaskSlotType::Value {
                        borrowed: false,
                        ..
                    }
                ) && owned_value(&expected);
                if !copy && !owned {
                    return Err(unsupported(
                        "reassignment of Task or Callable values in the native Task subset",
                        span,
                    ));
                }
                self.budget.text(name.len(), span)?;
                // Resolve and finish the RHS in the original lexical environment.
                // Await, ? and short circuit may change the current state; only
                // their successful endpoint may replace the existing binding.
                // An explicit RHS move still consumes its source as usual.
                let source = self.reassignment_expression(value, &expected, depth)?;
                if self.function.slots[source.0].ty != expected {
                    return Err(unsupported(
                        if copy {
                            "a mismatched Copy reassignment"
                        } else {
                            "a mismatched Owned reassignment"
                        },
                        span,
                    ));
                }
                if source == destination {
                    // The raw Move/Copy ABI still requires distinct slots. A
                    // self-assignment proves initialization without destroying
                    // or consuming the binding's current owner.
                    return self.emit(TaskOp::Read { slot: source });
                }
                if copy {
                    return self.emit(TaskOp::Copy {
                        dst: destination,
                        src: source,
                    });
                }
                // The successful RHS now owns a distinct initialized slot.
                // DropIfInit also permits reinitializing a moved or MAY-live
                // destination. Neither operation suspends or can fail at runtime;
                // keep both at this same endpoint and preserve slot provenance.
                self.emit(TaskOp::DropIfInit { slot: destination })?;
                return self.emit(TaskOp::Move {
                    dst: destination,
                    src: source,
                });
            }
        }
        let already_bound = if declaration {
            self.locals
                .last()
                .expect("ROOT scope exists")
                .contains_key(name)
        } else {
            self.local(name).is_some()
        };
        if already_bound {
            return Err(unsupported(
                "reassignment or duplicate local declarations in the native Task subset",
                span,
            ));
        }
        self.budget.text(name.len(), span)?;
        let source = if let (ExprKind::Array(elements), Some(TypeName::Array(inner))) =
            (&value.kind, annotation)
        {
            // The annotated empty-array entry still consumes the ordinary
            // expression depth/work budget before using its expected type.
            self.budget.expression(depth, value.span)?;
            self.array_expression(elements, primitive(inner), span, depth)?
        } else {
            self.expression(value, depth)?
        };
        if let Some(annotation) = annotation {
            if self.function.slots[source.0].ty != local_type(annotation, span)? {
                return Err(unsupported("a mismatched local annotation", span));
            }
        }
        let destination = self.copy_or_move(source)?;
        self.locals
            .last_mut()
            .expect("ROOT scope exists")
            .insert(name.to_owned(), destination);
        Ok(())
    }

    fn lower_block(&mut self, body: &'a [Stmt], depth: usize) -> KuResult<()> {
        let previous_depth = self.statement_depth;
        self.statement_depth = depth;
        // Charge before traversal; width is not recursion depth. No new AST is
        // cloned; structured statements check before each recursive descent.
        if body.len() > TaskLimits::default().max_operations {
            return Err(unsupported("statements beyond the Task budget", self.span));
        }
        self.budget.analysis(body.len(), self.span)?;
        for statement in body {
            if self.current.is_none() {
                return Err(unsupported(
                    "statements after unconditional exit",
                    self.span,
                ));
            }
            match statement {
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    span,
                } => {
                    self.lower_if(condition, then_branch, else_branch, *span, depth)?;
                }
                Stmt::While {
                    condition,
                    body,
                    span,
                } => {
                    if self.in_finally_body {
                        return Err(unsupported(
                            "loops inside a native Task finally body",
                            *span,
                        ));
                    }
                    self.lower_while(condition, body, *span, depth)?;
                }
                Stmt::For {
                    name,
                    iterable,
                    body,
                    span,
                } => {
                    if self.in_finally_body {
                        return Err(unsupported(
                            "loops inside a native Task finally body",
                            *span,
                        ));
                    }
                    self.lower_for(name, iterable, body, *span, depth)?;
                }
                Stmt::Break { span } => self.lower_loop_exit(false, *span)?,
                Stmt::Continue { span } => self.lower_loop_exit(true, *span)?,
                Stmt::Try {
                    body,
                    catch_name,
                    catch_body,
                    finally_body,
                    span,
                } => {
                    self.lower_try(
                        body,
                        catch_name.as_deref(),
                        catch_body,
                        finally_body,
                        *span,
                        depth,
                    )?;
                    if self.current.is_none() {
                        // The checker requires a direct Result exit even when
                        // both Try routes already exit. Its validated tail is
                        // unreachable; do not invent a second native exit.
                        break;
                    }
                }
                Stmt::Assign { name, value, span } => {
                    self.bind(name, None, value, *span, false, depth)?
                }
                Stmt::VarDecl {
                    name,
                    ty,
                    value,
                    span,
                    ..
                } => self.bind(name, ty.as_ref(), value, *span, true, depth)?,
                Stmt::Return {
                    value: Some(value), ..
                } => {
                    let result = self.expression(value, depth)?;
                    self.exit_result(result)?;
                }
                Stmt::Fail { value, span } => {
                    if self.handlers.is_empty() {
                        if let ExprKind::Literal(Literal::String(message)) = &value.kind {
                            self.budget.text(message.len(), *span)?;
                            let result = self.constant(
                                TaskConstant::Err {
                                    result: self.function.result.clone(),
                                    domain: "ku".into(),
                                    code: "fail".into(),
                                    message: message.clone(),
                                },
                                self.function.result.clone(),
                            )?;
                            self.exit_result(result)?;
                            continue;
                        }
                    }
                    let source = self.expression(value, depth)?;
                    let error = if self.function.slots[source.0].ty == value_slot(IrType::Str) {
                        let error = self.slot(value_slot(super::error_ir_type()))?;
                        self.emit(TaskOp::MakeError {
                            dst: error,
                            message: source,
                        })?;
                        error
                    } else if matches!(&self.function.slots[source.0].ty,
                        TaskSlotType::Value { ty, .. } if task::error_type(ty))
                    {
                        source
                    } else {
                        return Err(unsupported(
                            "fail of a value other than str or Error",
                            *span,
                        ));
                    };
                    self.raise_error(error)?;
                }
                Stmt::Print { value, .. } => {
                    let temporary = !matches!(&value.kind, ExprKind::Variable(_));
                    let slot = self.expression(value, depth)?;
                    self.emit(TaskOp::Print {
                        value: slot,
                        newline: false,
                    })?;
                    if temporary && owned_value(&self.function.slots[slot.0].ty) {
                        self.emit(TaskOp::Drop { slot })?;
                    }
                }
                Stmt::Expr { expr, .. } => {
                    let value = self.expression(expr, depth)?;
                    if matches!(&self.function.slots[value.0].ty, TaskSlotType::Task { .. }) {
                        return Err(unsupported("discarding a Task temporary before function-scope cleanup; bind it to a local", expr.span));
                    } else if owned_value(&self.function.slots[value.0].ty)
                        && !matches!(&expr.kind, ExprKind::Variable(_))
                    {
                        self.emit(TaskOp::Drop { slot: value })?;
                    } else {
                        self.emit(TaskOp::Read { slot: value })?;
                    }
                }
                _ => return Err(unsupported("this statement in a native Task", self.span)),
            }
        }
        self.statement_depth = previous_depth;
        Ok(())
    }

    fn drain_scope(&mut self, scope: TaskScopeId) -> KuResult<()> {
        let ready = self.state(TaskTerminator::Terminate)?;
        let cleanup = self.cleanup_state()?;
        self.terminate(TaskTerminator::ScopeDrain {
            scope,
            ready,
            cleanup,
        })?;
        self.budget.analysis(1, self.span)?;
        self.scope_drop_states.push((ready, scope));
        self.current = Some(ready);
        Ok(())
    }

    fn lower_loop_exit(&mut self, is_continue: bool, span: Span) -> KuResult<()> {
        if self.in_finally_body && !self.protected {
            self.discard_finally_primaries()?;
        }
        if self.protected {
            self.terminate(TaskTerminator::Jump {
                target: self
                    .protected_finish
                    .expect("protected finally continuation"),
            })?;
            return Ok(());
        }
        let context = self
            .loops
            .last_mut()
            .ok_or_else(|| unsupported("break or continue outside a loop", span))?;
        context.has_continue |= is_continue;
        let retained_depth = context.retained_depth;
        let finally_depth = context.finally_depth;
        let target = if is_continue {
            context.continue_target
        } else {
            context.break_target
        };
        if self.finalies.len() > finally_depth || self.unwinding {
            self.lower_finally_exit(
                FinallyExit::Jump(target),
                finally_depth,
                retained_depth,
                self.statement_depth,
            )
        } else {
            self.lower_scoped_jump(target, retained_depth)
        }
    }

    fn lower_scoped_jump(&mut self, target: StateId, retained_depth: usize) -> KuResult<()> {
        if retained_depth == self.scopes.len() {
            self.terminate(TaskTerminator::Jump { target })?;
            return Ok(());
        }
        let exit = TaskScopeExitId(self.next_scope_exit);
        self.next_scope_exit += 1; // Each ID consumes two bounded states.
        let begin = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: begin })?;
        self.current = Some(begin);
        self.emit(TaskOp::ScopeExitBegin {
            exit,
            retain: retained_depth.checked_sub(1).map(|i| self.scopes[i]),
        })?;
        let first_drain = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump {
            target: first_drain,
        })?;
        self.current = Some(first_drain);
        for index in (retained_depth..self.scopes.len()).rev() {
            self.drain_scope(self.scopes[index])?;
        }
        let end = self.state(TaskTerminator::Jump { target })?;
        self.terminate(TaskTerminator::Jump { target: end })?;
        self.emit_at(end, TaskOp::ScopeExitEnd { exit })?;
        // The compiler's lexical stack remains active for the other branch;
        // this generated path alone has drained down to the loop's parent.
        Ok(())
    }

    fn lower_finally_exit(
        &mut self,
        action: FinallyExit,
        retain_finally: usize,
        retain_scopes: usize,
        depth: usize,
    ) -> KuResult<()> {
        let saved_finalies = self.finalies.clone();
        let saved_scopes = self.scopes.clone();
        let names: usize = self.locals.iter().map(|scope| scope.len()).sum();
        let name_bytes = self
            .locals
            .iter()
            .flat_map(|scope| scope.keys())
            .try_fold(0usize, |sum, name| sum.checked_add(name.len()))
            .ok_or_else(|| unsupported("local name clone budget overflow", self.span))?;
        self.budget.analysis(
            names + self.finalies.len() + self.scopes.len() + self.handlers.len(),
            self.span,
        )?;
        self.budget.text(name_bytes, self.span)?;
        let saved_locals = self.locals.clone();
        let saved_handlers = self.handlers.clone();
        let saved_unwinding = self.unwinding;
        let saved_body = self.in_finally_body;
        let saved_handler_depth = self.finally_body_handler_depth;
        if !self.unwinding {
            self.marker(TaskOp::FinallyUnwindBegin)?;
        }
        self.unwinding = true;
        while self.finalies.len() > retain_finally && self.current.is_some() {
            let context = *self.finalies.last().expect("active finally");
            let ready = self.state(TaskTerminator::Terminate)?;
            self.lower_scoped_jump(ready, context.scope_depth)?;
            self.current = Some(ready);
            self.finalies.pop().expect("drained current finally");
            self.scopes.truncate(context.scope_depth);
            self.locals.truncate(context.locals_depth);
            self.handlers.truncate(context.handlers_depth);
            self.marker(TaskOp::FinallyLeave {
                cleanup: context.cleanup,
            })?;
            self.in_finally_body = true;
            self.finally_body_handler_depth = self.handlers.len();
            let entry = self.current.take().expect("finally entry");
            self.current = self.lower_arm(context.body, entry, depth + 1)?;
            self.in_finally_body = saved_body;
        }
        if self.current.is_some() {
            let ready = self.state(TaskTerminator::Terminate)?;
            self.lower_scoped_jump(ready, retain_scopes)?;
            self.current = Some(ready);
            match action {
                FinallyExit::Return(value) => {
                    self.terminate(TaskTerminator::Exit { value })?;
                }
                FinallyExit::Jump(target) => {
                    if let Some(handler) = self
                        .handlers
                        .iter_mut()
                        .find(|handler| handler.target == target)
                    {
                        handler.used = true;
                    }
                    self.marker(TaskOp::FinallyUnwindEnd)?;
                    self.terminate(TaskTerminator::Jump { target })?;
                }
            }
        }
        let used: Vec<_> = self.handlers.iter().map(|handler| handler.used).collect();
        self.handlers = saved_handlers;
        for (handler, used) in self.handlers.iter_mut().zip(used) {
            handler.used |= used;
        }
        self.finalies = saved_finalies;
        self.scopes = saved_scopes;
        self.locals = saved_locals;
        self.unwinding = saved_unwinding;
        self.in_finally_body = saved_body;
        self.finally_body_handler_depth = saved_handler_depth;
        Ok(())
    }

    fn lower_try(
        &mut self,
        body: &'a [Stmt],
        catch_name: Option<&str>,
        catch_body: &'a [Stmt],
        finally_body: &'a [Stmt],
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        if depth >= 32 {
            return Err(unsupported("statement nesting beyond 32 Task levels", span));
        }
        if self.in_finally_body && !finally_body.is_empty() {
            return Err(unsupported(
                "a nested finally inside a native Task finally body",
                span,
            ));
        }
        let finally = if finally_body.is_empty() {
            None
        } else {
            let cleanup = self.state(TaskTerminator::Terminate)?;
            let context = FinallyContext {
                body: finally_body,
                cleanup,
                locals_depth: self.locals.len(),
                scope_depth: self.scopes.len(),
                handlers_depth: self.handlers.len(),
            };
            self.marker(TaskOp::FinallyEnter { cleanup })?;
            self.finalies.push(context);
            Some(context)
        };
        let has_catch = catch_name.is_some() || !catch_body.is_empty();
        let body_entry = self.state(TaskTerminator::Terminate)?;
        let catch_entry = self.state(TaskTerminator::Terminate)?;
        let after = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: body_entry })?;
        let error = if has_catch {
            let error = self.slot(value_slot(super::error_ir_type()))?;
            self.handlers.push(ErrorHandler {
                target: catch_entry,
                error,
                retained_depth: self.scopes.len(),
                used: false,
                finally_depth: self.finalies.len(),
            });
            Some(error)
        } else {
            None
        };
        let body_end = self.lower_arm(body, body_entry, depth + 1)?;
        let used = if has_catch {
            self.handlers
                .pop()
                .expect("active recoverable handler")
                .used
        } else {
            false
        };
        if let Some(end) = body_end {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: after };
        }
        let catch_end = if used {
            self.lower_bound_arm(catch_body, catch_entry, depth + 1, catch_name.zip(error))?
        } else {
            None
        };
        if let Some(end) = catch_end {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: after };
        }
        if used && catch_name.is_none() {
            if let Some(error) = error {
                self.emit_at(catch_entry, TaskOp::DropIfInit { slot: error })?;
            }
        }
        self.current = (body_end.is_some() || catch_end.is_some()).then_some(after);
        if let Some(context) = finally {
            self.finalies.pop().expect("current finally");
            if self.current.is_some() {
                self.marker(TaskOp::FinallyLeave {
                    cleanup: context.cleanup,
                })?;
                let entry = self.current.take().expect("ordinary finally entry");
                let saved_body = self.in_finally_body;
                let saved_handler_depth = self.finally_body_handler_depth;
                self.in_finally_body = true;
                self.finally_body_handler_depth = self.handlers.len();
                self.current = self.lower_arm(context.body, entry, depth + 1)?;
                self.in_finally_body = saved_body;
                self.finally_body_handler_depth = saved_handler_depth;
            }
            self.lower_protected_finally(context, depth + 1)?;
        }
        Ok(())
    }

    fn lower_protected_finally(
        &mut self,
        context: FinallyContext<'a>,
        depth: usize,
    ) -> KuResult<()> {
        let saved_current = self.current;
        let saved_protected = self.protected;
        let saved_finish = self.protected_finish;
        let saved_body = self.in_finally_body;
        let saved_handlers = std::mem::take(&mut self.handlers);
        let finish = self.state(TaskTerminator::Terminate)?;
        self.protected = true;
        self.in_finally_body = true;
        self.protected_finish = Some(finish);
        let first_slot = self.function.slots.len();
        self.current = Some(context.cleanup);
        let end = self.lower_arm(context.body, context.cleanup, depth)?;
        if let Some(end) = end {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: finish };
        }
        self.current = Some(finish);
        let outer = self.finalies.last().map(|context| context.cleanup);
        if outer.is_some() {
            for index in (first_slot..self.function.slots.len()).rev() {
                if owned_value(&self.function.slots[index].ty) {
                    self.emit(TaskOp::DropIfInit {
                        slot: SlotId(index),
                    })?;
                }
            }
        } else {
            self.cleanup_exits.push(finish);
        }
        if let Some(outer) = outer {
            self.marker(TaskOp::FinallyLeave { cleanup: outer })?;
            self.terminate(TaskTerminator::Jump { target: outer })?;
        } else {
            self.terminate(TaskTerminator::Terminate)?;
        }
        self.handlers = saved_handlers;
        self.current = saved_current;
        self.protected = saved_protected;
        self.protected_finish = saved_finish;
        self.in_finally_body = saved_body;
        Ok(())
    }

    fn lower_for(
        &mut self,
        name: &str,
        iterable: &Expr,
        body: &'a [Stmt],
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        if depth >= 32 {
            return Err(unsupported("statement nesting beyond 32 Task levels", span));
        }
        // Evaluate once in the outer lexical environment. Keep a distinct
        // snapshot even when the expression is an existing mutable local.
        let source = self.expression(iterable, depth + 1)?;
        if matches!(&self.function.slots[source.0].ty,TaskSlotType::Value {ty,..} if task::flat_array(ty))
        {
            return self.lower_array_for(name, iterable, source, body, span, depth);
        }
        if self.function.slots[source.0].ty != value_slot(IrType::Int) {
            return Err(unsupported(
                "for iterables other than int or flat array",
                span,
            ));
        }
        let limit = self.copy_or_move(source)?;
        self.emit(TaskOp::CheckForLimit { slot: limit })?;
        let counter = self.constant(TaskConstant::Int(0), IrType::Int)?;
        let one = self.constant(TaskConstant::Int(1), IrType::Int)?;
        let header = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: header })?;
        self.current = Some(header);
        let condition = self.slot(value_slot(IrType::Bool))?;
        self.emit(TaskOp::Binary {
            dst: condition,
            op: TaskBinaryOp::Less,
            left: counter,
            right: limit,
        })?;
        let body_entry = self.state(TaskTerminator::Terminate)?;
        let after = self.state(TaskTerminator::Terminate)?;
        let latch = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Branch {
            condition,
            then_state: body_entry,
            else_state: after,
        })?;
        self.loops.push(LoopContext {
            break_target: after,
            continue_target: latch,
            retained_depth: self.scopes.len(),
            has_continue: false,
            finally_depth: self.finalies.len(),
        });
        let end = self.lower_bound_arm(body, body_entry, depth + 1, Some((name, counter)))?;
        let context = self.loops.pop().expect("loop context exists");
        if let Some(end) = end {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: latch };
        }
        if end.is_some() || context.has_continue {
            self.current = Some(latch);
            // The lexical body has already drained its Tasks and dropped its
            // owned locals. Reassigning its visible variable never changes
            // this hidden counter or the captured bound. Since counter < limit
            // and limit <= INT64_MAX, this increment cannot overflow.
            let next = self.slot(value_slot(IrType::Int))?;
            self.emit(TaskOp::Binary {
                dst: next,
                op: TaskBinaryOp::Add,
                left: counter,
                right: one,
            })?;
            self.emit(TaskOp::Copy {
                dst: counter,
                src: next,
            })?;
            let cleanup = self.cleanup_state()?;
            self.budget.analysis(1, span)?;
            self.terminate(TaskTerminator::Suspend {
                resume: header,
                cleanup,
            })?;
        }
        self.current = Some(after);
        Ok(())
    }

    fn lower_array_for(
        &mut self,
        name: &str,
        iterable: &Expr,
        source: SlotId,
        body: &'a [Stmt],
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        let TaskSlotType::Value {
            ty: IrType::Array(element),
            ..
        } = &self.function.slots[source.0].ty
        else {
            unreachable!("validated flat array");
        };
        let element = element.as_ref().clone();
        self.budget.analysis(1, span)?;
        let scope = TaskScopeId(self.next_scope);
        self.next_scope += 1;
        self.scopes.push(scope);
        self.locals.push(HashMap::new());
        self.emit(TaskOp::ScopeEnter {
            scope,
            tasks: Vec::new(),
        })?;
        let snapshot = self.slot(self.function.slots[source.0].ty.clone())?;
        self.emit(TaskOp::ArrayClone {
            dst: snapshot,
            src: source,
        })?;
        if !matches!(iterable.kind, ExprKind::Variable(_)) {
            self.emit(TaskOp::Drop { slot: source })?;
        }
        let limit = self.slot(value_slot(IrType::Int))?;
        self.emit(TaskOp::ArrayLen {
            dst: limit,
            src: snapshot,
        })?;
        let counter = self.constant(TaskConstant::Int(0), IrType::Int)?;
        let one = self.constant(TaskConstant::Int(1), IrType::Int)?;
        let header = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: header })?;
        self.current = Some(header);
        let condition = self.slot(value_slot(IrType::Bool))?;
        self.emit(TaskOp::Binary {
            dst: condition,
            op: TaskBinaryOp::Less,
            left: counter,
            right: limit,
        })?;
        let body_entry = self.state(TaskTerminator::Terminate)?;
        let after = self.state(TaskTerminator::Terminate)?;
        let latch = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Branch {
            condition,
            then_state: body_entry,
            else_state: after,
        })?;
        self.current = Some(body_entry);
        let item = self.slot(value_slot(element))?;
        self.emit(TaskOp::ArrayTake {
            dst: item,
            array: snapshot,
            index: counter,
        })?;
        self.loops.push(LoopContext {
            break_target: after,
            continue_target: latch,
            retained_depth: self.scopes.len(),
            has_continue: false,
            finally_depth: self.finalies.len(),
        });
        let end = self.lower_bound_arm(body, body_entry, depth + 1, Some((name, item)))?;
        let context = self.loops.pop().expect("loop context exists");
        if let Some(end) = end {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: latch };
        }
        if end.is_some() || context.has_continue {
            self.current = Some(latch);
            let next = self.slot(value_slot(IrType::Int))?;
            self.emit(TaskOp::Binary {
                dst: next,
                op: TaskBinaryOp::Add,
                left: counter,
                right: one,
            })?;
            self.emit(TaskOp::Copy {
                dst: counter,
                src: next,
            })?;
            let cleanup = self.cleanup_state()?;
            self.budget.analysis(1, span)?;
            self.terminate(TaskTerminator::Suspend {
                resume: header,
                cleanup,
            })?;
        }
        self.current = Some(after);
        self.drain_scope(scope)?;
        self.locals.pop();
        self.scopes.pop();
        Ok(())
    }

    fn lower_while(
        &mut self,
        condition: &Expr,
        body: &'a [Stmt],
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        if depth >= 32 {
            return Err(unsupported("statement nesting beyond 32 Task levels", span));
        }
        // Reenter before evaluating any condition operand, not at an Await
        // ready edge or the final bool Branch. The preheader runs only once.
        let header = self.state(TaskTerminator::Terminate)?;
        self.terminate(TaskTerminator::Jump { target: header })?;
        self.current = Some(header);
        let condition = self.expression(condition, depth + 1)?;
        if self.function.slots[condition.0].ty != value_slot(IrType::Bool) {
            return Err(unsupported("non-bool while conditions", span));
        }
        let body_entry = self.state(TaskTerminator::Terminate)?;
        let after = self.state(TaskTerminator::Terminate)?;
        let latch = self.state(TaskTerminator::Terminate)?;
        // Await, ? and short circuit may have replaced current. Only their
        // actual successful endpoint decides whether this iteration enters.
        self.terminate(TaskTerminator::Branch {
            condition,
            then_state: body_entry,
            else_state: after,
        })?;
        self.loops.push(LoopContext {
            break_target: after,
            continue_target: latch,
            retained_depth: self.scopes.len(),
            has_continue: false,
            finally_depth: self.finalies.len(),
        });
        let end = self.lower_arm(body, body_entry, depth + 1)?;
        let context = self.loops.pop().expect("loop context exists");
        if let Some(end) = end {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: latch };
        }
        if end.is_some() || context.has_continue {
            // A nonempty body's endpoint follows its real ScopeDrain and
            // local Value drops. An empty body still needs this forced yield.
            let cleanup = self.cleanup_state()?;
            self.budget.analysis(1, span)?;
            self.function.states[latch.0].terminator = TaskTerminator::Suspend {
                resume: header,
                cleanup,
            };
        }
        self.current = Some(after);
        Ok(())
    }

    fn lower_if(
        &mut self,
        condition: &Expr,
        then_branch: &'a [Stmt],
        else_branch: &'a [Stmt],
        span: Span,
        depth: usize,
    ) -> KuResult<()> {
        if depth >= 32 {
            return Err(unsupported("statement nesting beyond 32 Task levels", span));
        }
        let condition = self.expression(condition, depth + 1)?;
        if self.function.slots[condition.0].ty != value_slot(IrType::Bool) {
            return Err(unsupported("non-bool if conditions", span));
        }
        let then_state = self.state(TaskTerminator::Terminate)?;
        let else_state = self.state(TaskTerminator::Terminate)?;
        // Condition lowering may have changed current through Await, ? or
        // short circuit. Only its actual endpoint owns this Branch.
        self.terminate(TaskTerminator::Branch {
            condition,
            then_state,
            else_state,
        })?;
        let then_end = self.lower_arm(then_branch, then_state, depth + 1)?;
        let else_end = self.lower_arm(else_branch, else_state, depth + 1)?;
        if then_end.is_none() && else_end.is_none() {
            self.current = None;
            return Ok(());
        }
        let join = self.state(TaskTerminator::Terminate)?;
        for end in [then_end, else_end].into_iter().flatten() {
            self.function.states[end.0].terminator = TaskTerminator::Jump { target: join };
        }
        self.current = Some(join);
        Ok(())
    }

    fn lower_arm(
        &mut self,
        body: &'a [Stmt],
        entry: StateId,
        depth: usize,
    ) -> KuResult<Option<StateId>> {
        self.lower_bound_arm(body, entry, depth, None)
    }

    fn lower_bound_arm(
        &mut self,
        body: &'a [Stmt],
        entry: StateId,
        depth: usize,
        binding: Option<(&str, SlotId)>,
    ) -> KuResult<Option<StateId>> {
        self.current = Some(entry);
        if body.is_empty() && binding.is_none() {
            return Ok(self.current.take());
        }
        if self.protected {
            self.locals.push(HashMap::new());
            if let Some((name, source)) = binding {
                let destination = self.copy_or_move(source)?;
                self.locals
                    .last_mut()
                    .expect("protected body scope")
                    .insert(name.to_owned(), destination);
            }
            self.lower_block(body, depth)?;
            self.locals.pop();
            return Ok(self.current.take());
        }
        // IDs are dense declaration order, not the slot interval of an arm:
        // descendants have their own disjoint ownership mask.
        self.budget.analysis(1, self.span)?;
        let scope = TaskScopeId(self.next_scope);
        self.next_scope += 1; // Bounded by generated states/operations.
        self.scopes.push(scope);
        self.locals.push(HashMap::new());
        let enter_op = self.function.states[entry.0].operations.len();
        self.emit(TaskOp::ScopeEnter {
            scope,
            tasks: Vec::new(),
        })?;
        if let Some((name, source)) = binding {
            self.budget.text(name.len(), self.span)?;
            let destination = self.copy_or_move(source)?;
            self.locals
                .last_mut()
                .expect("body scope exists")
                .insert(name.to_owned(), destination);
        }
        self.lower_block(body, depth)?;
        // Finish membership from already-lowered slot provenance. Reserve work
        // before every possible push; never evaluate a statement a second time.
        let mut tasks = Vec::new();
        self.budget.analysis(self.function.slots.len(), self.span)?;
        for (index, slot) in self.function.slots.iter().enumerate() {
            if self.slot_owners[index] == Some(scope)
                && matches!(slot.ty, TaskSlotType::Task { .. })
            {
                self.budget.analysis(1, self.span)?;
                tasks.push(SlotId(index));
            }
        }
        self.function.states[entry.0].operations[enter_op] = TaskOp::ScopeEnter { scope, tasks };
        if self.current.is_some() {
            self.drain_scope(scope)?;
        }
        self.locals.pop();
        self.scopes.pop();
        Ok(self.current.take())
    }

    fn lower(mut self, declaration: &'a FnDecl) -> KuResult<TaskFunction> {
        self.lower_block(&declaration.body, 0)?;
        if self.current.is_some() {
            return Err(unsupported(
                "an async body without an explicit Result return or fail",
                declaration.span,
            ));
        }
        // A nonlocal exit may precede another arm's local declarations. Finish
        // each lexical drop mask only after all slots and birth scopes exist.
        for (state, scope) in std::mem::take(&mut self.scope_drop_states) {
            let following = std::mem::take(&mut self.function.states[state.0].operations);
            self.budget.analysis(self.function.slots.len(), self.span)?;
            for index in (0..self.function.slots.len()).rev() {
                if self.slot_owners[index] == Some(scope)
                    && owned_value(&self.function.slots[index].ty)
                {
                    self.emit_at(
                        state,
                        TaskOp::DropIfInit {
                            slot: SlotId(index),
                        },
                    )?;
                }
            }
            self.function.states[state.0].operations.extend(following);
        }
        for state in std::mem::take(&mut self.cleanup_exits) {
            for index in (0..self.function.slots.len()).rev() {
                let slot = SlotId(index);
                if owned_value(&self.function.slots[index].ty) {
                    self.emit_at(state, TaskOp::DropIfInit { slot })?;
                }
            }
        }
        Ok(self.function)
    }
}
