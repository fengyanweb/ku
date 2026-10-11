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
use std::num::NonZeroUsize;
use storage::{FrameLimits, FrameStack, ReserveError};

mod code;
mod storage;
use code::{
    AdmissionError, BlockId, CodeHandle, ExprCode, ExprId, ExprRange, FnId, ScalarLiteral, SpanId,
    StmtCode, StmtId, SymbolId,
};
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
    // Code backing has its separate original syntax budget. The construction
    // upper bound also includes temporary visit/binding/function buffers.
    code_retained_backing_bytes: usize,
    code_construction_peak_backing_bytes_upper_bound: usize,
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

struct CallFrame {
    env: Env,
    leases: Vec<BorrowLease>,
    depth: usize,
    value_base: usize,
    epilogue_span: SpanId,
    kind: CallKind,
}

#[derive(Clone, Copy)]
enum ScopeFallthrough {
    Continue,
    ReturnNull,
}

struct ArgumentFrame {
    span: SpanId,
    value_base: usize,
    leases: Vec<BorrowLease>,
    staged: Vec<Value>,
}

struct FinallyFrame {
    body: BlockId,
    span: SpanId,
    owner_call: usize,
    primary: Option<KuResult<Flow>>,
    mode: cancellation::FinallyMode,
    // Persistent effective TLS cause/D, separate from arbitration metadata.
    protection: Option<Protection>,
}

struct FinallyFrames {
    frames: FrameStack<FinallyFrame>,
    active_protected: Option<usize>,
}

impl Drop for FinallyFrames {
    fn drop(&mut self) {
        if let Some(panic) = self.drain() {
            std::panic::resume_unwind(panic);
        }
    }
}

#[derive(Clone, Copy)]
enum LoopState {
    While {
        condition: ExprId,
        body: BlockId,
        span: SpanId,
    },
    ForInt {
        name: SymbolId,
        body: BlockId,
        span: SpanId,
        current: i64,
        limit: i64,
    },
}

impl LoopState {
    fn span(self) -> SpanId {
        match self {
            Self::While { span, .. } | Self::ForInt { span, .. } => span,
        }
    }
}

