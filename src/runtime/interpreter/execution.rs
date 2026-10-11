//! Flat, bounded interpreter call/return continuations.
//!
//! The production slice admits a complete static scalar call tree before any
//! body effects. Once admitted, every nested call stays in this dispatch loop.
//! Other language constructs keep their existing evaluator; Owned, borrows,
//! function values, waiting and finally still require later migration slices.
//! This partial production integration does not close the full P0 contract.

use super::*;
use crate::ast::TypeName;
use storage::{FrameLimits, FrameStack, ReserveError};

mod storage;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
struct Limits {
    calls: FrameLimits,
    work: FrameLimits,
    values: FrameLimits,
    syntax: FrameLimits,
    syntax_steps: usize,
    // Test harness fuel, not a new language execution limit. Production
    // migration must use its existing cancellation/safepoint contract instead.
    prototype_dispatch_steps: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            calls: FrameLimits {
                max_frames: MAX_CALL_DEPTH,
                max_storage_bytes: 128 * 1024,
            },
            work: FrameLimits {
                max_frames: 16 * 1024,
                max_storage_bytes: 2 * 1024 * 1024,
            },
            values: FrameLimits {
                max_frames: 32 * 1024,
                max_storage_bytes: 8 * 1024 * 1024,
            },
            syntax: FrameLimits {
                max_frames: 16 * 1024,
                max_storage_bytes: 1024 * 1024,
            },
            syntax_steps: 1_000_000,
            prototype_dispatch_steps: 1_000_000,
        }
    }
}

#[derive(Debug, Default)]
struct Metrics {
    entered_calls: usize,
    retired_calls: usize,
    peak_calls: usize,
    steps: usize,
    dispatches: usize,
    remaining_calls: usize,
    remaining_work: usize,
    remaining_values: usize,
    logical_depth: usize,
    // Execution Vec backing only: calls/work/values, excluding the separate
    // preflight syntax stack, source, Env/string keys, allocator overhead and
    // Value payloads. Summed per-stack peaks are not a simultaneous RSS sample.
    retained_backing_bytes: usize,
    peak_backing_bytes_upper_bound: usize,
}

struct CallFrame {
    env: Env,
    depth: usize,
    work_base: usize,
    value_base: usize,
}

