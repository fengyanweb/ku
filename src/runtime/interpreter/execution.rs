//! Flat, bounded interpreter call/return continuations.
//!
//! The production slice admits a complete static scalar call tree before any
//! body effects. Once admitted, every nested call stays in this dispatch loop.
//! Scope, call, argument and scalar finally exits use the evaluator's real
//! resource arbitration.
//! Other language constructs keep their existing evaluator; Owned, borrows,
//! general function values, waiting and recoverable catch require later slices.
//! This partial production integration does not close the full P0 contract.

use super::*;
use crate::ast::{Param, TypeName};
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
    #[cfg(test)]
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
            #[cfg(test)]
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
    entered_finally_frames: usize,
    retired_finally_frames: usize,
    peak_finally_frames: usize,
    remaining_finally_frames: usize,
    logical_depth: usize,
    // Source execution frame Vec backing only: calls/work/finally/values and
    // staged arguments. This is a frame budget, not a complete memory budget;
    // it excludes preflight syntax storage, source, Env/string keys, allocator
    // overhead and Value/Error payloads. Summed peaks are not a simultaneous RSS
    // sample.
    retained_backing_bytes: usize,
    peak_backing_bytes_upper_bound: usize,
}

#[derive(Clone, Copy)]
enum CallKind {
    Named,
    FunctionValue,
}

impl CallKind {
    fn label(self) -> &'static str {
        match self {
            Self::Named => "function",
            Self::FunctionValue => "function value",
        }
    }
}

/// The evaluator supplies its existing resource and termination arbitration.
/// These callbacks never evaluate Ku syntax or start another execution machine.
trait ExecutionHooks {
    fn tick(&mut self, span: Span) -> KuResult<()>;
    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow>;
    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow>;
    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow>;
    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode;
    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow>;
}

impl ExecutionHooks for Interpreter {
    fn tick(&mut self, span: Span) -> KuResult<()> {
        Interpreter::tick(self, span)
    }

    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        self.finish_owned_scope(env, result, span, pop)
    }

    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        self.finish_call_scope(env, leases, result, span)?
            .into_call_flow(span, kind.label())
    }

    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        self.finish_call_arguments(env, leases, values, result, span)
    }

    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        Interpreter::prepare_finally(self, env, primary, span)
    }

    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        Interpreter::finish_finally(self, primary, replacement, mode, span)
    }
}

#[cfg(test)]
struct PollHooks<P> {
    host: Interpreter,
    poll: P,
}

#[cfg(test)]
impl<P: FnMut(Span) -> KuResult<()>> ExecutionHooks for PollHooks<P> {
    fn tick(&mut self, span: Span) -> KuResult<()> {
        (self.poll)(span)
    }

    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_scope(&mut self.host, env, result, span, pop)
    }

    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_call(&mut self.host, env, leases, result, span, kind)
    }

    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_arguments(&mut self.host, env, leases, values, result, span)
    }

    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        ExecutionHooks::prepare_finally(&mut self.host, env, primary, span)
    }

    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        ExecutionHooks::finish_finally(&mut self.host, primary, replacement, mode, span)
    }
}

struct CallFrame<'a> {
    env: Env,
    leases: Vec<BorrowLease>,
    depth: usize,
    value_base: usize,
    // A borrowed syntax span keeps the admitted frame small. The immutable
    // source tree outlives every frame, including its flat unwind.
    epilogue_span: &'a Span,
    kind: CallKind,
}

#[derive(Clone, Copy)]
enum ScopeFallthrough {
    Continue,
    ReturnNull,
}

struct ArgumentFrame<'a> {
    span: &'a Span,
    value_base: usize,
    leases: Vec<BorrowLease>,
    staged: Vec<Value>,
}

struct FinallyFrame<'a> {
    body: &'a [Stmt],
    span: &'a Span,
    owner_call: usize,
    // Kept out of Work so every continuation does not pay for a large outcome.
    // This is the sole owner of the primary while finally syntax executes.
    primary: Option<KuResult<Flow>>,
    mode: cancellation::FinallyMode,
    // Only this synchronous, no-await slice may hold a TLS guard. Future
    // Pending execution must store context, releasing guards before yielding.
    guard: Option<CleanupGuard>,
}

struct FinallyFrames<'a> {
    frames: FrameStack<FinallyFrame<'a>>,
}

impl Drop for FinallyFrames<'_> {
    fn drop(&mut self) {
        // Restoring nested TLS guards has its own strict order, including a
        // native panic leaving the production-local synchronous Machine. A
        // private caller catching panic while retaining Machine must drop it
        // before expecting TLS restoration; this is not a Pending/yield API.
        // Do not change other FrameStack payload drop contracts.
        while let Some(frame) = self.frames.pop() {
            drop(frame);
        }
    }
}

#[derive(Clone, Copy)]
enum LoopState<'a> {
    While {
        condition: &'a Expr,
        body: &'a [Stmt],
        span: &'a Span,
    },
    ForInt {
        name: &'a str,
        body: &'a [Stmt],
        span: &'a Span,
        current: i64,
        limit: i64,
    },
}