enum Work {
    Block {
        body: BlockId,
        next: usize,
    },
    Statement(StmtId),
    Eval(ExprId),
    SelfCallee(SpanId),
    Unary(UnaryOp, SpanId),
    BinaryLeft(BinaryOp, ExprId, SpanId),
    BinaryRight(BinaryOp, SpanId),
    Bool(SpanId),
    Choose(BlockId, BlockId, SpanId),
    WhileCheck {
        condition: ExprId,
        body: BlockId,
        span: SpanId,
    },
    ForInit {
        name: SymbolId,
        body: BlockId,
        span: SpanId,
    },
    LoopIteration(LoopState),
    LoopBoundary {
        state: LoopState,
        owner_call: usize,
    },
    Bind(SymbolId, bool, bool, SpanId),
    Discard,
    Return,
    TryExit(usize),
    FinallyExit(usize),
    ScopeExit {
        span: SpanId,
        value_base: usize,
        fallthrough: ScopeFallthrough,
    },
    CallExit,
    PrepareArguments {
        function: FnId,
        args: ExprRange,
        span: SpanId,
    },
    ArgumentsExit(ArgumentFrame),
    Arguments {
        function: FnId,
        args: ExprRange,
        next: usize,
        exit_index: usize,
        span: SpanId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MachineState {
    Ready,
    Running,
    Finished,
    Aborted,
}

// Private same-thread checkpoints, not scheduler await or worker migration.
// Cleanup guards exist only in a synchronous dispatch or drop window.
enum ChunkProgress {
    More {
        popped: usize,
    },
    Finished {
        popped: usize,
        result: KuResult<Flow>,
    },
}

struct Machine {
    resources: Option<MachineResources>,
    finally: FinallyFrames,
    // A popped protected frame can panic in its real arbitration callback.
    // Only an aborted machine needs this release context for its live prefix.
    aborted_release: Option<RuntimeTermination>,
    _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
    initial_depth: usize,
    call_depth: usize,
    #[cfg(test)]
    remaining_dispatches: usize,
    #[cfg(test)]
    test_fuel: bool,
    state: MachineState,
    entry_span: Span,
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

// Test adapters exercise the same owned-code admission as production.
#[cfg(test)]
fn preflight(source: &HashMap<String, FnDecl>, limits: Limits) -> KuResult<()> {
    CodeHandle::compile_all(source, limits)
        .map(|_| ())
        .map_err(AdmissionError::into_error)
}

#[cfg(test)]
fn preflight_tree(
    source: &HashMap<String, FnDecl>,
    limits: Limits,
    entry: Option<&FnDecl>,
) -> KuResult<()> {
    match entry {
        Some(entry) => CodeHandle::compile_named(source, entry, limits),
        None => CodeHandle::compile_all(source, limits),
    }
    .map(|_| ())
    .map_err(AdmissionError::into_error)
}

impl Machine {
    #[cfg(test)]
    fn new(
        source: &HashMap<String, FnDecl>,
        limits: Limits,
        initial_depth: usize,
    ) -> KuResult<Self> {
        let code = CodeHandle::compile_all(source, limits).map_err(AdmissionError::into_error)?;
        Ok(Self::from_code(code, limits, initial_depth))
    }

    fn from_code(code: CodeHandle, limits: Limits, initial_depth: usize) -> Self {
        let metrics = Metrics {
            code_retained_backing_bytes: code.retained_bytes(),
            code_construction_peak_backing_bytes_upper_bound: code.construction_peak_bytes(),
            ..Metrics::default()
        };
        Self {
            resources: Some(MachineResources {
                code,
                captures: None,
                calls: FrameStack::new(limits.calls),
                work: FrameStack::new(limits.work),
                values: FrameStack::new(limits.values),
            }),
            finally: FinallyFrames {
                frames: FrameStack::new(limits.work),
                active_protected: None,
            },
            aborted_release: None,
            _same_thread: std::marker::PhantomData,
            initial_depth,
            call_depth: initial_depth,
            #[cfg(test)]
            remaining_dispatches: limits.prototype_dispatch_steps,
            #[cfg(test)]
            test_fuel: false,
            state: MachineState::Ready,
            entry_span: Span::default(),
            metrics,
            pending: None,
            value_limits: limits.values,
            work_limits: limits.work,
            argument_capacity: 0,
            argument_bytes: 0,
            peak_argument_bytes: 0,
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
        function_id: FnId,
        args: &[Value],
        depth: usize,
        span_id: SpanId,
    ) -> KuResult<()> {
        let function = self.code.function(function_id);
        let span = self.code.span(span_id);
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "runtime",
                "call_depth_exceeded",
                format!("maximum call depth exceeded: {MAX_CALL_DEPTH}"),
                span,
            ));
        }
        if args.len() != function.arity() {
            return Err(KuError::runtime(
                format!(
                    "function '{}' expects {} arguments but got {}",
                    self.code.symbol(function.name),
                    function.arity(),
                    args.len()
                ),
                self.code.span(function.span),
            ));
        }
        reserve(&mut self.calls, 1, span)?;
        self.reserve_work(3, span)?;
        self.reserve_values(1, span)?;
        let env = if let Some(captures) = &self.captures {
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
        let value_base = self.values.len();
        let epilogue_span = if self.captures.is_some() {
            span_id
        } else {
            function.span
        };
        push(
            &mut self.calls,
            CallFrame {
                env,
                leases: Vec::new(),
                depth,
                value_base,
                epilogue_span,
                kind,
            },
        );
        self.call_depth += 1;
        self.metrics.entered_calls += 1;
        self.metrics.peak_calls = self.metrics.peak_calls.max(self.calls.len());
        push(&mut self.work, Work::CallExit);
        for (index, value) in args.iter().enumerate() {
            let param = self.code.parameter(function, index);
            let name = self.code.symbol(param.name).to_string();
            let param_span = self.code.span(param.span);
            let value = scalar(value, span)?;
            self.frame()
                .env
                .define_parameter(name, value, false, param_span)?;
        }
        let body_span = self.code.block_span(function.body);
        let value_base = self.values.len();
        push(
            &mut self.work,
            Work::ScopeExit {
                span: body_span,
                value_base,
                fallthrough: ScopeFallthrough::ReturnNull,
            },
        );
        self.frame().env.push_scope();
        push(
            &mut self.work,
            Work::Block {
                body: function.body,
                next: 0,
            },
        );
        Ok(())
    }

    fn enter_loop_iteration(&mut self, state: LoopState) -> KuResult<()> {
        let span_id = state.span();
        let span = self.code.span(span_id);
        let (body, scope_span) = match state {
            LoopState::While { body, .. } => (body, self.code.block_span(body)),
            LoopState::ForInt { body, .. } => (body, span_id),
        };
        self.reserve_work(3, span)?;
        let owner_call = self.calls.len();
        let value_base = self.values.len();
        push(&mut self.work, Work::LoopBoundary { state, owner_call });
        push(
            &mut self.work,
            Work::ScopeExit {
                span: scope_span,
                value_base,
                fallthrough: ScopeFallthrough::Continue,
            },
        );
        self.frame().env.push_scope();
        if let LoopState::ForInt { name, current, .. } = state {
            let name = self.code.symbol(name).to_string();
            self.frame()
                .env
                .define_owned(name, Value::Int(current), true, span)?;
        }
        push(&mut self.work, Work::Block { body, next: 0 });
        Ok(())
    }

    fn resume_loop(&mut self, state: LoopState) -> KuResult<()> {
        match state {
            LoopState::While {
                condition,
                body,
                span,
            } => {
                self.reserve_work(2, self.code.span(span))?;
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
                let current = current + 1;
                if current < limit {
                    self.reserve_work(1, self.code.span(span))?;
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

    fn enter_try(&mut self, body: BlockId, finally_body: BlockId, span: SpanId) -> KuResult<()> {
        let source_span = self.code.span(span);
        self.reserve_finally(1, source_span)?;
        let index = self.finally.frames.len();
        let owner_call = self.calls.len();
        push(
            &mut self.finally.frames,
            FinallyFrame {
                body: finally_body,
                span,
                owner_call,
                primary: None,
                mode: cancellation::FinallyMode::Skip,
                protection: None,
            },
        );
        if let Err(error) = self.reserve_work(4, source_span) {
            drop(self.finally.pop().expect("unentered finally frame"));
            return Err(error);
        }
        self.metrics.entered_finally_frames += 1;
        self.metrics.peak_finally_frames = self
            .metrics
            .peak_finally_frames
            .max(self.finally.frames.len());
        push(&mut self.work, Work::FinallyExit(index));
        push(&mut self.work, Work::TryExit(index));
        let body_span = self.code.block_span(body);
        let value_base = self.values.len();
        push(
            &mut self.work,
            Work::ScopeExit {
                span: body_span,
                value_base,
                fallthrough: ScopeFallthrough::Continue,
            },
        );
        self.frame().env.push_scope();
        push(&mut self.work, Work::Block { body, next: 0 });
        Ok(())
    }

    fn step(&mut self, step: Work) -> KuResult<()> {
        match step {
            Work::Block { body, next } => {
                if let Some(stmt) = self.code.block_statement(body, next) {
                    self.reserve_work(2, self.code.span(self.code.statement(stmt).span))?;
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
                let stmt = self.code.statement(stmt);
                let span = self.code.span(stmt.span);
                if !matches!(
                    stmt.kind,
                    StmtCode::Break | StmtCode::Continue | StmtCode::Try(..)
                ) {
                    self.reserve_work(2, span)?;
                }
                match stmt.kind {
                    StmtCode::VarDecl(name, value, mutable) => {
                        let mutable = mutable && !is_constant_name(self.code.symbol(name));
                        push(&mut self.work, Work::Bind(name, mutable, true, stmt.span));
                        push(&mut self.work, Work::Eval(value));
                    }
                    StmtCode::Assign(name, value) => {
                        let mutable = !is_constant_name(self.code.symbol(name));
                        push(&mut self.work, Work::Bind(name, mutable, false, stmt.span));
                        push(&mut self.work, Work::Eval(value));
                    }
                    StmtCode::If(condition, then_body, else_body) => {
                        push(
                            &mut self.work,
                            Work::Choose(then_body, else_body, stmt.span),
                        );
                        push(&mut self.work, Work::Eval(condition));
                    }
                    StmtCode::While(condition, body) => {
                        push(
                            &mut self.work,
                            Work::WhileCheck {
                                condition,
                                body,
                                span: stmt.span,
                            },
                        );
                        push(&mut self.work, Work::Eval(condition));
                    }
                    StmtCode::For(name, iterable, body) => {
                        push(
                            &mut self.work,
                            Work::ForInit {
                                name,
                                body,
                                span: stmt.span,
                            },
                        );
                        push(&mut self.work, Work::Eval(iterable));
                    }
                    StmtCode::Break => self.pending = Some(Ok(Flow::Break(None))),
                    StmtCode::Continue => self.pending = Some(Ok(Flow::LoopContinue(None))),
                    StmtCode::Try(body, finally_body) => {
                        self.enter_try(body, finally_body, stmt.span)?
                    }
                    StmtCode::Return(value) => {
                        push(&mut self.work, Work::Return);
                        if let Some(value) = value {
                            push(&mut self.work, Work::Eval(value));
                        } else {
                            self.emit(Value::Null, span)?;
                        }
                    }
                    StmtCode::Expr(expr) => {
                        push(&mut self.work, Work::Discard);
                        push(&mut self.work, Work::Eval(expr));
                    }
                }
            }
            Work::Eval(expr) => {
                self.metrics.steps += 1;
                let expr = self.code.expression(expr);
                let span = self.code.span(expr.span);
                match expr.kind {
                    ExprCode::Literal(value) => self.emit(
                        match value {
                            ScalarLiteral::Int(value) => Value::Int(value),
                            ScalarLiteral::Float(value) => Value::Float(value),
                            ScalarLiteral::Bool(value) => Value::Bool(value),
                            ScalarLiteral::Null => Value::Null,
                        },
                        span,
                    )?,
                    ExprCode::Variable(name) => {
                        let name = self.code.symbol(name);
                        let env = &self
                            .calls
                            .get(self.calls.len() - 1)
                            .expect("current call frame")
                            .env;
                        let value = env.with_value(name, span, |value| scalar(value, span))?;
                        self.emit(value, span)?;
                    }
                    ExprCode::Unary(op, operand) => {
                        self.reserve_work(2, span)?;
                        push(&mut self.work, Work::Unary(op, expr.span));
                        push(&mut self.work, Work::Eval(operand));
                    }
                    ExprCode::Binary(left, op, right) => {
                        self.reserve_work(2, span)?;
                        push(&mut self.work, Work::BinaryLeft(op, right, expr.span));
                        push(&mut self.work, Work::Eval(left));
                    }
                    ExprCode::Call {
                        function,
                        args,
                        callee_span,
                    } => {
                        self.reserve_work(if self.captures.is_some() { 2 } else { 1 }, span)?;
                        push(
                            &mut self.work,
                            Work::PrepareArguments {
                                function,
                                args,
                                span: expr.span,
                            },
                        );
                        if self.captures.is_some() {
                            push(&mut self.work, Work::SelfCallee(callee_span));
                        }
                    }
                }
            }
            Work::SelfCallee(_) => self.metrics.steps += 1,
            Work::Unary(op, span_id) => {
                let span = self.code.span(span_id);
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
            Work::BinaryLeft(op, right, span_id) => {
                let span = self.code.span(span_id);
                self.reserve_work(2, span)?;
                if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    let left = expect_bool_condition(self.value(), span)?;
                    if (op == BinaryOp::And && !left) || (op == BinaryOp::Or && left) {
                        self.emit(Value::Bool(left), span)?;
                    } else {
                        push(&mut self.work, Work::Bool(span_id));
                        push(&mut self.work, Work::Eval(right));
                    }
                } else {
                    push(&mut self.work, Work::BinaryRight(op, span_id));
                    push(&mut self.work, Work::Eval(right));
                }
            }
            Work::BinaryRight(op, span_id) => {
                let span = self.code.span(span_id);
                let right = self.value();
                let left = self.value();
                self.emit(eval_binary(op, left, right, span)?, span)?;
            }
            Work::Bool(span_id) => {
                let span = self.code.span(span_id);
                let value = expect_bool_condition(self.value(), span)?;
                self.emit(Value::Bool(value), span)?;
            }
            Work::Choose(then_body, else_body, span_id) => {
                let span = self.code.span(span_id);
                let selected = if expect_bool_condition(self.value(), span)? {
                    then_body
                } else {
                    else_body
                };
                self.reserve_work(2, span)?;
                let selected_span = self.code.block_span(selected);
                let value_base = self.values.len();
                push(
                    &mut self.work,
                    Work::ScopeExit {
                        span: selected_span,
                        value_base,
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
                let source_span = self.code.span(span);
                if expect_bool_condition(self.value(), source_span)? {
                    self.reserve_work(1, source_span)?;
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
                let source_span = self.code.span(span);
                let limit = match self.value() {
                    Value::Int(limit) if limit >= 0 => limit,
                    Value::Int(_) => {
                        return Err(KuError::runtime(
                            "for int iterator expects a non-negative int",
                            source_span,
                        ))
                    }
                    other => {
                        return Err(KuError::runtime(
                            format!(
                                "type error: for expects array or int but got {}",
                                other.type_name()
                            ),
                            source_span,
                        ))
                    }
                };
                if limit > 0 {
                    self.reserve_work(1, source_span)?;
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
            Work::Bind(name, mutable, declare, span_id) => {
                let value = self.value();
                let span = self.code.span(span_id);
                let name = self.code.symbol(name).to_string();
                let env = &mut self.frame().env;
                if !declare && env.contains(&name) {
                    env.assign_owned(&name, value, span)?;
                } else {
                    env.define_owned(name, value, mutable, span)?;
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
                let source_span = self.code.span(span);
                self.reserve_work(2, source_span)?;
                let staged = self.argument_storage(args.len, source_span)?;
                let exit_index = self.work.len();
                let value_base = self.values.len();
                push(
                    &mut self.work,
                    Work::ArgumentsExit(ArgumentFrame {
                        span,
                        value_base,
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
                        unreachable!("argument marker stable stack index");
                    };
                    assert!(arguments.staged.len() < arguments.staged.capacity());
                    arguments.staged.push(value);
                }
                if let Some(arg) = args.get(next) {
                    self.reserve_work(2, self.code.span(span))?;
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

    fn finish_boundary(
        &mut self,
        boundary: Work,
        hooks: &mut impl ExecutionHooks,
        window: &mut DispatchWindow,
    ) {
        match boundary {
            Work::TryExit(index) => {
                assert_eq!(index + 1, self.finally.frames.len(), "current try frame");
                let (body, span, owner_call) = {
                    let frame = self.finally.frames.get(index).expect("live try frame");
                    (frame.body, frame.span, frame.owner_call)
                };
                assert_eq!(owner_call, self.calls.len(), "try owns its call frame");
                let primary = self.pending.take().unwrap_or(Ok(Flow::Continue));
                let source_span = self.code.span(span);
                let mode = hooks.prepare_finally(&self.frame().env, &primary, source_span);
                if matches!(mode, cancellation::FinallyMode::Skip) {
                    // Ordinary fatal errors skip finally in the existing
                    // language. Retire its preinstalled exit with its owner.
                    assert!(matches!(
                        self.work.pop(),
                        Some(Work::FinallyExit(saved)) if saved == index
                    ));
                    drop(self.finally.pop().expect("skipped finally frame"));
                    self.metrics.retired_finally_frames += 1;
                    self.pending = Some(primary);
                } else {
                    let protection = match mode {
                        cancellation::FinallyMode::Protected(context) => Some(Protection {
                            parent: self.finally.active_protected,
                            context: window.activate(context),
                        }),
                        cancellation::FinallyMode::Ordinary => None,
                        cancellation::FinallyMode::Skip => unreachable!("handled skip"),
                    };
                    let frame = self.finally.frames.get_mut(index).expect("live try frame");
                    frame.primary = Some(primary);
                    frame.mode = mode;
                    frame.protection = protection;
                    if protection.is_some() {
                        self.finally.active_protected = Some(index);
                    }
                    assert!(matches!(
                        self.work.get(self.work.len() - 1),
                        Some(Work::FinallyExit(saved)) if *saved == index
                    ));
                    // Try admission reserved four slots. Its body ScopeExit,
                    // Block and TryExit have retired, leaving two guaranteed
                    // slots above the already-installed FinallyExit. No unwind
                    // allocation can prevent this finally body from entering.
                    let body_span = self.code.block_span(body);
                    let value_base = self.values.len();
                    push(
                        &mut self.work,
                        Work::ScopeExit {
                            span: body_span,
                            value_base,
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
                let mut frame = self.finally.pop().expect("live finally frame");
                assert_eq!(
                    frame.owner_call,
                    self.calls.len(),
                    "finally owns its call frame"
                );
                let primary = frame.primary.take().expect("saved primary outcome");
                let mut result = hooks.finish_finally(
                    primary,
                    replacement,
                    frame.mode,
                    self.code.span(frame.span),
                );
                if let cancellation::FinallyMode::Protected(mut metadata) = frame.mode {
                    if let Some(observed) = current_cleanup_context()
                        .or_else(|| frame.protection.map(|protection| protection.context))
                    {
                        metadata.cleanup_deadline =
                            metadata.cleanup_deadline.min(observed.cleanup_deadline);
                    }
                    match &mut result {
                        Ok(flow) => cancellation::ScopeExit::remember_cleanup(flow, metadata),
                        Err(error) => error.remember_scope_cleanup(metadata),
                    }
                }
                // This step's window keeps the pre-pop context through real
                // arbitration and discard. The next step enters its parent.
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
                let source_span = self.code.span(span);
                let result = hooks.finish_scope(&mut self.frame().env, result, source_span, true);
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
                    self.code.span(frame.epilogue_span),
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
                let span = self.code.span(arguments.span);
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
            .code
            .find_function(entry)
            .map(|id| self.code.span(self.code.function(id).span))
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
        // Preserve the existing explicit independent-call reuse of this
        // private synchronous wrapper. Continuation dispatch never restarts.
        if self.state == MachineState::Finished {
            self.prepare_next_run()?;
        }
        self.start_once(entry, args, depth, test_fuel)?;
        let budget = NonZeroUsize::new(256).expect("positive internal dispatch chunk budget");
        loop {
            match self.dispatch_chunk(budget, hooks)? {
                ChunkProgress::More { popped } => debug_assert_eq!(popped, budget.get()),
                ChunkProgress::Finished { popped, result } => {
                    debug_assert!(popped <= budget.get());
                    return result;
                }
            }
        }
    }

    fn lifecycle_error(&self, action: &str) -> KuError {
        KuError::runtime(
            format!(
                "internal execution cannot {action} in {:?} state",
                self.state
            ),
            self.entry_span,
        )
    }

    fn prepare_next_run(&mut self) -> KuResult<()> {
        if self.state != MachineState::Finished {
            return Err(self.lifecycle_error("prepare next run"));
        }
        // Retain immutable code, allocated frame capacity, cumulative metrics
        // and remaining test fuel only after actual resource arbitration.
        if self.calls.len() != 0
            || self.work.len() != 0
            || self.values.len() != 0
            || self.finally.frames.len() != 0
            || self.pending.is_some()
            || self.argument_capacity != 0
            || self.argument_bytes != 0
            || self.call_depth != self.initial_depth
        {
            return Err(self.lifecycle_error("prepare undrained next run"));
        }
        self.state = MachineState::Ready;
        Ok(())
    }

    fn start_once(
        &mut self,
        entry: &str,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
    ) -> KuResult<()> {
        if self.state != MachineState::Ready {
            return Err(self.lifecycle_error("start"));
        }
        #[cfg(not(test))]
        let _ = test_fuel;
        let function = self.code.find_function(entry);
        let span = function
            .map(|id| self.code.span(self.code.function(id).span))
            .unwrap_or_default();
        self.entry_span = span;
        self.state = MachineState::Running;
        #[cfg(test)]
        {
            self.test_fuel = test_fuel;
        }
        let admission = (|| {
            let function = function
                .ok_or_else(|| KuError::runtime(format!("undefined function '{entry}'"), span))?;
            // Retain the existing entry argument budget admission even though
            // this caller already owns those arguments outside the machine.
            let function_span = self.code.function(function).span;
            self.reserve_values(args.len(), self.code.span(function_span))?;
            self.enter(function, args, depth, function_span)
        })();
        if let Err(error) = admission {
            self.pending = Some(Err(error));
        }
        Ok(())
    }

    fn dispatch_chunk(
        &mut self,
        budget: NonZeroUsize,
        hooks: &mut impl ExecutionHooks,
    ) -> KuResult<ChunkProgress> {
        if self.state != MachineState::Running {
            return Err(self.lifecycle_error("dispatch"));
        }
        // One catch per chunk. The window stays outside it so native panic
        // feedback occurs before its synchronous guards restore TLS.
        let mut window = DispatchWindow::default();
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.dispatch_chunk_inner(budget, hooks, &mut window)
        }));
        if dispatched.is_err() {
            // Copy before restoring TLS: FinallyExit may already have popped
            // the frame that owned this shorter effective context. Never keep
            // a synchronous guard across the return to the host.
            self.aborted_release = current_cleanup_context();
        }
        window.finish(&mut self.finally);
        match dispatched {
            Ok(result) => result,
            Err(panic) => {
                self.state = MachineState::Aborted;
                std::panic::resume_unwind(panic)
            }
        }
    }

    fn dispatch_chunk_inner(
        &mut self,
        budget: NonZeroUsize,
        hooks: &mut impl ExecutionHooks,
        window: &mut DispatchWindow,
    ) -> KuResult<ChunkProgress> {
        let mut popped = 0;
        while popped < budget.get() {
            // Include pop and the skipped Work's actual destructor in the same
            // saved cleanup window as tick, step and exit arbitration.
            window.begin(self.finally.context());
            let Some(step) = self.work.pop() else {
                window.finish(&mut self.finally);
                break;
            };
            popped += 1;
            self.dispatch_one(step, hooks, window);
            window.finish(&mut self.finally);
        }
        if self.work.len() != 0 {
            return Ok(ChunkProgress::More { popped });
        }
        Ok(ChunkProgress::Finished {
            popped,
            result: self.finish_run(),
        })
    }

    fn dispatch_one(
        &mut self,
        step: Work,
        hooks: &mut impl ExecutionHooks,
        window: &mut DispatchWindow,
    ) {
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
                self.finish_boundary(step, hooks, window);
            }
            return;
        }
        // Fuel belongs to the test harness alone. Its failure takes the
        // same flat resource exit path as a real tick or expression error.
        #[cfg(test)]
        if self.test_fuel {
            let Some(remaining) = self.remaining_dispatches.checked_sub(1) else {
                self.pending = Some(Err(KuError::runtime(
                    "internal execution prototype test fuel exhausted",
                    self.entry_span,
                )));
                if boundary {
                    self.finish_boundary(step, hooks, window);
                }
                return;
            };
            self.remaining_dispatches = remaining;
        }
        self.metrics.dispatches += 1;
        if boundary {
            self.finish_boundary(step, hooks, window);
            return;
        }
        let poll = match &step {
            Work::Statement(stmt) => hooks.tick(self.code.span(self.code.statement(*stmt).span)),
            Work::Eval(expr) => hooks.tick(self.code.span(self.code.expression(*expr).span)),
            Work::SelfCallee(span) => hooks.tick(self.code.span(*span)),
            Work::LoopIteration(state) => hooks.tick(self.code.span(state.span())),
            _ => Ok(()),
        };
        if let Err(error) = poll {
            self.pending = Some(Err(error));
        } else if let Err(error) = self.step(step) {
            self.pending = Some(Err(error));
        }
    }

    fn finish_run(&mut self) -> KuResult<Flow> {
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
        // A completed run keeps the same immutable code owner. Code is shared
        // by every call, so its retained backing cannot grow with call depth.
        assert_eq!(
            self.metrics.code_retained_backing_bytes,
            self.code.retained_bytes()
        );
        assert!(
            self.metrics.code_retained_backing_bytes
                <= self
                    .metrics
                    .code_construction_peak_backing_bytes_upper_bound
        );
        let result = self.pending.take().expect("root call or admission outcome");
        self.state = MachineState::Finished;
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
    let mut machine = match Machine::new(source, limits, initial_depth) {
        Ok(machine) => machine,
        Err(error) => {
            return (
                Err(error),
                Metrics {
                    logical_depth: initial_depth,
                    ..Metrics::default()
                },
            )
        }
    };
    let result = machine.run(entry, args, depth);
    (result, std::mem::take(&mut machine.metrics))
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
    // Unsupported or inconclusive bounded validation retains the whole legacy
    // tree. Once fully selected, an arena rejection is a before-effects error;
    // never bypass the admitted storage contract by restarting recursively.
    if !code::eligible_named(&source, function, limits) {
        return None;
    }
    if function.params.len() != args.len() {
        return Some(Err(KuError::runtime(
            format!(
                "function '{}' expects {} arguments but got {}",
                function.name,
                function.params.len(),
                args.len()
            ),
            function.span,
        )));
    }
    let code = match CodeHandle::compile_named(&source, function, limits) {
        Ok(code) => code,
        Err(AdmissionError::Unsupported(_)) => return None,
        Err(error @ AdmissionError::Rejected(_)) => return Some(Err(error.into_error())),
    };
    let mut machine = Machine::from_code(code, limits, interpreter.call_depth);
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
    let name = call.self_name.as_deref()?;
    if interpreter.functions.contains_key(name) {
        return None;
    }
    let source = interpreter.functions.clone();
    let limits = Limits::default();
    if !code::eligible_local(&source, call, limits) {
        return None;
    }
    let code = match CodeHandle::compile_local(&source, call, limits) {
        Ok(code) => code,
        Err(AdmissionError::Unsupported(_)) => return None,
        Err(error @ AdmissionError::Rejected(_)) => return Some(Err(error.into_error())),
    };
    let mut machine = Machine::from_code(code, limits, interpreter.call_depth);
    // Env clone shares binding cells and clears owned_bindings. It does not
    // clone any Value payload; actual per-call scopes still exit through hooks.
    machine.captures = Some(call.captures.clone());
    Some(machine.run_with_hooks(name, &call.args, call.depth, false, interpreter))
}

// Same-thread dispatch windows and the machine's original resource drop prefix.

struct MachineResources {
    code: CodeHandle,
    captures: Option<Env>,
    calls: FrameStack<CallFrame>,
    work: FrameStack<Work>,
    values: FrameStack<Value>,
}

impl std::ops::Deref for Machine {
    type Target = MachineResources;
    fn deref(&self) -> &Self::Target {
        self.resources
            .as_ref()
            .expect("live execution resource prefix")
    }
}
impl std::ops::DerefMut for Machine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.resources
            .as_mut()
            .expect("live execution resource prefix")
    }
}

#[derive(Clone, Copy)]
struct Protection {
    context: RuntimeTermination,
    parent: Option<usize>,
}

impl FinallyFrames {
    fn context(&self) -> Option<RuntimeTermination> {
        self.active_protected.map(|index| {
            self.frames
                .get(index)
                .expect("live protected finally")
                .protection
                .expect("active protection")
                .context
        })
    }

    fn tighten(&mut self, observed: Option<RuntimeTermination>) {
        if let (Some(index), Some(observed)) = (self.active_protected, observed) {
            let protection = self
                .frames
                .get_mut(index)
                .expect("live protected finally")
                .protection
                .as_mut()
                .expect("active protection");
            protection.context.cleanup_deadline = protection
                .context
                .cleanup_deadline
                .min(observed.cleanup_deadline);
        }
    }

    fn pop(&mut self) -> Option<FinallyFrame> {
        let index = self.frames.len().checked_sub(1)?;
        let frame = self.frames.pop().expect("live finally frame");
        if let Some(protection) = frame.protection {
            assert_eq!(self.active_protected, Some(index));
            self.active_protected = protection.parent;
            self.tighten(Some(protection.context));
        }
        Some(frame)
    }

    fn drain(&mut self) -> Option<NativeCleanupPanic> {
        let mut first = None;
        while self.frames.len() != 0 {
            // The context belongs to the state BEFORE retiring this frame.
            let guard = self.context().map(CleanupGuard::enter);
            let frame = self.pop().expect("live finally frame");
            let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(frame)));
            self.tighten(current_cleanup_context());
            drop(guard);
            if let Err(panic) = dropped {
                retain_first_panic(&mut first, panic);
            }
        }
        assert!(self.active_protected.is_none());
        first
    }
}

type NativeCleanupPanic = Box<dyn std::any::Any + Send + 'static>;

fn retain_first_panic(first: &mut Option<NativeCleanupPanic>, panic: NativeCleanupPanic) {
    if first.is_none() {
        *first = Some(panic);
    } else {
        // The original native panic hook already recorded its diagnostic.
        // Keep a static secondary marker without disclosing payload data.
        eprintln!("ku interpreter suppressed secondary native cleanup panic");
        drop(panic);
    }
}

#[derive(Default)]
struct DispatchWindow {
    base: Option<CleanupGuard>,
    activation: Option<CleanupGuard>,
}

impl DispatchWindow {
    fn begin(&mut self, context: Option<RuntimeTermination>) {
        assert!(self.base.is_none() && self.activation.is_none());
        self.base = context.map(CleanupGuard::enter);
    }

    fn activate(&mut self, context: RuntimeTermination) -> RuntimeTermination {
        assert!(self.activation.is_none());
        self.activation = Some(CleanupGuard::enter(context));
        current_cleanup_context().expect("new protected execution window")
    }

    fn finish(&mut self, frames: &mut FinallyFrames) {
        frames.tighten(current_cleanup_context());
        self.close();
    }

    fn close(&mut self) {
        drop(self.activation.take());
        drop(self.base.take());
    }
}

impl Drop for DispatchWindow {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // The same original prefix order is preserved by Rust's drop glue:
        // code, captures, calls, work, values. No user payload is cloned.
        let release = match (self.aborted_release, self.finally.context()) {
            (Some(mut saved), Some(active)) => {
                saved.cleanup_deadline = saved.cleanup_deadline.min(active.cleanup_deadline);
                Some(saved)
            }
            (saved, active) => saved.or(active),
        };
        let guard = release.map(CleanupGuard::enter);
        let prefix = self.resources.take();
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(prefix)));
        self.finally.tighten(current_cleanup_context());
        // Restore before ordinary outer primary outcomes are retired.
        drop(guard);
        let mut first = dropped.err();
        if let Some(panic) = self.finally.drain() {
            retain_first_panic(&mut first, panic);
        }
        if let Some(panic) = first {
            std::panic::resume_unwind(panic);
        }
    }
}
