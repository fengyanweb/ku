//! Bounded source lowering for the first native Task subset.
//!
//! This is separate from synchronous IR: no async flag is erased, and every
//! unsupported construct is rejected before an artifact can be written.

use std::collections::HashMap;

use crate::{
    ast::{
        BinaryOp, Expr, ExprKind, FnDecl, Item, Literal, ParamMode, Program, Stmt, TypeName,
        UnaryOp,
    },
    error::{KuError, KuResult},
    span::Span,
};

use super::{
    task::{
        self, SlotId, StateId, TaskBinaryOp, TaskConstant, TaskFunction, TaskFunctionId,
        TaskLimits, TaskOp, TaskProgram, TaskScopeId, TaskSlot, TaskSlotType, TaskState,
        TaskTerminator, TaskUnaryOp,
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
    parameters: Vec<IrType>,
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

fn value_type(ty: &TypeName, span: Span) -> KuResult<IrType> {
    if let Some(ty) = primitive(ty) {
        return Ok(ty);
    }
    if let TypeName::Result(inner) = ty {
        if let Some(inner) = primitive(inner) {
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
    matches!(
        ty,
        TaskSlotType::Value {
            ty: IrType::Str | IrType::Result(_),
            ..
        }
    )
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
    if program.items.len() > limits.max_functions {
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
    for item in &program.items {
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
                "an async function without an explicit primitive Result return type",
                function.span,
            ));
        };
        let result =
            IrType::Result(Box::new(primitive(inner).ok_or_else(|| {
                unsupported("this async Result payload", function.span)
            })?));
        let mut parameters = Vec::new();
        for parameter in &function.params {
            if parameter.mode != ParamMode::Owned {
                return Err(unsupported("borrowed async parameters", parameter.span));
            }
            budget.text(parameter.name.len(), parameter.span)?;
            parameters.push(value_type(
                parameter
                    .ty
                    .as_ref()
                    .ok_or_else(|| unsupported("untyped async parameters", parameter.span))?,
                parameter.span,
            )?);
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
        functions
            .push(FunctionLowerer::new(declaration, &signatures, &mut budget)?.lower(declaration)?);
    }
    let tasks = TaskProgram { functions };
    task::verify_and_plan(&tasks, limits)?;
    Ok(NativeTaskProgram { tasks, entry })
}

struct FunctionLowerer<'a> {
    signatures: &'a HashMap<String, Signature>,
    budget: &'a mut Budget,
    function: TaskFunction,
    locals: Vec<HashMap<String, SlotId>>,
    // ROOT is None. Slots never change their lexical birth scope; Task moves
    // across these owners are deliberately outside this first scoped subset.
    slot_owners: [Option<TaskScopeId>; 64],
    scopes: Vec<TaskScopeId>,
    next_scope: usize,
    current: Option<StateId>,
    // Await/ScopeDrain cancellation exits receive synthetic Value cleanup. Normal
    // Exit leaves its current owners for generated all-Task handoff/finish glue.
    // Fill these finite cleanup regions after every slot is known.
    cleanup_exits: Vec<StateId>,
    span: Span,
}

impl<'a> FunctionLowerer<'a> {
    fn new(
        declaration: &FnDecl,
        signatures: &'a HashMap<String, Signature>,
        budget: &'a mut Budget,
    ) -> KuResult<Self> {
        let signature = &signatures[&declaration.name];
        let mut lowerer = Self {
            signatures,
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
            current: None,
            cleanup_exits: Vec::new(),
            span: declaration.span,
        };
        lowerer.current = Some(lowerer.state(TaskTerminator::Terminate)?);
        for (parameter, ty) in declaration.params.iter().zip(&signature.parameters) {
            let slot = lowerer.slot(value_slot(ty.clone()))?;
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
        self.terminate(TaskTerminator::Exit { value })?;
        Ok(())
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
            ExprKind::Variable(name) => self.local(name).ok_or_else(|| {
                unsupported(
                    "an unknown local or first-class function value",
                    expression.span,
                )
            }),
            ExprKind::Call { callee, args } => {
                let ExprKind::Variable(name) = &callee.kind else {
                    return Err(unsupported("indirect or method calls", expression.span));
                };
                if self.local(name).is_some() {
                    return Err(unsupported("a call through a local value", expression.span));
                }
                // The ordinary checker resolves declared functions before
                // builtin fallback. Native lowering must preserve that choice.
                let declared_function = self.signatures.contains_key(name);
                if !declared_function && name == "ok" {
                    if args.len() != 1 {
                        return Err(unsupported(
                            "ok with other than one argument",
                            expression.span,
                        ));
                    }
                    let source = self.expression(&args[0], depth + 1)?;
                    let TaskSlotType::Value { ty, .. } = &self.function.slots[source.0].ty else {
                        return Err(unsupported("Task payloads inside Result", expression.span));
                    };
                    if !matches!(ty, IrType::Int | IrType::Bool | IrType::Null | IrType::Str) {
                        return Err(unsupported("nested Result values", expression.span));
                    }
                    let destination =
                        self.slot(value_slot(IrType::Result(Box::new(ty.clone()))))?;
                    self.emit(TaskOp::WrapOk {
                        dst: destination,
                        src: source,
                    })?;
                    return Ok(destination);
                }
                if !declared_function && (name == "println" || name == "print") {
                    if args.len() != 1 {
                        return Err(unsupported(
                            "print with other than one argument",
                            expression.span,
                        ));
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
                let signature = self.signatures.get(name).cloned().ok_or_else(|| {
                    unsupported(
                        "a call other than a known top-level async function",
                        expression.span,
                    )
                })?;
                if args.len() != signature.parameters.len() || args.len() > 64 {
                    return Err(unsupported(
                        "mismatched async call arguments",
                        expression.span,
                    ));
                }
                let mut arguments = Vec::new();
                for (argument, expected) in args.iter().zip(&signature.parameters) {
                    let source = self.expression(argument, depth + 1)?;
                    if self.function.slots[source.0].ty != value_slot(expected.clone()) {
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
            ExprKind::Await(expression) => {
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
                let cleanup = self.state(TaskTerminator::Terminate)?;
                self.terminate(TaskTerminator::Jump { target: poll })?;
                self.function.states[poll.0].terminator = TaskTerminator::Await {
                    task: hidden,
                    dst: destination,
                    ready,
                    cleanup,
                };
                self.cleanup_exits.push(cleanup);
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
                let error = self.slot(value_slot(self.function.result.clone()))?;
                let ok = self.state(TaskTerminator::Terminate)?;
                let err = self.state(TaskTerminator::Exit { value: error })?;
                self.terminate(TaskTerminator::TryResult {
                    src: source,
                    ok_value: destination,
                    err_result: error,
                    ok,
                    err,
                })?;
                self.current = Some(ok);
                Ok(destination)
            }
            _ => Err(unsupported(
                "this expression in the bounded primitive Task subset",
                expression.span,
            )),
        }
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

    fn bind(
        &mut self,
        name: &str,
        annotation: Option<&TypeName>,
        value: &Expr,
        span: Span,
        declaration: bool,
        depth: usize,
    ) -> KuResult<()> {
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
        let source = self.expression(value, depth)?;
        if let Some(annotation) = annotation {
            if self.function.slots[source.0].ty != value_slot(value_type(annotation, span)?) {
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

    fn lower_block(&mut self, body: &[Stmt], depth: usize) -> KuResult<()> {
        // Charge before traversal; width is not recursion depth. No new AST is
        // cloned, and lower_if separately checks before each recursive descent.
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
                Stmt::Fail {
                    value:
                        Expr {
                            kind: ExprKind::Literal(Literal::String(message)),
                            ..
                        },
                    span,
                } => {
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
                _ => {
                    return Err(unsupported(
                        "this statement; loops and try/catch/finally remain gated",
                        self.span,
                    ))
                }
            }
        }
        Ok(())
    }

    fn lower_if(
        &mut self,
        condition: &Expr,
        then_branch: &[Stmt],
        else_branch: &[Stmt],
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
        body: &[Stmt],
        entry: StateId,
        depth: usize,
    ) -> KuResult<Option<StateId>> {
        self.current = Some(entry);
        if body.is_empty() {
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
            let ready = self.state(TaskTerminator::Terminate)?;
            let cleanup = self.state(TaskTerminator::Terminate)?;
            self.terminate(TaskTerminator::ScopeDrain {
                scope,
                ready,
                cleanup,
            })?;
            self.budget.analysis(1, self.span)?;
            self.cleanup_exits.push(cleanup);
            self.current = Some(ready);
            self.budget.analysis(self.function.slots.len(), self.span)?;
            for index in (0..self.function.slots.len()).rev() {
                if self.slot_owners[index] == Some(scope)
                    && owned_value(&self.function.slots[index].ty)
                {
                    self.emit(TaskOp::DropIfInit {
                        slot: SlotId(index),
                    })?;
                }
            }
        }
        self.locals.pop();
        self.scopes.pop();
        Ok(self.current.take())
    }

    fn lower(mut self, declaration: &FnDecl) -> KuResult<TaskFunction> {
        self.lower_block(&declaration.body, 0)?;
        if self.current.is_some() {
            return Err(unsupported(
                "an async body without an explicit Result return or fail",
                declaration.span,
            ));
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