impl LoopState<'_> {
    fn span(self) -> Span {
        match self {
            Self::While { span, .. } | Self::ForInt { span, .. } => *span,
        }
    }
}

enum Work<'a> {
    Block {
        body: &'a [Stmt],
        next: usize,
    },
    Statement(&'a Stmt),
    Eval(&'a Expr),
    SelfCallee(Span),
    Unary(UnaryOp, Span),
    BinaryLeft(BinaryOp, &'a Expr, Span),
    BinaryRight(BinaryOp, Span),
    Bool(Span),
    Choose(&'a [Stmt], &'a [Stmt], Span),
    WhileCheck {
        condition: &'a Expr,
        body: &'a [Stmt],
        span: &'a Span,
    },
    ForInit {
        name: &'a str,
        body: &'a [Stmt],
        span: &'a Span,
    },
    LoopIteration(LoopState<'a>),
    LoopBoundary {
        state: LoopState<'a>,
        owner_call: usize,
    },
    Bind(&'a str, bool, bool, Span),
    Discard,
    Return,
    TryExit(usize),
    FinallyExit(usize),
    ScopeExit {
        span: Span,
        value_base: usize,
        fallthrough: ScopeFallthrough,
    },
    CallExit,
    PrepareArguments {
        function: &'a FnDecl,
        args: &'a [Expr],
        span: &'a Span,
    },
    ArgumentsExit(ArgumentFrame<'a>),
    Arguments {
        function: &'a FnDecl,
        args: &'a [Expr],
        next: usize,
        exit_index: usize,
        span: &'a Span,
    },
}

enum Syntax<'a> {
    Function(&'a FnDecl),
    Statement(&'a Stmt),
    Expression(&'a Expr),
}

struct Machine<'a> {
    source: &'a HashMap<String, FnDecl>,
    // Some only for one closed local self-recursive function. These shared
    // binding cells contain stable scalar values, never a copied Owned payload.
    captures: Option<&'a Env>,
    calls: FrameStack<CallFrame<'a>>,
    work: FrameStack<Work<'a>>,
    values: FrameStack<Value>,
    // Declared after the ordinary scalar frames so their panic drops remain
    // inside active synchronous cleanup before these guards restore TLS.
    finally: FinallyFrames<'a>,
    initial_depth: usize,
    call_depth: usize,
    #[cfg(test)]
    remaining_dispatches: usize,
    metrics: Metrics,
    pending: Option<KuResult<Flow>>,
    value_limits: FrameLimits,
    work_limits: FrameLimits,
    argument_capacity: usize,
    argument_bytes: usize,
    peak_argument_bytes: usize,
}

fn storage_error(error: ReserveError, span: Span) -> KuError {
    // Private storage diagnostics. Existing runtime diagnoses are reused for
    // already-defined semantics, e.g. call_depth_exceeded below.
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
    preflight_tree_mode(source, limits, entry, false, None)
}

fn preflight_tree_mode(
    source: &HashMap<String, FnDecl>,
    limits: Limits,
    entry: Option<&FnDecl>,
    local_self: bool,
    _captures: Option<&HashSet<String>>,
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
                    || function.params.iter().any(|param| {
                        param.mode != ParamMode::Owned
                            || !scalar_type(&param.ty)
                            || (local_self && param.name == function.name)
                    })
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
            Syntax::Statement(stmt) => {
                if local_self
                    && matches!(stmt, Stmt::VarDecl { name, .. }
                        | Stmt::Assign { name, .. } | Stmt::For { name, .. }
                        if entry.is_some_and(|function| name == &function.name))
                {
                    return Err(unsupported(stmt_span(stmt)));
                }
                match stmt {
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
                    Stmt::While {
                        condition,
                        body,
                        span,
                    }
                    | Stmt::For {
                        iterable: condition,
                        body,
                        span,
                        ..
                    } => {
                        let count = body
                            .len()
                            .checked_add(1)
                            .ok_or_else(|| unsupported(*span))?;
                        reserve(&mut pending, count, *span)?;
                        for stmt in body.iter().rev() {
                            push(&mut pending, Syntax::Statement(stmt));
                        }
                        push(&mut pending, Syntax::Expression(condition));
                    }
                    Stmt::Break { .. } | Stmt::Continue { .. } => {}
                    Stmt::Try {
                        body,
                        catch_name,
                        catch_body,
                        finally_body,
                        span,
                    } => {
                        // Fail, Result propagation and catch-owned fields have
                        // not migrated. Preserve the entire old evaluator for
                        // any catch clause, including an unreachable one.
                        if catch_name.is_some() || !catch_body.is_empty() {
                            return Err(unsupported(*span));
                        }
                        let count = body
                            .len()
                            .checked_add(finally_body.len())
                            .ok_or_else(|| unsupported(*span))?;
                        reserve(&mut pending, count, *span)?;
                        for stmt in body.iter().chain(finally_body).rev() {
                            push(&mut pending, Syntax::Statement(stmt));
                        }
                    }
                    _ => return Err(unsupported(stmt_span(stmt))),
                }
            }
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
                    if local_self && args.len() != function.params.len() {
                        // Preserve the existing function-value argument error
                        // and its priority by leaving this complete tree there.
                        return Err(unsupported(expr.span));
                    }
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
            captures: None,
            calls: FrameStack::new(limits.calls),
            work: FrameStack::new(limits.work),
            values: FrameStack::new(limits.values),
            finally: FinallyFrames {
                frames: FrameStack::new(limits.work),
            },
            initial_depth,
            call_depth: initial_depth,
            #[cfg(test)]
            remaining_dispatches: limits.prototype_dispatch_steps,
            metrics: Metrics::default(),
            pending: None,
            value_limits: limits.values,
            work_limits: limits.work,
            argument_capacity: 0,
            argument_bytes: 0,
            peak_argument_bytes: 0,
        }
    }

    fn frame(&mut self) -> &mut CallFrame<'a> {
        let index = self.calls.len() - 1;
        self.calls.get_mut(index).expect("current call frame")
    }

    fn value(&mut self) -> Value {
        self.values.pop().expect("completed operand")
    }

    fn emit(&mut self, value: Value, span: Span) -> KuResult<()> {
        self.reserve_values(1, span)?;
        push(&mut self.values, value);
        Ok(())
    }

    fn reserve_work(&mut self, additional: usize, span: Span) -> KuResult<()> {
        let frames = self
            .work_limits
            .max_frames
            .checked_sub(self.finally.frames.len())
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .work_limits
            .max_storage_bytes
            .checked_sub(self.finally.frames.charged_storage_bytes())
            .ok_or_else(|| storage_error(ReserveError::StorageLimit, span))?;
        self.work
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| storage_error(error, span))
    }

    fn reserve_finally(&mut self, additional: usize, span: Span) -> KuResult<()> {
        let frames = self
            .work_limits
            .max_frames
            .checked_sub(self.work.len())
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .work_limits
            .max_storage_bytes
            .checked_sub(self.work.charged_storage_bytes())
            .ok_or_else(|| storage_error(ReserveError::StorageLimit, span))?;
        self.finally
            .frames
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| storage_error(error, span))
    }

    fn reserve_values(&mut self, additional: usize, span: Span) -> KuResult<()> {
        let frames = self
            .value_limits
            .max_frames
            .checked_sub(self.argument_capacity)
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .value_limits
            .max_storage_bytes
            .checked_sub(self.argument_bytes)
            .ok_or_else(|| storage_error(ReserveError::StorageLimit, span))?;
        self.values
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| storage_error(error, span))
    }

    fn argument_storage(&mut self, count: usize, span: Span) -> KuResult<Vec<Value>> {
        self.check_argument_capacity(count, span)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| storage_error(ReserveError::AllocationFailed, span))?;
        // As with FrameStack, allocator overcapacity is checked before the
        // candidate is published. This is retained backing accounting, not a
        // pre-allocation hard cap on allocator metadata or process RSS.
        let capacity = values.capacity();
        let bytes = capacity
            .checked_mul(std::mem::size_of::<Value>())
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        let transient_bytes = self
            .argument_bytes
            .checked_add(bytes)
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        self.peak_argument_bytes = self.peak_argument_bytes.max(transient_bytes);
        self.check_argument_capacity(capacity, span)?;
        self.argument_capacity += capacity;
        self.argument_bytes += bytes;
        Ok(values)
    }

    fn check_argument_capacity(&self, additional: usize, span: Span) -> KuResult<()> {
        let capacity = self
            .argument_capacity
            .checked_add(additional)
            .and_then(|total| total.checked_add(self.values.capacity()))
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        if capacity > self.value_limits.max_frames {
            return Err(storage_error(ReserveError::FrameLimit, span));
        }
        let bytes = additional
            .checked_mul(std::mem::size_of::<Value>())
            .and_then(|bytes| bytes.checked_add(self.argument_bytes))
            .and_then(|bytes| bytes.checked_add(self.values.charged_storage_bytes()))
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        if bytes > self.value_limits.max_storage_bytes {
            return Err(storage_error(ReserveError::StorageLimit, span));
        }
        Ok(())
    }

    fn release_argument_capacity(&mut self, capacity: usize) {
        // The corresponding buffer was consumed by a hook or already dropped.
        self.argument_capacity -= capacity;
        self.argument_bytes -= capacity * std::mem::size_of::<Value>();
    }

    fn discard_values_after(&mut self, base: usize) {
        // Only scalar operands enter this slice. Future Owned migration must
        // add its resource teardown rather than relying on this scalar pop.
        while self.values.len() > base {
            self.values.pop();
        }
    }

    fn enter(
        &mut self,
        function: &'a FnDecl,
        args: &[Value],
        depth: usize,
        span: &'a Span,
    ) -> KuResult<()> {
        // Same two checks and same admission point as named call_function_flow.
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "runtime",
                "call_depth_exceeded",
                format!("maximum call depth exceeded: {MAX_CALL_DEPTH}"),
                *span,
            ));
        }
        if args.len() != function.params.len() {
            return Err(KuError::runtime(
                format!(
                    "function '{}' expects {} arguments but got {}",
                    function.name,
                    function.params.len(),
                    args.len()
                ),
                function.span,
            ));
        }
        reserve(&mut self.calls, 1, *span)?;
        self.reserve_work(3, *span)?;
        // The result and all exit markers are admitted before parameter binding
        // or a scope effect. A later failure unwinds without new storage.
        self.reserve_values(1, *span)?;
        let env = if let Some(captures) = self.captures {
            let mut env = captures.clone();
            env.push_scope();
            env
        } else {
            Env::new()
        };
        let kind = if self.captures.is_some() {
            CallKind::FunctionValue
        } else {
            CallKind::Named
        };
        push(
            &mut self.calls,
            CallFrame {
                env,
                leases: Vec::new(),
                depth,
                value_base: self.values.len(),
                epilogue_span: if self.captures.is_some() {
                    span
                } else {
                    &function.span
                },
                kind,
            },
        );
        self.call_depth += 1;
        self.metrics.entered_calls += 1;
        self.metrics.peak_calls = self.metrics.peak_calls.max(self.calls.len());
        push(&mut self.work, Work::CallExit);
        for (param, value) in function.params.iter().zip(args) {
            let value = scalar(value, *span)?;
            self.frame()
                .env
                .define_parameter(param.name.clone(), value, false, param.span)?;
        }
        push(
            &mut self.work,
            Work::ScopeExit {
                span: function.body.first().map(stmt_span).unwrap_or_default(),
                value_base: self.values.len(),
                fallthrough: ScopeFallthrough::ReturnNull,
            },
        );
        self.frame().env.push_scope();
        push(
            &mut self.work,
            Work::Block {
                body: &function.body,
                next: 0,
            },
        );
        Ok(())
    }

    fn enter_loop_iteration(&mut self, state: LoopState<'a>) -> KuResult<()> {
        let span = state.span();
        let (body, scope_span) = match state {
            LoopState::While { body, .. } => {
                (body, body.first().map(stmt_span).unwrap_or_default())
            }
            LoopState::ForInt { body, .. } => (body, span),
        };
        // No scope or iterator effect precedes admission of every continuation.
        // Binding failure still has its real ScopeExit and LoopBoundary live.
        self.reserve_work(3, span)?;
        push(
            &mut self.work,
            Work::LoopBoundary {
                state,
                owner_call: self.calls.len(),
            },
        );
        push(
            &mut self.work,
            Work::ScopeExit {
                span: scope_span,
                value_base: self.values.len(),
                fallthrough: ScopeFallthrough::Continue,
            },
        );
        self.frame().env.push_scope();
        if let LoopState::ForInt { name, current, .. } = state {
            // exec_for_iteration gives the mutable iterator and its body one
            // shared scope, even when the iterator shadows an outer binding.
            self.frame()
                .env
                .define_owned(name.to_string(), Value::Int(current), true, span)?;
        }
        push(&mut self.work, Work::Block { body, next: 0 });
        Ok(())
    }

    fn resume_loop(&mut self, state: LoopState<'a>) -> KuResult<()> {
        match state {
            LoopState::While {
                condition,
                body,
                span,
            } => {
                self.reserve_work(2, *span)?;
                push(
                    &mut self.work,
                    Work::WhileCheck {
                        condition,
                        body,
                        span,
                    },
                );
                push(&mut self.work, Work::Eval(condition));
            }
            LoopState::ForInt {
                name,
                body,
                span,
                current,
                limit,
            } => {
                // Only 0 <= current < limit <= i64::MAX is admitted. The
                // iterator binding may be assigned in the body; advancement
                // follows this private counter, as in the original evaluator.
                let current = current + 1;
                if current < limit {
                    self.reserve_work(1, *span)?;
                    push(
                        &mut self.work,
                        Work::LoopIteration(LoopState::ForInt {
                            name,
                            body,
                            span,
                            current,
                            limit,
                        }),
                    );
                }
            }
        }
        Ok(())
    }

    fn enter_try(
        &mut self,
        body: &'a [Stmt],
        finally_body: &'a [Stmt],
        span: &'a Span,
    ) -> KuResult<()> {
        // Reserve the outcome owner and every stage marker before body effects.
        // Publishing an empty frame lets reserve_work charge its live count and
        // retained bytes; failed admission drops only that empty metadata frame.
        self.reserve_finally(1, *span)?;
        let index = self.finally.frames.len();
        push(
            &mut self.finally.frames,
            FinallyFrame {
                body: finally_body,
                span,
                owner_call: self.calls.len(),
                primary: None,
                mode: cancellation::FinallyMode::Skip,
                guard: None,
            },
        );
        if let Err(error) = self.reserve_work(4, *span) {
            drop(self.finally.frames.pop().expect("unentered finally frame"));
            return Err(error);
        }
        self.metrics.entered_finally_frames += 1;
        self.metrics.peak_finally_frames = self
            .metrics
            .peak_finally_frames
            .max(self.finally.frames.len());
        push(&mut self.work, Work::FinallyExit(index));
        push(&mut self.work, Work::TryExit(index));
        push(
            &mut self.work,
            Work::ScopeExit {
                span: body.first().map(stmt_span).unwrap_or_default(),
                value_base: self.values.len(),
                fallthrough: ScopeFallthrough::Continue,
            },
        );
        self.frame().env.push_scope();
        push(&mut self.work, Work::Block { body, next: 0 });
        Ok(())
    }

    fn step(&mut self, step: Work<'a>) -> KuResult<()> {
        match step {
            Work::Block { body, next } => {
                if let Some(stmt) = body.get(next) {
                    self.reserve_work(2, stmt_span(stmt))?;
                    push(
                        &mut self.work,
                        Work::Block {
                            body,
                            next: next + 1,
                        },
                    );
                    push(&mut self.work, Work::Statement(stmt));
                }
            }
            Work::Statement(stmt) => {
                self.metrics.steps += 1;
                if !matches!(
                    stmt,
                    Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Try { .. }
                ) {
                    self.reserve_work(2, stmt_span(stmt))?;
                }
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
                    Stmt::While {
                        condition,
                        body,
                        span,
                    } => {
                        push(
                            &mut self.work,
                            Work::WhileCheck {
                                condition,
                                body,
                                span,
                            },
                        );
                        push(&mut self.work, Work::Eval(condition));
                    }
                    Stmt::For {
                        name,
                        iterable,
                        body,
                        span,
                    } => {
                        push(&mut self.work, Work::ForInit { name, body, span });
                        push(&mut self.work, Work::Eval(iterable));
                    }
                    Stmt::Break { .. } => self.pending = Some(Ok(Flow::Break(None))),
                    Stmt::Continue { .. } => self.pending = Some(Ok(Flow::LoopContinue(None))),
                    Stmt::Try {
                        body,
                        finally_body,
                        span,
                        ..
                    } => self.enter_try(body, finally_body, span)?,
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
                        self.reserve_work(2, expr.span)?;
                        push(&mut self.work, Work::Unary(*op, expr.span));
                        push(&mut self.work, Work::Eval(operand));
                    }
                    ExprKind::Binary { left, op, right } => {
                        self.reserve_work(2, expr.span)?;
                        push(&mut self.work, Work::BinaryLeft(*op, right, expr.span));
                        push(&mut self.work, Work::Eval(left));
                    }
                    ExprKind::Call { callee, args } => {
                        let ExprKind::Variable(name) = &callee.kind else {
                            unreachable!("preflight");
                        };
                        let function = self.source.get(name).expect("preflight checked callee");
                        self.reserve_work(if self.captures.is_some() { 2 } else { 1 }, expr.span)?;
                        push(
                            &mut self.work,
                            Work::PrepareArguments {
                                function,
                                args,
                                span: &expr.span,
                            },
                        );
                        if self.captures.is_some() {
                            // Function-value dispatch evaluates the callable
                            // before its arguments. Keep that exact safepoint;
                            // its self binding cannot be observed in this tree.
                            push(&mut self.work, Work::SelfCallee(callee.span));
                        }
                    }
                    _ => unreachable!("preflight rejected unsupported expression"),
                }
            }
            Work::SelfCallee(_) => self.metrics.steps += 1,
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
                self.reserve_work(2, span)?;
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
                self.reserve_work(2, span)?;
                push(
                    &mut self.work,
                    Work::ScopeExit {
                        span: selected.first().map(stmt_span).unwrap_or_default(),
                        value_base: self.values.len(),
                        fallthrough: ScopeFallthrough::Continue,
                    },
                );
                self.frame().env.push_scope();
                push(
                    &mut self.work,
                    Work::Block {
                        body: selected,
                        next: 0,
                    },
                );
            }
            Work::WhileCheck {
                condition,
                body,
                span,
            } => {
                if expect_bool_condition(self.value(), *span)? {
                    self.reserve_work(1, *span)?;
                    push(
                        &mut self.work,
                        Work::LoopIteration(LoopState::While {
                            condition,
                            body,
                            span,
                        }),
                    );
                }
            }
            Work::ForInit { name, body, span } => {
                let limit = match self.value() {
                    Value::Int(limit) if limit >= 0 => limit,
                    Value::Int(_) => {
                        return Err(KuError::runtime(
                            "for int iterator expects a non-negative int",
                            *span,
                        ));
                    }
                    other => {
                        return Err(KuError::runtime(
                            format!(
                                "type error: for expects array or int but got {}",
                                other.type_name()
                            ),
                            *span,
                        ));
                    }
                };
                if limit > 0 {
                    self.reserve_work(1, *span)?;
                    push(
                        &mut self.work,
                        Work::LoopIteration(LoopState::ForInt {
                            name,
                            body,
                            span,
                            current: 0,
                            limit,
                        }),
                    );
                }
            }
            Work::LoopIteration(state) => {
                self.metrics.steps += 1;
                self.enter_loop_iteration(state)?;
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
                self.pending = Some(Ok(Flow::returned(value)));
            }
            Work::PrepareArguments {
                function,
                args,
                span,
            } => {
                // The argument boundary is installed before the first argument
                // effect. Its actual buffer stays alive through the callee's
                // scope/call exits and is consumed by the original arbiter.
                self.reserve_work(2, *span)?;
                let staged = self.argument_storage(args.len(), *span)?;
                let exit_index = self.work.len();
                push(
                    &mut self.work,
                    Work::ArgumentsExit(ArgumentFrame {
                        span,
                        value_base: self.values.len(),
                        leases: Vec::new(),
                        staged,
                    }),
                );
                push(
                    &mut self.work,
                    Work::Arguments {
                        function,
                        args,
                        next: 0,
                        exit_index,
                        span,
                    },
                );
            }
            Work::Arguments {
                function,
                args,
                next,
                exit_index,
                span,
            } => {
                if next > 0 {
                    let value = self.value();
                    let Work::ArgumentsExit(arguments) = self
                        .work
                        .get_mut(exit_index)
                        .expect("live argument exit marker")
                    else {
                        unreachable!("argument marker retains its stable stack index");
                    };
                    // Full arity capacity was reserved before argument effects.
                    assert!(arguments.staged.len() < arguments.staged.capacity());
                    arguments.staged.push(value);
                }
                if let Some(arg) = args.get(next) {
                    self.reserve_work(2, *span)?;
                    push(
                        &mut self.work,
                        Work::Arguments {
                            function,
                            args,
                            next: next + 1,
                            exit_index,
                            span,
                        },
                    );
                    push(&mut self.work, Work::Eval(arg));
                } else {
                    let depth = self.frame().depth + 1;
                    // Temporarily move the buffer, never clone its elements.
                    // Stack growth may move the marker, but its index remains
                    // stable until every continuation above it has retired.
                    let Work::ArgumentsExit(arguments) = self
                        .work
                        .get_mut(exit_index)
                        .expect("live argument exit marker")
                    else {
                        unreachable!("argument exit marker");
                    };
                    let staged = std::mem::take(&mut arguments.staged);
                    let result = self.enter(function, &staged, depth, span);
                    let Work::ArgumentsExit(arguments) = self
                        .work
                        .get_mut(exit_index)
                        .expect("live argument exit marker")
                    else {
                        unreachable!("argument exit marker");
                    };
                    arguments.staged = staged;
                    result?;
                }
            }
            Work::ScopeExit { .. }
            | Work::CallExit
            | Work::ArgumentsExit(_)
            | Work::LoopBoundary { .. }
            | Work::TryExit(_)
            | Work::FinallyExit(_) => {
                unreachable!("exit markers run through resource arbitration");
            }
        }
        Ok(())
    }

    fn finish_boundary(&mut self, boundary: Work<'a>, hooks: &mut impl ExecutionHooks) {
        match boundary {
            Work::TryExit(index) => {
                assert_eq!(index + 1, self.finally.frames.len(), "current try frame");
                let (body, span, owner_call) = {
                    let frame = self.finally.frames.get(index).expect("live try frame");
                    (frame.body, *frame.span, frame.owner_call)
                };
                assert_eq!(owner_call, self.calls.len(), "try owns its call frame");
                let primary = self.pending.take().unwrap_or(Ok(Flow::Continue));
                let mode = hooks.prepare_finally(&self.frame().env, &primary, span);
                if matches!(mode, cancellation::FinallyMode::Skip) {
                    // Ordinary fatal errors skip finally in the existing
                    // language. Retire its preinstalled exit with its owner.
                    assert!(matches!(
                        self.work.pop(),
                        Some(Work::FinallyExit(saved)) if saved == index
                    ));
                    drop(self.finally.frames.pop().expect("skipped finally frame"));
                    self.metrics.retired_finally_frames += 1;
                    self.pending = Some(primary);
                } else {
                    let guard = match mode {
                        cancellation::FinallyMode::Protected(context) => {
                            Some(CleanupGuard::enter(context))
                        }
                        cancellation::FinallyMode::Ordinary => None,
                        cancellation::FinallyMode::Skip => unreachable!("handled skip"),
                    };
                    let frame = self.finally.frames.get_mut(index).expect("live try frame");
                    frame.primary = Some(primary);
                    frame.mode = mode;
                    frame.guard = guard;
                    assert!(matches!(
                        self.work.get(self.work.len() - 1),
                        Some(Work::FinallyExit(saved)) if *saved == index
                    ));
                    // Try admission reserved four slots. Its body ScopeExit,
                    // Block and TryExit have retired, leaving two guaranteed
                    // slots above the already-installed FinallyExit. No unwind
                    // allocation can prevent this finally body from entering.
                    push(
                        &mut self.work,
                        Work::ScopeExit {
                            span: body.first().map(stmt_span).unwrap_or_default(),
                            value_base: self.values.len(),
                            fallthrough: ScopeFallthrough::Continue,
                        },
                    );
                    self.frame().env.push_scope();
                    push(&mut self.work, Work::Block { body, next: 0 });
                }
            }
            Work::FinallyExit(index) => {
                assert_eq!(
                    index + 1,
                    self.finally.frames.len(),
                    "current finally frame"
                );
                let replacement = self.pending.take().unwrap_or(Ok(Flow::Continue));
                let mut frame = self.finally.frames.pop().expect("live finally frame");
                assert_eq!(
                    frame.owner_call,
                    self.calls.len(),
                    "finally owns its call frame"
                );
                let primary = frame.primary.take().expect("saved primary outcome");
                let guard = frame.guard.take();
                let result = hooks.finish_finally(primary, replacement, frame.mode, *frame.span);
                // Keep the exact synchronous cleanup window through shared
                // arbitration/discard, then restore its parent before resuming.
                drop(guard);
                drop(frame);
                self.metrics.retired_finally_frames += 1;
                if !matches!(&result, Ok(Flow::Continue)) {
                    self.pending = Some(result);
                }
            }
            Work::LoopBoundary { state, owner_call } => {
                // ScopeExit has already settled this iteration. CallExit must
                // normalize a callee's escaped loop control before it can reach
                // any caller loop, and the owning frame must still be current.
                assert_eq!(owner_call, self.calls.len(), "loop owns its call frame");
                match self.pending.take() {
                    None | Some(Ok(Flow::Continue | Flow::LoopContinue(_))) => {
                        if let Err(error) = self.resume_loop(state) {
                            self.pending = Some(Err(error));
                        }
                    }
                    Some(Ok(Flow::Break(_))) => {}
                    Some(result) => self.pending = Some(result),
                }
            }
            Work::ScopeExit {
                span,
                value_base,
                fallthrough,
            } => {
                let result = self.pending.take().unwrap_or_else(|| {
                    Ok(match fallthrough {
                        ScopeFallthrough::Continue => Flow::Continue,
                        ScopeFallthrough::ReturnNull => Flow::returned(Value::Null),
                    })
                });
                self.discard_values_after(value_base);
                let result = hooks.finish_scope(&mut self.frame().env, result, span, true);
                // Only a normally completed branch resumes its enclosing block.
                // Return and error continue through every remaining exit marker.
                if !matches!(&result, Ok(Flow::Continue)) {
                    self.pending = Some(result);
                }
            }
            Work::CallExit => {
                let result = self.pending.take().expect("call body outcome");
                let frame = self.calls.pop().expect("live call exit marker");
                self.discard_values_after(frame.value_base);
                let result = hooks.finish_call(
                    frame.env,
                    frame.leases,
                    result,
                    *frame.epilogue_span,
                    frame.kind,
                );
                // Retire only after the real parameter Env/leases arbitration.
                self.call_depth -= 1;
                self.metrics.retired_calls += 1;
                self.pending = Some(result);
            }
            Work::ArgumentsExit(arguments) => {
                let result = self.pending.take().expect("argument or callee outcome");
                self.discard_values_after(arguments.value_base);
                let capacity = arguments.staged.capacity();
                let span = *arguments.span;
                let result = hooks.finish_arguments(
                    &mut self.frame().env,
                    arguments.leases,
                    arguments.staged,
                    result,
                    span,
                );
                self.release_argument_capacity(capacity);
                match result {
                    // A callee's Fail has already been normalized at CallExit.
                    // Keep any argument Fail in its original Flow form instead
                    // of treating it as a callee Result value during unwinding.
                    Ok(flow @ Flow::Fail { .. }) => self.pending = Some(Ok(flow)),
                    Ok(flow) => match flow.into_call_value(span, "function") {
                        Ok(value) => {
                            // Callee admission reserved its return slot before
                            // parameter/body effects. No allocation on unwind.
                            push(&mut self.values, value);
                        }
                        Err(error) => self.pending = Some(Err(error)),
                    },
                    Err(error) => self.pending = Some(Err(error)),
                }
            }
            _ => unreachable!("only exit markers use resource arbitration"),
        }
    }

    #[cfg(test)]
    fn run(&mut self, entry: &str, args: &[Value], depth: usize) -> KuResult<Value> {
        self.run_with_poll(entry, args, depth, true, |_| Ok(()))
    }

    #[cfg(test)]
    fn run_with_poll(
        &mut self,
        entry: &str,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
        poll: impl FnMut(Span) -> KuResult<()>,
    ) -> KuResult<Value> {
        let span = self
            .source
            .get(entry)
            .map(|function| function.span)
            .unwrap_or_default();
        let mut hooks = PollHooks {
            host: Interpreter::new(),
            poll,
        };
        self.run_with_hooks(entry, args, depth, test_fuel, &mut hooks)?
            .into_call_value(span, "function")
    }

    fn run_with_hooks(
        &mut self,
        entry: &str,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
        hooks: &mut impl ExecutionHooks,
    ) -> KuResult<Flow> {
        #[cfg(not(test))]
        let _ = test_fuel;
        let function = self.source.get(entry);
        let span = function.map(|function| function.span).unwrap_or_default();
        self.pending = None;
        let admission = (|| {
            let function = function
                .ok_or_else(|| KuError::runtime(format!("undefined function '{entry}'"), span))?;
            // Retain the existing entry argument budget admission even though
            // this caller already owns those arguments outside the machine.
            self.reserve_values(args.len(), function.span)?;
            self.enter(function, args, depth, &function.span)
        })();
        if let Err(error) = admission {
            self.pending = Some(Err(error));
        }
        // The sole dispatch loop also performs every exit on failure. There is
        // no clearing of markers, recursive evaluator or child execution driver.
        while let Some(step) = self.work.pop() {
            let boundary = matches!(
                &step,
                Work::ScopeExit { .. }
                    | Work::CallExit
                    | Work::ArgumentsExit(_)
                    | Work::LoopBoundary { .. }
                    | Work::TryExit(_)
                    | Work::FinallyExit(_)
            );
            if self.pending.is_some() {
                if boundary {
                    self.finish_boundary(step, hooks);
                }
                continue;
            }
            // Fuel belongs to the test harness alone. Its failure takes the
            // same flat resource exit path as a real tick or expression error.
            #[cfg(test)]
            if test_fuel {
                let Some(remaining) = self.remaining_dispatches.checked_sub(1) else {
                    self.pending = Some(Err(KuError::runtime(
                        "internal execution prototype test fuel exhausted",
                        span,
                    )));
                    if boundary {
                        self.finish_boundary(step, hooks);
                    }
                    continue;
                };
                self.remaining_dispatches = remaining;
            }
            self.metrics.dispatches += 1;
            if boundary {
                self.finish_boundary(step, hooks);
                continue;
            }
            let poll = match &step {
                Work::Statement(stmt) => hooks.tick(stmt_span(stmt)),
                Work::Eval(expr) => hooks.tick(expr.span),
                Work::SelfCallee(span) => hooks.tick(*span),
                Work::LoopIteration(state) => hooks.tick(state.span()),
                _ => Ok(()),
            };
            if let Err(error) = poll {
                self.pending = Some(Err(error));
            } else if let Err(error) = self.step(step) {
                self.pending = Some(Err(error));
            }
        }
        assert_eq!(self.calls.len(), 0);
        assert_eq!(self.values.len(), 0);
        assert_eq!(self.argument_capacity, 0);
        assert_eq!(self.argument_bytes, 0);
        assert_eq!(self.finally.frames.len(), 0);
        self.metrics.remaining_calls = self.calls.len();
        self.metrics.remaining_work = self.work.len();
        self.metrics.remaining_values = self.values.len();
        self.metrics.remaining_finally_frames = self.finally.frames.len();
        self.metrics.logical_depth = self.call_depth;
        assert_eq!(self.call_depth, self.initial_depth);
        self.metrics.retained_backing_bytes = self.calls.charged_storage_bytes()
            + self.work.charged_storage_bytes()
            + self.values.charged_storage_bytes()
            + self.finally.frames.charged_storage_bytes()
            + self.argument_bytes;
        self.metrics.peak_backing_bytes_upper_bound = self.calls.peak_storage_bytes()
            + self.work.peak_storage_bytes()
            + self.values.peak_storage_bytes()
            + self.finally.frames.peak_storage_bytes()
            + self.peak_argument_bytes;
        self.pending.take().expect("root call or admission outcome")
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
) -> Option<KuResult<Flow>> {
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
    Some(machine.run_with_hooks(&function.name, args, depth, false, interpreter))
}