enum Work<'a> {
    Block {
        body: &'a [Stmt],
        next: usize,
    },
    Statement(&'a Stmt),
    Eval(&'a Expr),
    Unary(UnaryOp, Span),
    BinaryLeft(BinaryOp, &'a Expr, Span),
    BinaryRight(BinaryOp, Span),
    Bool(Span),
    Choose(&'a [Stmt], &'a [Stmt], Span),
    Bind(&'a str, bool, bool, Span),
    Discard,
    Return,
    Fallthrough,
    Arguments {
        function: &'a FnDecl,
        args: &'a [Expr],
        next: usize,
        base: usize,
        span: Span,
    },
}

enum Syntax<'a> {
    Function(&'a FnDecl),
    Statement(&'a Stmt),
    Expression(&'a Expr),
}

struct Machine<'a> {
    source: &'a HashMap<String, FnDecl>,
    calls: FrameStack<CallFrame>,
    work: FrameStack<Work<'a>>,
    values: FrameStack<Value>,
    initial_depth: usize,
    call_depth: usize,
    remaining_dispatches: usize,
    metrics: Metrics,
}

fn storage_error(error: ReserveError, span: Span) -> KuError {
    // Prototype-only diagnostics. Existing runtime diagnoses are reused only
    // for already-defined semantics, e.g. call_depth_exceeded below.
    KuError::runtime(
        format!("internal execution storage rejected: {error:?}"),
        span,
    )
}

fn unsupported(span: Span) -> KuError {
    KuError::runtime(
        "internal execution prototype does not support this source",
        span,
    )
}

fn reserve<T>(stack: &mut FrameStack<T>, count: usize, span: Span) -> KuResult<()> {
    stack
        .reserve(count)
        .map_err(|error| storage_error(error, span))
}

fn push<T>(stack: &mut FrameStack<T>, value: T) {
    // Callers reserve before transferring ownership. A violation is an
    // implementation defect, never a fallback to the recursive evaluator.
    if stack.push_reserved(value).is_err() {
        unreachable!("continuation storage was reserved before transfer");
    }
}

fn scalar(value: &Value, span: Span) -> KuResult<Value> {
    match value {
        Value::Int(value) => Ok(Value::Int(*value)),
        Value::Float(value) => Ok(Value::Float(*value)),
        Value::Bool(value) => Ok(Value::Bool(*value)),
        Value::Null => Ok(Value::Null),
        _ => Err(unsupported(span)),
    }
}

fn scalar_type(ty: &Option<TypeName>) -> bool {
    matches!(
        ty,
        None | Some(TypeName::Int | TypeName::Float | TypeName::Bool | TypeName::Null)
    )
}

// The source walk itself is bounded and flat; no recursive preflight merely
// moves the stack-overflow problem ahead of the execution loop.
#[cfg(test)]
fn preflight(source: &HashMap<String, FnDecl>, limits: Limits) -> KuResult<()> {
    preflight_tree(source, limits, None)
}

fn preflight_tree(
    source: &HashMap<String, FnDecl>,
    limits: Limits,
    entry: Option<&FnDecl>,
) -> KuResult<()> {
    let mut pending = FrameStack::new(limits.syntax);
    let mut checked = FrameStack::new(limits.syntax);
    let mut remaining = limits.syntax_steps;
    if let Some(function) = entry {
        reserve(&mut pending, 1, function.span)?;
        push(&mut pending, Syntax::Function(function));
    } else {
        reserve(&mut pending, source.len(), Span::default())?;
        for function in source.values() {
            push(&mut pending, Syntax::Function(function));
        }
    }
    while let Some(node) = pending.pop() {
        remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| unsupported(Span::default()))?;
        match node {
            Syntax::Function(function) => {
                if (0..checked.len())
                    .any(|index| checked.get(index) == Some(&function.name.as_str()))
                {
                    continue;
                }
                remaining = remaining
                    .checked_sub(function.params.len())
                    .ok_or_else(|| unsupported(function.span))?;
                if function.is_async
                    || !function.type_params.is_empty()
                    || !scalar_type(&function.return_type)
                    || function
                        .params
                        .iter()
                        .any(|param| param.mode != ParamMode::Owned || !scalar_type(&param.ty))
                {
                    return Err(unsupported(function.span));
                }
                reserve(&mut checked, 1, function.span)?;
                push(&mut checked, function.name.as_str());
                reserve(&mut pending, function.body.len(), function.span)?;
                for stmt in function.body.iter().rev() {
                    push(&mut pending, Syntax::Statement(stmt));
                }
            }
            Syntax::Statement(stmt) => match stmt {
                Stmt::VarDecl {
                    value, ty, span, ..
                } => {
                    if !scalar_type(ty) {
                        return Err(unsupported(*span));
                    }
                    reserve(&mut pending, 1, *span)?;
                    push(&mut pending, Syntax::Expression(value));
                }
                Stmt::Assign { value, span, .. } | Stmt::Expr { expr: value, span } => {
                    reserve(&mut pending, 1, *span)?;
                    push(&mut pending, Syntax::Expression(value));
                }
                Stmt::Return { value, span } => {
                    if let Some(value) = value {
                        reserve(&mut pending, 1, *span)?;
                        push(&mut pending, Syntax::Expression(value));
                    }
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    span,
                } => {
                    let count = then_branch
                        .len()
                        .checked_add(else_branch.len())
                        .and_then(|n| n.checked_add(1))
                        .ok_or_else(|| unsupported(*span))?;
                    reserve(&mut pending, count, *span)?;
                    for stmt in then_branch.iter().chain(else_branch).rev() {
                        push(&mut pending, Syntax::Statement(stmt));
                    }
                    push(&mut pending, Syntax::Expression(condition));
                }
                _ => return Err(unsupported(stmt_span(stmt))),
            },
            Syntax::Expression(expr) => match &expr.kind {
                ExprKind::Literal(
                    Literal::Int(_) | Literal::Float(_) | Literal::Bool(_) | Literal::Null,
                ) => {}
                ExprKind::Variable(name) => {
                    // A bare global function is a first-class value in the
                    // existing evaluator, not a scalar variable. Leave the
                    // entire call tree there until function values migrate.
                    if entry.is_some() && source.contains_key(name) {
                        return Err(unsupported(expr.span));
                    }
                }
                ExprKind::Unary { expr, .. } => {
                    reserve(&mut pending, 1, expr.span)?;
                    push(&mut pending, Syntax::Expression(expr));
                }
                ExprKind::Binary { left, right, .. } => {
                    reserve(&mut pending, 2, expr.span)?;
                    push(&mut pending, Syntax::Expression(right));
                    push(&mut pending, Syntax::Expression(left));
                }
                ExprKind::Call { callee, args } => {
                    let ExprKind::Variable(name) = &callee.kind else {
                        return Err(unsupported(callee.span));
                    };
                    let function = source.get(name).ok_or_else(|| unsupported(callee.span))?;
                    reserve(
                        &mut pending,
                        args.len()
                            .checked_add(1)
                            .ok_or_else(|| unsupported(expr.span))?,
                        expr.span,
                    )?;
                    push(&mut pending, Syntax::Function(function));
                    for arg in args.iter().rev() {
                        push(&mut pending, Syntax::Expression(arg));
                    }
                }
                _ => return Err(unsupported(expr.span)),
            },
        }
    }
    Ok(())
}

impl<'a> Machine<'a> {
    fn new(source: &'a HashMap<String, FnDecl>, limits: Limits, initial_depth: usize) -> Self {
        Self {
            source,
            calls: FrameStack::new(limits.calls),
            work: FrameStack::new(limits.work),
            values: FrameStack::new(limits.values),
            initial_depth,
            call_depth: initial_depth,
            remaining_dispatches: limits.prototype_dispatch_steps,
            metrics: Metrics::default(),
        }
    }

    fn frame(&mut self) -> &mut CallFrame {
        let index = self.calls.len() - 1;
        self.calls.get_mut(index).expect("current call frame")
    }

    fn value(&mut self) -> Value {
        self.values.pop().expect("completed operand")
    }

    fn emit(&mut self, value: Value, span: Span) -> KuResult<()> {
        reserve(&mut self.values, 1, span)?;
        push(&mut self.values, value);
        Ok(())
    }

    fn enter(
        &mut self,
        function: &'a FnDecl,
        base: usize,
        depth: usize,
        span: Span,
    ) -> KuResult<()> {
        // Same two checks and same admission point as named call_function_flow.
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "runtime",
                "call_depth_exceeded",
                format!("maximum call depth exceeded: {MAX_CALL_DEPTH}"),
                span,
            ));
        }
        if self.values.len() - base != function.params.len() {
            return Err(KuError::runtime(
                format!(
                    "function '{}' expects {} arguments but got {}",
                    function.name,
                    function.params.len(),
                    self.values.len() - base
                ),
                function.span,
            ));
        }
        reserve(&mut self.calls, 1, span)?;
        reserve(&mut self.work, 2, span)?;
        // A zero-argument return also needs an operand slot. Obtain it before
        // entering the call so unwinding never has to allocate its result slot.
        let return_slot = base
            .checked_add(1)
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?
            .saturating_sub(self.values.len());
        reserve(&mut self.values, return_slot, span)?;
        let mut env = Env::new();
        for (index, param) in function.params.iter().enumerate() {
            let value = std::mem::replace(
                self.values.get_mut(base + index).expect("argument slot"),
                Value::Null,
            );
            env.define_parameter(param.name.clone(), value, false, param.span)?;
        }
        while self.values.len() > base {
            self.values.pop();
        }
        env.push_scope();
        push(
            &mut self.calls,
            CallFrame {
                env,
                depth,
                work_base: self.work.len(),
                value_base: base,
            },
        );
        self.call_depth += 1;
        self.metrics.entered_calls += 1;
        self.metrics.peak_calls = self.metrics.peak_calls.max(self.calls.len());
        push(&mut self.work, Work::Fallthrough);
        push(
            &mut self.work,
            Work::Block {
                body: &function.body,
                next: 0,
            },
        );
        Ok(())
    }

    fn leave(&mut self, value: Value) {
        let frame = self.calls.pop().expect("return has an active call");
        while self.work.len() > frame.work_base {
            self.work.pop();
        }
        while self.values.len() > frame.value_base {
            self.values.pop();
        }
        // Only scalar envs are admitted in this prototype. No task/lease/owned
        // cleanup is claimed here; those require explicit unwind continuations.
        drop(frame);
        self.call_depth -= 1;
        self.metrics.retired_calls += 1;
        push(&mut self.values, value);
    }

    fn step(&mut self, step: Work<'a>) -> KuResult<()> {
        match step {
            Work::Block { body, next } => {
                if let Some(stmt) = body.get(next) {
                    reserve(&mut self.work, 2, stmt_span(stmt))?;
                    push(
                        &mut self.work,
                        Work::Block {
                            body,
                            next: next + 1,
                        },
                    );
                    push(&mut self.work, Work::Statement(stmt));
                } else {
                    self.frame().env.pop_scope();
                }
            }
            Work::Statement(stmt) => {
                self.metrics.steps += 1;
                reserve(&mut self.work, 2, stmt_span(stmt))?;
                match stmt {
                    Stmt::VarDecl {
                        name,
                        value,
                        mutable,
                        span,
                        ..
                    } => {
                        push(
                            &mut self.work,
                            Work::Bind(name, *mutable && !is_constant_name(name), true, *span),
                        );
                        push(&mut self.work, Work::Eval(value));
                    }
                    Stmt::Assign { name, value, span } => {
                        push(
                            &mut self.work,
                            Work::Bind(name, !is_constant_name(name), false, *span),
                        );
                        push(&mut self.work, Work::Eval(value));
                    }
                    Stmt::If {
                        condition,
                        then_branch,
                        else_branch,
                        span,
                    } => {
                        push(
                            &mut self.work,
                            Work::Choose(then_branch, else_branch, *span),
                        );
                        push(&mut self.work, Work::Eval(condition));
                    }
                    Stmt::Return { value, span } => {
                        push(&mut self.work, Work::Return);
                        if let Some(value) = value {
                            push(&mut self.work, Work::Eval(value));
                        } else {
                            self.emit(Value::Null, *span)?;
                        }
                    }
                    Stmt::Expr { expr, .. } => {
                        push(&mut self.work, Work::Discard);
                        push(&mut self.work, Work::Eval(expr));
                    }
                    _ => unreachable!("preflight rejected unsupported statement"),
                }
            }
            Work::Eval(expr) => {
                self.metrics.steps += 1;
                match &expr.kind {
                    ExprKind::Literal(Literal::Int(value)) => {
                        self.emit(Value::Int(*value), expr.span)?
                    }
                    ExprKind::Literal(Literal::Float(value)) => {
                        self.emit(Value::Float(*value), expr.span)?
                    }
                    ExprKind::Literal(Literal::Bool(value)) => {
                        self.emit(Value::Bool(*value), expr.span)?
                    }
                    ExprKind::Literal(Literal::Null) => self.emit(Value::Null, expr.span)?,
                    ExprKind::Variable(name) => {
                        let value = self.frame().env.get(name, expr.span)?;
                        self.emit(scalar(&value, expr.span)?, expr.span)?;
                    }
                    ExprKind::Unary { op, expr: operand } => {
                        reserve(&mut self.work, 2, expr.span)?;
                        push(&mut self.work, Work::Unary(*op, expr.span));
                        push(&mut self.work, Work::Eval(operand));
                    }
                    ExprKind::Binary { left, op, right } => {
                        reserve(&mut self.work, 2, expr.span)?;
                        push(&mut self.work, Work::BinaryLeft(*op, right, expr.span));
                        push(&mut self.work, Work::Eval(left));
                    }
                    ExprKind::Call { callee, args } => {
                        let ExprKind::Variable(name) = &callee.kind else {
                            unreachable!("preflight");
                        };
                        let function = self.source.get(name).expect("preflight checked callee");
                        reserve(&mut self.work, 1, expr.span)?;
                        push(
                            &mut self.work,
                            Work::Arguments {
                                function,
                                args,
                                next: 0,
                                base: self.values.len(),
                                span: expr.span,
                            },
                        );
                    }
                    _ => unreachable!("preflight rejected unsupported expression"),
                }
            }
            Work::Unary(op, span) => {
                let value = match (op, self.value()) {
                    (UnaryOp::Negate, Value::Int(value)) => value
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| KuError::runtime("integer overflow", span))?,
                    (UnaryOp::Negate, Value::Float(value)) => Value::Float(-value),
                    (UnaryOp::Not, Value::Bool(value)) => Value::Bool(!value),
                    (_, value) => {
                        return Err(KuError::runtime(
                            format!("invalid unary operation for {}", value.type_name()),
                            span,
                        ))
                    }
                };
                self.emit(value, span)?;
            }
            Work::BinaryLeft(op, right, span) => {
                reserve(&mut self.work, 2, span)?;
                if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    let left = expect_bool_condition(self.value(), span)?;
                    if (op == BinaryOp::And && !left) || (op == BinaryOp::Or && left) {
                        self.emit(Value::Bool(left), span)?;
                    } else {
                        push(&mut self.work, Work::Bool(span));
                        push(&mut self.work, Work::Eval(right));
                    }
                } else {
                    push(&mut self.work, Work::BinaryRight(op, span));
                    push(&mut self.work, Work::Eval(right));
                }
            }
            Work::BinaryRight(op, span) => {
                let right = self.value();
                let left = self.value();
                self.emit(eval_binary(op, left, right, span)?, span)?;
            }
            Work::Bool(span) => {
                let value = expect_bool_condition(self.value(), span)?;
                self.emit(Value::Bool(value), span)?;
            }
            Work::Choose(then_body, else_body, span) => {
                let selected = if expect_bool_condition(self.value(), span)? {
                    then_body
                } else {
                    else_body
                };
                reserve(&mut self.work, 1, span)?;
                self.frame().env.push_scope();
                push(
                    &mut self.work,
                    Work::Block {
                        body: selected,
                        next: 0,
                    },
                );
            }
            Work::Bind(name, mutable, declare, span) => {
                let value = self.value();
                let env = &mut self.frame().env;
                if !declare && env.contains(name) {
                    env.assign_owned(name, value, span)?;
                } else {
                    env.define_owned(name.to_string(), value, mutable, span)?;
                }
            }
            Work::Discard => {
                self.value();
            }
            Work::Return => {
                let value = self.value();
                self.leave(value);
            }
            Work::Fallthrough => self.leave(Value::Null),
            Work::Arguments {
                function,
                args,
                next,
                base,
                span,
            } => {
                if let Some(arg) = args.get(next) {
                    reserve(&mut self.work, 2, span)?;
                    push(
                        &mut self.work,
                        Work::Arguments {
                            function,
                            args,
                            next: next + 1,
                            base,
                            span,
                        },
                    );
                    push(&mut self.work, Work::Eval(arg));
                } else {
                    let depth = self.frame().depth + 1;
                    self.enter(function, base, depth, span)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn run(&mut self, entry: &str, args: &[Value], depth: usize) -> KuResult<Value> {
        self.run_with_poll(entry, args, depth, true, |_| Ok(()))
    }

    fn run_with_poll(
        &mut self,
        entry: &str,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
        mut poll: impl FnMut(Span) -> KuResult<()>,
    ) -> KuResult<Value> {
        let result = (|| {
            let function = self.source.get(entry).ok_or_else(|| {
                KuError::runtime(format!("undefined function '{entry}'"), Span::default())
            })?;
            reserve(&mut self.values, args.len(), function.span)?;
            for arg in args {
                scalar(arg, function.span)?;
            }
            for arg in args {
                push(&mut self.values, scalar(arg, function.span)?);
            }
            self.enter(function, 0, depth, function.span)?;
            // The sole dispatch loop: call entry/return only modify these flat
            // stacks; no recursive evaluator or child driver is invoked.
            while let Some(step) = self.work.pop() {
                if test_fuel {
                    self.remaining_dispatches =
                        self.remaining_dispatches.checked_sub(1).ok_or_else(|| {
                            KuError::runtime(
                                "internal execution prototype test fuel exhausted",
                                function.span,
                            )
                        })?;
                }
                match &step {
                    Work::Statement(stmt) => poll(stmt_span(stmt))?,
                    Work::Eval(expr) => poll(expr.span)?,
                    _ => {}
                }
                self.metrics.dispatches += 1;
                self.step(step)?;
            }
            assert_eq!(self.calls.len(), 0);
            assert_eq!(self.values.len(), 1);
            Ok(self.value())
        })();
        // Scalar-only rollback needs no allocations and no recursive frame
        // links. Production ownership cleanup is explicitly not wired here.
        while self.work.pop().is_some() {}
        while self.values.pop().is_some() {}
        while let Some(frame) = self.calls.pop() {
            drop(frame);
            self.call_depth -= 1;
            self.metrics.retired_calls += 1;
        }
        self.metrics.remaining_calls = self.calls.len();
        self.metrics.remaining_work = self.work.len();
        self.metrics.remaining_values = self.values.len();
        self.metrics.logical_depth = self.call_depth;
        assert_eq!(self.call_depth, self.initial_depth);
        self.metrics.retained_backing_bytes = self.calls.charged_storage_bytes()
            + self.work.charged_storage_bytes()
            + self.values.charged_storage_bytes();
        self.metrics.peak_backing_bytes_upper_bound = self.calls.peak_storage_bytes()
            + self.work.peak_storage_bytes()
            + self.values.peak_storage_bytes();
        result
    }
}

#[cfg(test)]
fn execute(
    source: &HashMap<String, FnDecl>,
    entry: &str,
    args: &[Value],
    depth: usize,
    initial_depth: usize,
    limits: Limits,
) -> (KuResult<Value>, Metrics) {
    if let Err(error) = preflight(source, limits) {
        return (
            Err(error),
            Metrics {
                logical_depth: initial_depth,
                ..Metrics::default()
            },
        );
    }
    let mut machine = Machine::new(source, limits, initial_depth);
    let result = machine.run(entry, args, depth);
    (result, machine.metrics)
}

pub(super) fn try_execute(
    interpreter: &mut Interpreter,
    function: &FnDecl,
    args: &[Value],
    depth: usize,
) -> Option<KuResult<Value>> {
    if args
        .iter()
        .any(|value| scalar(value, function.span).is_err())
    {
        return None;
    }
    let source = interpreter.functions.clone();
    let limits = Limits::default();
    // Selection has no user effects. A failed eligibility check preserves the
    // complete old evaluator for this tree; there is no mid-execution fallback.
    if preflight_tree(&source, limits, Some(function)).is_err() {
        return None;
    }
    let mut machine = Machine::new(&source, limits, interpreter.call_depth);
    Some(
        machine.run_with_poll(&function.name, args, depth, false, |span| {
            interpreter.tick(span)
        }),
    )
}