pub(super) fn try_execute_function_value(
    interpreter: &mut Interpreter,
    call: &FunctionValueCall<'_>,
) -> Option<KuResult<Flow>> {
    if call.is_async
        || call.params.len() != call.args.len()
        || call.params.len() != call.param_modes.len()
        || call
            .param_modes
            .iter()
            .any(|mode| *mode != ParamMode::Owned)
        || call
            .args
            .iter()
            .any(|value| scalar(value, call.span).is_err())
    {
        return None;
    }
    let name = call.self_name.as_ref()?;
    if interpreter.functions.contains_key(name) {
        // Global named calls take precedence over same-name local bindings in
        // the existing language, even inside a function value.
        return None;
    }
    let mut parameter_names = HashSet::new();
    if call.params.iter().any(|name| !parameter_names.insert(name)) {
        return None;
    }
    let function = FnDecl {
        name: name.clone(),
        is_async: false,
        type_params: Vec::new(),
        params: call
            .params
            .iter()
            .map(|name| Param {
                mode: ParamMode::Owned,
                name: name.clone(),
                ty: None,
                span: call.span,
            })
            .collect(),
        return_type: None,
        body: call.body.to_vec(),
        span: call.span,
    };
    // Clone only immutable code metadata, once for the complete subtree. The
    // recursive self binding is replaced by a static code reference only when
    // no admitted operation can read, overwrite or export that binding.
    let limits = Limits::default();
    let source = HashMap::from([(name.clone(), function)]);
    let entry = source.get(name).expect("local entry code");
    let captures = function_capture_names(entry).ok()?;
    if captures.iter().any(|name| {
        interpreter.functions.contains_key(name)
            || (call.captures.contains(name) && !call.captures.is_stable_scalar_binding(name))
    }) {
        return None;
    }
    if preflight_tree_mode(&source, limits, Some(entry), true, Some(&captures)).is_err() {
        return None;
    }
    let mut machine = Machine::new(&source, limits, interpreter.call_depth);
    machine.captures = Some(call.captures);
    Some(machine.run_with_hooks(name, &call.args, call.depth, false, interpreter))
}
