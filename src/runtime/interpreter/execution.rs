//! Flat, bounded interpreter call/return continuations.
//!
//! The production slice admits a complete static scalar call tree before any
//! body effects, including scalar Result roots, static async dependencies and
//! local named async self calls with immutable scalar captures.
//! An async root enters only after its Task is claimed. Await checkpoints own
//! the original ticket and hand helper execution to the claimant-local driver.
//! Once admitted, every nested call stays in this dispatch loop.
//! Scope, call, argument and scalar finally exits use the evaluator's real
//! resource arbitration.
//! Other language constructs keep their existing evaluator; Owned, borrows,
//! general function values and recoverable catch require later slices.
//! This partial production integration does not close the full P0 contract.

use super::*;
use crate::ast::{Param, TypeName};
use std::cell::Cell;
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
    entered_exit_frames: usize,
    retired_exit_frames: usize,
    peak_exit_frames: usize,
    remaining_exit_frames: usize,
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
/// A child task receives shared code; its machine is constructed by its claimant.
trait ExecutionHooks {
    fn host(&mut self) -> &mut Interpreter;

    fn prepare_scope_exit(&mut self, _env: &mut Env, _span: Span) -> KuResult<()> {
        Ok(())
    }

    fn tick(&mut self, span: Span) -> KuResult<()>;
    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow>;
    fn finish_scope_with_feedback(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
        _feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        self.finish_scope(env, result, span, pop)
    }
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
    fn host(&mut self) -> &mut Interpreter {
        self
    }

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

    fn finish_scope_with_feedback(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
        feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        self.finish_owned_scope_with_feedback(env, result, span, pop, feedback)
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
    fn host(&mut self) -> &mut Interpreter {
        &mut self.host
    }

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
    previous_observers: usize,
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
    WrapTerminalOk(SpanId),
    AwaitOperand(SpanId),
    AwaitResume,
    Propagate(SpanId),
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
        dispatch: ArgumentDispatch,
    },
    ArgumentsExit(ArgumentFrame),
    Arguments {
        function: FnId,
        args: ExprRange,
        next: usize,
        exit_index: usize,
        span: SpanId,
        dispatch: ArgumentDispatch,
    },
}

#[derive(Clone, Copy)]
enum ArgumentDispatch {
    Synchronous,
    NamedAsync,
    LocalAsync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MachineState {
    Ready,
    Running,
    Waiting,
    Finished,
    Aborted,
}

// Private same-thread checkpoints, including retained await, not worker migration.
// Cleanup guards exist only in a synchronous dispatch or drop window.
enum ChunkProgress {
    More {
        popped: usize,
    },
    Waiting {
        popped: usize,
    },
    Finished {
        popped: usize,
        result: KuResult<Flow>,
    },
}

struct Machine {
    // The suspended expression owns its sole target before outer call/argument
    // resources. No synchronous TLS guard lives in this checkpoint.
    awaiting: Option<AwaitFrame>,
    exits: FrameStack<PendingScopeExit>,
    resources: Option<MachineResources>,
    finally: FinallyFrames,
    // A popped protected frame can panic in its real arbitration callback.
    // Only an aborted machine needs this release context for its live prefix.
    aborted_release: Option<RuntimeTermination>,
    // Budget rollback stays synchronous, but its actual release feedback
    // must survive restoring its short TLS window on return or native panic.
    fallback_release: Cell<Option<RuntimeTermination>>,
    fallback_metadata: Option<RuntimeTermination>,
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
    exit_handle_capacity: usize,
    exit_handle_bytes: usize,
    peak_exit_handle_bytes: usize,
}

struct PendingScopeExit {
    // The real Env stays in its original CallFrame. No borrowed Env or TLS
    // guard crosses dispatch boundaries, and its owner cannot retire early.
    owner_call: usize,
    state: cancellation::ExitState<Flow>,
    first_index: Option<usize>,
    handle_capacity: usize,
    span: SpanId,
}

struct AwaitFrame {
    ticket: crate::runtime::task::AwaitTicket,
    span: SpanId,
}

fn merge_release_context(
    first: Option<RuntimeTermination>,
    other: Option<RuntimeTermination>,
) -> Option<RuntimeTermination> {
    match (first, other) {
        (Some(mut first), Some(other)) => {
            first.cleanup_deadline = first.cleanup_deadline.min(other.cleanup_deadline);
            Some(first)
        }
        (first, other) => first.or(other),
    }
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
            awaiting: None,
            exits: FrameStack::new(limits.work),
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
            fallback_release: Cell::new(None),
            fallback_metadata: None,
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
            exit_handle_capacity: 0,
            exit_handle_bytes: 0,
            peak_exit_handle_bytes: 0,
        }
    }

    fn frame(&mut self) -> &mut CallFrame {
        let index = self.calls.len() - 1;
        self.calls.get_mut(index).expect("current call frame")
    }

    fn frame_owned_value(&mut self, name: SymbolId, span: Span) -> KuResult<Value> {
        let name = self.code.symbol(name);
        self.calls
            .get(self.calls.len() - 1)
            .expect("current call frame")
            .env
            .get_owning(name, span)
    }

    fn register_call_observer(
        &mut self,
        hooks: &mut impl ExecutionHooks,
        span: Span,
    ) -> KuResult<usize> {
        let host = hooks.host();
        let previous = host.caller_owners.len();
        if host
            .task_runtime
            .as_ref()
            .is_some_and(TaskRuntime::has_task_submissions)
        {
            let observer = self.frame().env.observe_owned_bindings(span)?;
            if !observer.is_empty() {
                host.caller_owners.try_reserve(1).map_err(|_| {
                    KuError::runtime("caller task ownership observation out of memory", span)
                })?;
                host.caller_owners.push(observer);
            }
        }
        Ok(previous)
    }

    fn start_async(
        &mut self,
        function: FnId,
        args: Vec<Value>,
        depth: usize,
        span: SpanId,
        local: bool,
        hooks: &mut impl ExecutionHooks,
    ) -> KuResult<()> {
        let source_span = self.code.span(span);
        self.reserve_values(1, source_span)?;
        if depth >= MAX_CALL_DEPTH || self.call_depth >= MAX_CALL_DEPTH {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "runtime",
                "call_depth_exceeded",
                format!("maximum call depth exceeded: {MAX_CALL_DEPTH}"),
                source_span,
            ));
        }
        let host = hooks.host();
        host.reject_cleanup_submission(source_span)?;
        for value in &args {
            value.require_owned_root(source_span)?;
        }
        let runtime = host.task_runtime.clone().ok_or_else(|| {
            KuError::runtime("async task runtime is not initialized", source_span)
        })?;
        let child_runtime = runtime.clone();
        let code = self.code.clone();
        let captures = if local {
            Some(
                self.captures
                    .as_ref()
                    .expect("local async code has captures")
                    .clone(),
            )
        } else {
            None
        };
        let task = runtime.spawn_runner_deferred(|| {
            let template = host.template();
            move || {
                let mut interpreter = Interpreter::from_template(template, child_runtime);
                interpreter.async_execution = true;
                match captures {
                    Some(captures) => ClaimedNamedTask::from_local_code(
                        interpreter,
                        code,
                        function,
                        captures,
                        args,
                        span,
                    ),
                    None => {
                        ClaimedNamedTask::from_code(interpreter, code, function, args, source_span)
                    }
                }
            }
        })?;
        // A synchronous call reaches ArgumentsExit through CallExit. Async
        // submission returns immediately, so install that same sole outcome
        // here; ArgumentsExit performs the actual returned-value emission.
        self.pending = Some(Ok(Flow::returned(Value::Task(task))));
        Ok(())
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
            .and_then(|frames| frames.checked_sub(self.exits.len()))
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .work_limits
            .max_storage_bytes
            .checked_sub(self.finally.frames.charged_storage_bytes())
            .and_then(|bytes| bytes.checked_sub(self.exits.charged_storage_bytes()))
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
            .and_then(|frames| frames.checked_sub(self.exits.len()))
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .work_limits
            .max_storage_bytes
            .checked_sub(self.work.charged_storage_bytes())
            .and_then(|bytes| bytes.checked_sub(self.exits.charged_storage_bytes()))
            .ok_or_else(|| storage_error(ReserveError::StorageLimit, span))?;
        self.finally
            .frames
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| storage_error(error, span))
    }

    fn reserve_exits(&mut self, additional: usize, span: Span) -> KuResult<()> {
        let frames = self
            .work_limits
            .max_frames
            .checked_sub(self.work.len())
            .and_then(|frames| frames.checked_sub(self.finally.frames.len()))
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .work_limits
            .max_storage_bytes
            .checked_sub(self.work.charged_storage_bytes())
            .and_then(|bytes| bytes.checked_sub(self.finally.frames.charged_storage_bytes()))
            .ok_or_else(|| storage_error(ReserveError::StorageLimit, span))?;
        self.exits
            .reserve_with_budget(additional, frames, bytes)
            .map_err(|error| storage_error(error, span))
    }

    fn reserve_values(&mut self, additional: usize, span: Span) -> KuResult<()> {
        let frames = self
            .value_limits
            .max_frames
            .checked_sub(self.argument_capacity)
            .and_then(|frames| frames.checked_sub(self.exit_handle_capacity))
            .ok_or_else(|| storage_error(ReserveError::FrameLimit, span))?;
        let bytes = self
            .value_limits
            .max_storage_bytes
            .checked_sub(self.argument_bytes)
            .and_then(|bytes| bytes.checked_sub(self.exit_handle_bytes))
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
            .and_then(|total| total.checked_add(self.exit_handle_capacity))
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        if capacity > self.value_limits.max_frames {
            return Err(storage_error(ReserveError::FrameLimit, span));
        }
        let bytes = additional
            .checked_mul(std::mem::size_of::<Value>())
            .and_then(|bytes| bytes.checked_add(self.argument_bytes))
            .and_then(|bytes| bytes.checked_add(self.exit_handle_bytes))
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

    fn check_exit_handle_capacity(&self, additional: usize, span: Span) -> KuResult<()> {
        let count = self
            .exit_handle_capacity
            .checked_add(additional)
            .and_then(|count| count.checked_add(self.argument_capacity))
            .and_then(|count| count.checked_add(self.values.capacity()))
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        if count > self.value_limits.max_frames {
            return Err(storage_error(ReserveError::FrameLimit, span));
        }
        let bytes = additional
            .checked_mul(std::mem::size_of::<crate::runtime::task::TaskHandle>())
            .and_then(|bytes| bytes.checked_add(self.exit_handle_bytes))
            .and_then(|bytes| bytes.checked_add(self.argument_bytes))
            .and_then(|bytes| bytes.checked_add(self.values.charged_storage_bytes()))
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        if bytes > self.value_limits.max_storage_bytes {
            return Err(storage_error(ReserveError::StorageLimit, span));
        }
        Ok(())
    }

    fn release_exit_handle_capacity(&mut self, capacity: usize) {
        self.exit_handle_capacity -= capacity;
        self.exit_handle_bytes -=
            capacity * std::mem::size_of::<crate::runtime::task::TaskHandle>();
    }

    fn scope_handles(
        &mut self,
        span: Span,
    ) -> KuResult<Option<Vec<crate::runtime::task::TaskHandle>>> {
        let count = match self.frame().env.current_scope_simple_task_count(span) {
            Ok(0) | Err(_) => return Ok(None),
            Ok(count) => count,
        };
        self.reserve_exits(1, span)?;
        self.check_exit_handle_capacity(count, span)?;
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(count)
            .map_err(|_| storage_error(ReserveError::AllocationFailed, span))?;
        let capacity = handles.capacity();
        let bytes = capacity
            .checked_mul(std::mem::size_of::<crate::runtime::task::TaskHandle>())
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        let peak = self
            .exit_handle_bytes
            .checked_add(bytes)
            .ok_or_else(|| storage_error(ReserveError::ArithmeticOverflow, span))?;
        self.peak_exit_handle_bytes = self.peak_exit_handle_bytes.max(peak);
        // Empty candidates and allocator overcapacity are checked before any
        // proxy or sole primary is published into a persistent exit header.
        self.check_exit_handle_capacity(capacity, span)?;
        self.exit_handle_capacity += capacity;
        self.exit_handle_bytes += bytes;
        if self
            .frame()
            .env
            .collect_current_scope_simple_tasks_reserved(&mut handles, span)
            .is_err()
        {
            drop(handles);
            self.release_exit_handle_capacity(capacity);
            // A general or changed scope retains the original collector and
            // its original diagnostic/cleanup priority; it is not skipped.
            return Ok(None);
        }
        Ok(Some(handles))
    }

    fn begin_scope_exit(
        &mut self,
        span: SpanId,
        value_base: usize,
        fallthrough: ScopeFallthrough,
        hooks: &mut impl ExecutionHooks,
    ) {
        let source_span = self.code.span(span);
        let admitted = hooks
            .prepare_scope_exit(&mut self.frame().env, source_span)
            .and_then(|()| self.scope_handles(source_span));
        // Keep admission before taking the sole primary, and retain the
        // evaluator's original primary-then-temporary-values release order.
        let result = self.pending.take().unwrap_or_else(|| {
            Ok(match fallthrough {
                ScopeFallthrough::Continue => Flow::Continue,
                ScopeFallthrough::ReturnNull => Flow::returned(Value::Null),
            })
        });
        self.discard_values_after(value_base);
        match admitted {
            Ok(Some(handles)) => {
                assert!(self.awaiting.is_none() && self.exits.len() == 0);
                let capacity = handles.capacity();
                let state = cancellation::ExitState::new(
                    hooks.host(),
                    result,
                    source_span,
                    handles,
                    Ok(()),
                );
                let owner_call = self.calls.len();
                push(
                    &mut self.exits,
                    PendingScopeExit {
                        owner_call,
                        state,
                        first_index: None,
                        handle_capacity: capacity,
                        span,
                    },
                );
                self.metrics.entered_exit_frames += 1;
                self.metrics.peak_exit_frames = self.metrics.peak_exit_frames.max(self.exits.len());
            }
            Err(mut error) => {
                // Select a single metadata context before scope cleanup can
                // consume a Continue outcome's only visible cleanup cause.
                // Reuse its D for real Env cleanup, outgoing-owner discard
                // and the storage error; never start a second cleanup budget.
                let host = hooks.host();
                host.observe_termination(&result);
                let carried = match &result {
                    Ok(flow) => cancellation::ScopeExit::cleanup_context(flow),
                    Err(primary) => primary.scope_cleanup(),
                };
                let mut context = host
                    .scope_cleanup_context(carried)
                    .or(self.fallback_metadata)
                    .unwrap_or_else(|| CancellationContext::new(TerminationReason::Cancelled));
                self.fallback_metadata = Some(context);
                let release = merge_release_context(
                    merge_release_context(current_cleanup_context(), Some(context)),
                    self.fallback_release.get(),
                );
                self.fallback_release.set(release);
                let _window = cancellation::PhaseWindow::enter(&self.fallback_release);
                // Borrow disjoint fields: the saved Cell remains alive while
                // the real scope and its native destructors are processed.
                let calls = &mut self.resources.as_mut().expect("live prefix").calls;
                let owner_call = calls.len() - 1;
                let env = &mut calls.get_mut(owner_call).expect("current scope call").env;
                let result = hooks.finish_scope_with_feedback(
                    env,
                    result,
                    source_span,
                    true,
                    &self.fallback_release,
                );
                for release in [self.fallback_release.get(), current_cleanup_context()]
                    .into_iter()
                    .flatten()
                {
                    context.cleanup_deadline =
                        context.cleanup_deadline.min(release.cleanup_deadline);
                }
                self.fallback_metadata = Some(context);
                let result = {
                    if matches!(&result, Err(primary) if primary.runtime_termination().is_some()) {
                        result
                    } else {
                        let host = hooks.host();
                        error.remember_scope_cleanup(context);
                        host.discard_flow(result, context, source_span);
                        // A losing owner's actual Drop can tighten D again.
                        // Preserve metadata cause while carrying that final
                        // deadline into the storage error and parent exits.
                        for release in [self.fallback_release.get(), current_cleanup_context()]
                            .into_iter()
                            .flatten()
                        {
                            context.cleanup_deadline =
                                context.cleanup_deadline.min(release.cleanup_deadline);
                        }
                        self.fallback_metadata = Some(context);
                        error.remember_scope_cleanup(context);
                        Err(error)
                    }
                };
                if !matches!(&result, Ok(Flow::Continue)) {
                    self.pending = Some(result);
                }
            }
            Ok(None) => {
                // No Task or a general collector keeps the original adapter
                // without adding a cancellation context to a successful scope.
                let result = hooks.finish_scope(&mut self.frame().env, result, source_span, true);
                if !matches!(&result, Ok(Flow::Continue)) {
                    self.pending = Some(result);
                }
            }
        }
    }

    fn scope_release_context(&self) -> Option<RuntimeTermination> {
        merge_release_context(
            self.exits
                .get(0)
                .and_then(|exit| exit.state.release_context()),
            self.fallback_release.get(),
        )
    }

    fn drive_scope_exit(&mut self, hooks: &mut impl ExecutionHooks) {
        assert!(self.awaiting.is_none() && self.exits.len() == 1);
        let exit = self.exits.get_mut(0).expect("one active scope exit");
        assert_eq!(
            exit.owner_call,
            self.resources.as_ref().expect("live prefix").calls.len()
        );
        exit.first_index = None;
        let host = hooks.host();
        let env = &mut self
            .resources
            .as_mut()
            .expect("live prefix")
            .calls
            .get_mut(exit.owner_call - 1)
            .expect("scope retains its original call")
            .env;
        match exit.state.phase() {
            cancellation::ExitPhase::Prepare => exit.state.begin(),
            cancellation::ExitPhase::WaitTerminal => {
                exit.state.prepare_terminal(host, env);
                if exit.state.phase() == cancellation::ExitPhase::WaitTerminal {
                    let runtime = host.task_runtime.as_ref().expect("prepared runtime batch");
                    if let cancellation::TerminalProgress::Pending { first_index, .. } =
                        exit.state.poll_terminal(runtime, Instant::now())
                    {
                        exit.first_index = Some(first_index);
                        self.state = MachineState::Waiting;
                    }
                }
            }
            cancellation::ExitPhase::ReleaseOwners => exit.state.release_owners(host),
            cancellation::ExitPhase::DropHandles => {
                exit.state.drop_handles(host);
                let capacity = std::mem::take(&mut exit.handle_capacity);
                self.release_exit_handle_capacity(capacity);
            }
            cancellation::ExitPhase::Teardown => exit.state.teardown(host, env, Env::pop_scope),
            cancellation::ExitPhase::DeadlineCheck => exit.state.deadline_check(host),
            cancellation::ExitPhase::Arbitrate => exit.state.arbitrate(host),
            cancellation::ExitPhase::Discard => exit.state.discard(host),
            cancellation::ExitPhase::Done => {
                let exit = self.exits.pop().expect("settled scope exit");
                assert_eq!(exit.handle_capacity, 0);
                let source_span = self
                    .resources
                    .as_ref()
                    .expect("live prefix")
                    .code
                    .span(exit.span);
                let result = exit.state.into_result();
                self.metrics.retired_exit_frames += 1;
                // Test-fuel failure can occur while the original primary is
                // retained by this exit. Finish actual arbitration first,
                // then retire that outgoing owner under the same context.
                if self.pending.is_some() {
                    let carried = match &result {
                        Ok(flow) => cancellation::ScopeExit::cleanup_context(flow),
                        Err(error) => error.scope_cleanup(),
                    };
                    let mut context = host
                        .scope_cleanup_context(carried)
                        .unwrap_or_else(|| CancellationContext::new(TerminationReason::Cancelled));
                    if let Some(Err(error)) = &mut self.pending {
                        error.remember_scope_cleanup(context);
                    }
                    host.discard_flow(result, context, source_span);
                    if let Some(release) = current_cleanup_context() {
                        context.cleanup_deadline =
                            context.cleanup_deadline.min(release.cleanup_deadline);
                    }
                    if let Some(Err(error)) = &mut self.pending {
                        error.remember_scope_cleanup(context);
                    }
                    return;
                }
                if !matches!(&result, Ok(Flow::Continue)) {
                    self.pending = Some(result);
                }
            }
        }
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
        // Reserve the persistent scope-exit header before this call creates
        // its real Env or performs parameter/body effects. One active scope
        // exit is sufficient because it settles before any parent work pops.
        self.reserve_exits(1, span)?;
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
            let param_span = if self.captures.is_some() {
                // Function-value parameters belong to this invocation, rather
                // than the producer's first call used to compile the code.
                span
            } else {
                self.code.span(param.span)
            };
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

    fn step(&mut self, step: Work, hooks: &mut impl ExecutionHooks) -> KuResult<()> {
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
                    ExprCode::VariableOwning(name) => {
                        self.reserve_values(1, span)?;
                        let value = self.frame_owned_value(name, span)?;
                        self.emit(value, span)?;
                    }
                    ExprCode::Await(operand) => {
                        // Admission precedes operand effects, as in eval(Await).
                        hooks.host().reject_cleanup_submission(span)?;
                        self.reserve_work(2, span)?;
                        push(&mut self.work, Work::AwaitOperand(expr.span));
                        push(&mut self.work, Work::Eval(operand));
                    }
                    ExprCode::Propagate(operand) => {
                        self.reserve_work(2, span)?;
                        push(&mut self.work, Work::Propagate(expr.span));
                        push(&mut self.work, Work::Eval(operand));
                    }
                    ExprCode::AsyncStart { function, args } => {
                        self.reserve_work(1, span)?;
                        push(
                            &mut self.work,
                            Work::PrepareArguments {
                                function,
                                args,
                                span: expr.span,
                                dispatch: ArgumentDispatch::NamedAsync,
                            },
                        );
                    }
                    ExprCode::AsyncSelfStart {
                        function,
                        args,
                        callee_span,
                    } => {
                        self.reserve_work(2, span)?;
                        push(
                            &mut self.work,
                            Work::PrepareArguments {
                                function,
                                args,
                                span: expr.span,
                                dispatch: ArgumentDispatch::LocalAsync,
                            },
                        );
                        // Function-value calls evaluate the self callable before
                        // their arguments. Its original tick remains observable.
                        push(&mut self.work, Work::SelfCallee(callee_span));
                    }
                    ExprCode::Unary(op, operand) => {
                        self.reserve_work(2, span)?;
                        push(&mut self.work, Work::Unary(op, expr.span));
                        push(&mut self.work, Work::Eval(operand));
                    }
                    ExprCode::TerminalOk(operand) => {
                        self.reserve_work(2, span)?;
                        push(&mut self.work, Work::WrapTerminalOk(expr.span));
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
                                dispatch: ArgumentDispatch::Synchronous,
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
            Work::WrapTerminalOk(span_id) => {
                let span = self.code.span(span_id);
                let value = self.value();
                // Admission allows this only in a Result root's terminal
                // return, with a scalar operand. Result itself never becomes
                // a copyable local, argument or nested scalar expression.
                scalar(&value, span)?;
                self.emit(
                    Value::Result {
                        ok: true,
                        value: Box::new(value),
                    },
                    span,
                )?;
            }
            Work::AwaitOperand(span) => {
                let source_span = self.code.span(span);
                // Reserve before moving the operand owner into its ticket.
                self.reserve_work(1, source_span)?;
                self.reserve_values(1, source_span)?;
                let value = self.value();
                let Value::Task(task) = value else {
                    return Err(KuError::runtime(
                        format!("await expects task but got {}", value.type_name()),
                        source_span,
                    ));
                };
                assert!(
                    self.awaiting.is_none(),
                    "one active await expression per machine"
                );
                self.awaiting = Some(AwaitFrame {
                    ticket: crate::runtime::task::AwaitTicket::begin(task),
                    span,
                });
                push(&mut self.work, Work::AwaitResume);
            }
            Work::AwaitResume => {
                let awaiting = self.awaiting.as_mut().expect("retained await expression");
                match awaiting.ticket.poll() {
                    crate::runtime::task::AwaitPoll::Pending => {
                        // This marker has just vacated one reserved slot. Poll
                        // does not repeat the expression tick or claim.
                        push(&mut self.work, Work::AwaitResume);
                        self.state = MachineState::Waiting;
                    }
                    crate::runtime::task::AwaitPoll::Ready => {
                        let AwaitFrame { ticket, span } =
                            self.awaiting.take().expect("one await transfer");
                        let result =
                            ticket.finish_with(|result| hooks.host().observe_termination(result));
                        self.emit(result?, self.code.span(span))?;
                    }
                }
            }
            Work::Propagate(span) => {
                let source_span = self.code.span(span);
                match self.value() {
                    Value::Borrowed(_) => {
                        return Err(KuError::runtime(
                            "cannot unwrap a borrowed Result; clone to create an owned value",
                            source_span,
                        ));
                    }
                    Value::Result { ok: true, value } => self.emit(*value, source_span)?,
                    Value::Result { ok: false, value } => {
                        self.pending = Some(Ok(Flow::fail(*value)));
                    }
                    other => {
                        return Err(KuError::runtime(
                            format!("'?' expects result but got {}", other.type_name()),
                            source_span,
                        ));
                    }
                }
            }
            Work::PrepareArguments {
                function,
                args,
                span,
                dispatch,
            } => {
                let source_span = self.code.span(span);
                self.reserve_work(2, source_span)?;
                let staged = self.argument_storage(args.len, source_span)?;
                let previous_observers = match self.register_call_observer(hooks, source_span) {
                    Ok(previous) => previous,
                    Err(error) => {
                        let capacity = staged.capacity();
                        drop(staged);
                        self.release_argument_capacity(capacity);
                        return Err(error);
                    }
                };
                let exit_index = self.work.len();
                let value_base = self.values.len();
                push(
                    &mut self.work,
                    Work::ArgumentsExit(ArgumentFrame {
                        span,
                        value_base,
                        leases: Vec::new(),
                        staged,
                        previous_observers,
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
                        dispatch,
                    },
                );
            }
            Work::Arguments {
                function,
                args,
                next,
                exit_index,
                span,
                dispatch,
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
                            dispatch,
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
                    if !matches!(dispatch, ArgumentDispatch::Synchronous) {
                        let capacity = staged.capacity();
                        let result = self.start_async(
                            function,
                            staged,
                            depth,
                            span,
                            matches!(dispatch, ArgumentDispatch::LocalAsync),
                            hooks,
                        );
                        // The original argument buffer moved into the child
                        // factory (or was actually dropped by failed admission).
                        // Its parent marker now owns an empty buffer.
                        self.release_argument_capacity(capacity);
                        result?;
                    } else {
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
                self.begin_scope_exit(span, value_base, fallthrough, hooks);
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
                // Observation must span the callee's actual scope/finally and
                // the full argument epilogue, as in the original eval(Call).
                hooks
                    .host()
                    .caller_owners
                    .truncate(arguments.previous_observers);
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
                ChunkProgress::Waiting { .. } => self.wait_synchronously(hooks.host()),
                ChunkProgress::Finished { popped, result } => {
                    debug_assert!(popped <= budget.get());
                    return result;
                }
            }
        }
    }

    fn wait_synchronously(&mut self, host: &Interpreter) {
        // The legacy synchronous adapter cannot hand its Rust caller a pause.
        // It uses the same ticket, preserving its old bounded helper behavior.
        // Production runner callers instead expose waiting() to LocalJobFrames.
        assert_eq!(self.state, MachineState::Waiting);
        let guard = self.finally.context().map(CleanupGuard::enter);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(awaiting) = &mut self.awaiting {
                assert_eq!(self.exits.len(), 0);
                awaiting.ticket.drive_sync();
            } else {
                let exit = self
                    .exits
                    .get_mut(0)
                    .expect("waiting scope retains its exit");
                if let Some(first_index) = exit.first_index {
                    exit.state.wait_pending_once(
                        host.task_runtime.as_ref().expect("pending scope runtime"),
                        first_index,
                    );
                }
            }
        }));
        let release = self.scope_release_context();
        self.finally.tighten(release);
        self.finally.tighten(current_cleanup_context());
        if result.is_err() {
            self.aborted_release = merge_release_context(current_cleanup_context(), release);
            self.state = MachineState::Aborted;
        }
        drop(guard);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
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
            || self.awaiting.is_some()
            || self.exits.len() != 0
            || self.argument_capacity != 0
            || self.argument_bytes != 0
            || self.exit_handle_capacity != 0
            || self.exit_handle_bytes != 0
            || self.call_depth != self.initial_depth
        {
            return Err(self.lifecycle_error("prepare undrained next run"));
        }
        self.state = MachineState::Ready;
        self.fallback_release.set(None);
        self.fallback_metadata = None;
        Ok(())
    }

    fn start_once(
        &mut self,
        entry: &str,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
    ) -> KuResult<()> {
        let function = self.code.find_function(entry);
        let span_id = function.map(|id| self.code.function(id).span);
        self.start_selected_once(function, args, depth, test_fuel, span_id, entry)
    }

    fn start_function_once(
        &mut self,
        function: FnId,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
        span: SpanId,
    ) -> KuResult<()> {
        self.start_selected_once(Some(function), args, depth, test_fuel, Some(span), "")
    }

    fn start_selected_once(
        &mut self,
        function: Option<FnId>,
        args: &[Value],
        depth: usize,
        test_fuel: bool,
        span_id: Option<SpanId>,
        entry: &str,
    ) -> KuResult<()> {
        if self.state != MachineState::Ready {
            return Err(self.lifecycle_error("start"));
        }
        #[cfg(not(test))]
        let _ = test_fuel;
        let span = span_id.map(|id| self.code.span(id)).unwrap_or_default();
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
            let invocation_span = span_id.expect("admitted entry has an invocation span");
            self.reserve_values(args.len(), span)?;
            self.enter(function, args, depth, invocation_span)
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
        if !matches!(self.state, MachineState::Running | MachineState::Waiting) {
            return Err(self.lifecycle_error("dispatch"));
        }
        self.state = MachineState::Running;
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
            self.aborted_release =
                merge_release_context(current_cleanup_context(), self.scope_release_context());
        }
        self.finally.tighten(self.scope_release_context());
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
            if self.exits.len() != 0 {
                window.begin(merge_release_context(
                    self.finally.context(),
                    self.scope_release_context(),
                ));
                popped += 1;
                #[cfg(test)]
                if self.test_fuel && self.pending.is_none() {
                    match self.remaining_dispatches.checked_sub(1) {
                        Some(remaining) => self.remaining_dispatches = remaining,
                        None => {
                            self.pending = Some(Err(KuError::runtime(
                                "internal execution prototype test fuel exhausted",
                                self.entry_span,
                            )))
                        }
                    }
                }
                self.metrics.dispatches += 1;
                self.drive_scope_exit(hooks);
                self.finally.tighten(self.scope_release_context());
                window.finish(&mut self.finally);
                if self.state == MachineState::Waiting {
                    return Ok(ChunkProgress::Waiting { popped });
                }
                continue;
            }
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
            if self.state == MachineState::Waiting {
                return Ok(ChunkProgress::Waiting { popped });
            }
        }
        if self.work.len() != 0 || self.exits.len() != 0 {
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
            if matches!(&step, Work::AwaitResume) {
                drop(self.awaiting.take());
            }
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
                if matches!(&step, Work::AwaitResume) {
                    drop(self.awaiting.take());
                }
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
        } else if let Err(error) = self.step(step, hooks) {
            self.pending = Some(Err(error));
        }
    }

    fn finish_run(&mut self) -> KuResult<Flow> {
        assert!(
            self.awaiting.is_none(),
            "finished execution retired its await"
        );
        assert_eq!(self.calls.len(), 0);
        assert_eq!(self.values.len(), 0);
        assert_eq!(self.argument_capacity, 0);
        assert_eq!(self.argument_bytes, 0);
        assert_eq!(self.exits.len(), 0);
        assert_eq!(self.exit_handle_capacity, 0);
        assert_eq!(self.exit_handle_bytes, 0);
        assert_eq!(self.finally.frames.len(), 0);
        self.metrics.remaining_calls = self.calls.len();
        self.metrics.remaining_work = self.work.len();
        self.metrics.remaining_values = self.values.len();
        self.metrics.remaining_finally_frames = self.finally.frames.len();
        self.metrics.remaining_exit_frames = self.exits.len();
        self.metrics.logical_depth = self.call_depth;
        assert_eq!(self.call_depth, self.initial_depth);
        self.metrics.retained_backing_bytes = self.calls.charged_storage_bytes()
            + self.work.charged_storage_bytes()
            + self.values.charged_storage_bytes()
            + self.finally.frames.charged_storage_bytes()
            + self.exits.charged_storage_bytes()
            + self.argument_bytes
            + self.exit_handle_bytes;
        self.metrics.peak_backing_bytes_upper_bound = self.calls.peak_storage_bytes()
            + self.work.peak_storage_bytes()
            + self.values.peak_storage_bytes()
            + self.finally.frames.peak_storage_bytes()
            + self.exits.peak_storage_bytes()
            + self.peak_argument_bytes
            + self.peak_exit_handle_bytes;
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

// Prepared while the producer still owns the callable. Eligible code borrows
// its source during compilation and keeps only shared flat code afterward.
// Legacy AST copies retain their original producer-side panic boundary.
pub(super) struct PreparedAsyncFunctionValue(PreparedAsyncFunctionValueKind);

enum PreparedAsyncFunctionValueKind {
    Flat {
        code: CodeHandle,
        function: FnId,
        captures: Env,
        args: Vec<Value>,
        span: SpanId,
    },
    Legacy(OwnedFunctionValueCall),
    Rejected {
        error: KuError,
        captures: Env,
        args: Vec<Value>,
    },
}

pub(super) struct OwnedFunctionValueCall {
    params: Vec<String>,
    param_modes: Vec<ParamMode>,
    body: Vec<Stmt>,
    captures: Env,
    self_name: Option<String>,
    args: Vec<Value>,
    span: Span,
}

pub(super) struct ClaimedLegacyFunctionValue {
    // With the project's Rust 1.89, the original one-shot closure drops its
    // Interpreter before params/modes/body/captures/self metadata. Keep that
    // observed order in one owner; destructuring locals would reverse it.
    interpreter: Interpreter,
    call: OwnedFunctionValueCall,
}

pub(super) struct ClaimedRejectedFunctionValue {
    // A selected admission error still owns its child-side release window.
    // Its argument buffer is consumed first; the retained host then precedes
    // the capture template in the same order as the original callable owner.
    _interpreter: Interpreter,
    _captures: Env,
    args: Vec<Value>,
    error: Option<KuError>,
}

pub(super) fn prepare_async_function_value(
    interpreter: &Interpreter,
    call: FunctionValueCall<'_>,
) -> PreparedAsyncFunctionValue {
    let limits = Limits::default();
    if code::eligible_claimed_async_local(&interpreter.functions, &call, limits) {
        match CodeHandle::compile_claimed_async_local(&interpreter.functions, &call, limits) {
            Ok(code) => {
                let function = code
                    .find_function(call.self_name.as_deref().expect("admitted self name"))
                    .expect("compiled local async entry");
                let span = code.function(function).span;
                return PreparedAsyncFunctionValue(PreparedAsyncFunctionValueKind::Flat {
                    code,
                    function,
                    captures: call.captures.clone(),
                    args: call.args,
                    span,
                });
            }
            Err(error @ AdmissionError::Rejected(_)) => {
                // Selection is complete. Its admission error belongs to the
                // original deferred child; restarting the legacy body would
                // bypass the selected storage budget.
                return PreparedAsyncFunctionValue(PreparedAsyncFunctionValueKind::Rejected {
                    error: error.into_error(),
                    captures: call.captures.clone(),
                    args: call.args,
                });
            }
            Err(AdmissionError::Unsupported(_)) => {}
        }
    }
    PreparedAsyncFunctionValue(PreparedAsyncFunctionValueKind::Legacy(
        OwnedFunctionValueCall {
            params: call.params.to_vec(),
            param_modes: call.param_modes.to_vec(),
            body: call.body.to_vec(),
            captures: call.captures.clone(),
            self_name: call.self_name.clone(),
            args: call.args,
            span: call.span,
        },
    ))
}

// Keep the flat runner inline: its actual typed header is charged by the
// claimant-local driver. Boxing the larger variant would hide that backing
// from the header budget. The legacy variant owns the original one-shot call.
#[allow(clippy::large_enum_variant)]
pub(super) enum ClaimedFunctionValueTask {
    Flat(ClaimedNamedTask),
    Legacy {
        call: Option<ClaimedLegacyFunctionValue>,
        task_id: i64,
        finished: bool,
    },
    Rejected {
        call: Option<ClaimedRejectedFunctionValue>,
        task_id: i64,
        finished: bool,
    },
}

impl ClaimedFunctionValueTask {
    pub(super) fn new(interpreter: Interpreter, prepared: PreparedAsyncFunctionValue) -> Self {
        match prepared.0 {
            PreparedAsyncFunctionValueKind::Flat {
                code,
                function,
                captures,
                args,
                span,
            } => Self::Flat(ClaimedNamedTask::from_local_code(
                interpreter,
                code,
                function,
                captures,
                args,
                span,
            )),
            PreparedAsyncFunctionValueKind::Legacy(call) => Self::Legacy {
                call: Some(ClaimedLegacyFunctionValue { interpreter, call }),
                task_id: crate::runtime::task::current_task_id(),
                finished: false,
            },
            PreparedAsyncFunctionValueKind::Rejected {
                error,
                captures,
                args,
            } => Self::Rejected {
                call: Some(ClaimedRejectedFunctionValue {
                    _interpreter: interpreter,
                    _captures: captures,
                    args,
                    error: Some(error),
                }),
                task_id: crate::runtime::task::current_task_id(),
                finished: false,
            },
        }
    }
}

impl crate::runtime::task::LocalTaskRunner for ClaimedFunctionValueTask {
    fn dispatch(&mut self, budget: NonZeroUsize) -> std::ops::ControlFlow<KuResult<Value>> {
        match self {
            Self::Flat(runner) => crate::runtime::task::LocalTaskRunner::dispatch(runner, budget),
            Self::Legacy {
                call,
                task_id,
                finished,
            } => {
                assert!(!*finished, "a finished function-value runner cannot resume");
                assert_eq!(
                    crate::runtime::task::current_task_id(),
                    *task_id,
                    "function-value task resumes on its original Task"
                );
                *finished = true;
                let legacy = call.take().expect("one legacy function-value call");
                let mut once = crate::runtime::task::OnceTaskRunner::new(move || {
                    let mut legacy = legacy;
                    let args = std::mem::take(&mut legacy.call.args);
                    legacy.interpreter.call_function_value_direct(
                        FunctionValueCall {
                            params: &legacy.call.params,
                            param_modes: &legacy.call.param_modes,
                            body: &legacy.call.body,
                            captures: &legacy.call.captures,
                            self_name: &legacy.call.self_name,
                            is_async: true,
                            args,
                            span: legacy.call.span,
                            depth: 0,
                        },
                        true,
                    )
                });
                crate::runtime::task::LocalTaskRunner::dispatch(&mut once, budget)
            }
            Self::Rejected {
                call,
                task_id,
                finished,
            } => {
                assert!(!*finished, "a finished rejected runner cannot resume");
                assert_eq!(
                    crate::runtime::task::current_task_id(),
                    *task_id,
                    "rejected function-value task resumes on its original Task"
                );
                *finished = true;
                let mut rejected = call.take().expect("one selected admission error");
                let error = rejected.error.take().expect("sole child admission error");
                drop(std::mem::take(&mut rejected.args));
                std::ops::ControlFlow::Break(Err(error))
            }
        }
    }

    fn waiting(&mut self) -> Option<&mut crate::runtime::task::AwaitTicket> {
        match self {
            Self::Flat(runner) => crate::runtime::task::LocalTaskRunner::waiting(runner),
            Self::Legacy { .. } | Self::Rejected { .. } => None,
        }
    }

    fn cleanup_wait_once(&mut self) {
        if let Self::Flat(runner) = self {
            crate::runtime::task::LocalTaskRunner::cleanup_wait_once(runner);
        }
    }
}

// Created only by the already-claimed Task's Send factory. The runner and its
// owned Machine stay on that claimant thread beneath the original job catch.
// More continues immediately; Waiting exposes the retained ticket to its driver.
pub(super) struct ClaimedNamedTask {
    // Preserve the execution prefix's Drop before its Interpreter host.
    machine: Option<Machine>,
    interpreter: Interpreter,
    legacy: Option<(FnDecl, Vec<Value>)>,
    outcome: Option<KuResult<Value>>,
    span: Span,
    task_id: i64,
    finished: bool,
}

impl ClaimedNamedTask {
    fn from_code(
        interpreter: Interpreter,
        code: CodeHandle,
        function: FnId,
        args: Vec<Value>,
        span: Span,
    ) -> Self {
        let mut machine = Machine::from_code(code, Limits::default(), interpreter.call_depth);
        let entry_code = machine.code.clone();
        let entry = entry_code.symbol(entry_code.function(function).name);
        let outcome = machine.start_once(entry, &args, 0, false).err().map(Err);
        // Scalar arguments have entered the actual child Env; release the
        // transferred factory buffer before the first dispatch, as before.
        drop(args);
        Self {
            machine: Some(machine),
            interpreter,
            legacy: None,
            outcome,
            span,
            task_id: crate::runtime::task::current_task_id(),
            finished: false,
        }
    }

    fn from_local_code(
        interpreter: Interpreter,
        code: CodeHandle,
        function: FnId,
        captures: Env,
        args: Vec<Value>,
        span: SpanId,
    ) -> Self {
        let mut machine = Machine::from_code(code, Limits::default(), interpreter.call_depth);
        machine.captures = Some(captures);
        let source_span = machine.code.span(span);
        let outcome = machine
            .start_function_once(function, &args, 0, false, span)
            .err()
            .map(Err);
        drop(args);
        Self {
            machine: Some(machine),
            interpreter,
            legacy: None,
            outcome,
            span: source_span,
            task_id: crate::runtime::task::current_task_id(),
            finished: false,
        }
    }

    pub(super) fn new(
        interpreter: Interpreter,
        function: FnDecl,
        args: Vec<Value>,
        span: Span,
    ) -> Self {
        let task_id = crate::runtime::task::current_task_id();
        let mut runner = Self {
            machine: None,
            interpreter,
            legacy: Some((function, args)),
            outcome: None,
            span,
            task_id,
            finished: false,
        };
        let (function, args) = runner.legacy.as_ref().expect("initial named task body");
        let limits = Limits::default();
        let source = runner.interpreter.functions.clone();
        if args.iter().any(|value| scalar(value, span).is_err())
            || !code::eligible_claimed_async_entry(&source, function, limits)
        {
            return runner;
        }
        let code = match CodeHandle::compile_claimed_async_entry(&source, function, limits) {
            Ok(code) => code,
            Err(AdmissionError::Unsupported(_)) => return runner,
            Err(error @ AdmissionError::Rejected(_)) => {
                runner.outcome = Some(Err(error.into_error()));
                drop(runner.legacy.take());
                return runner;
            }
        };
        let mut machine = Machine::from_code(code, limits, runner.interpreter.call_depth);
        if let Err(error) = machine.start_once(&function.name, args, 0, false) {
            runner.outcome = Some(Err(error));
        }
        runner.machine = Some(machine);
        // The admitted parameters are Copy scalars. No AST or borrowed source
        // remains in the Machine, and no Owned argument is implicitly cloned.
        drop(runner.legacy.take());
        runner
    }
}

impl crate::runtime::task::LocalTaskRunner for ClaimedNamedTask {
    fn dispatch(&mut self, budget: NonZeroUsize) -> std::ops::ControlFlow<KuResult<Value>> {
        assert!(!self.finished, "a finished named task runner cannot resume");
        assert_eq!(
            crate::runtime::task::current_task_id(),
            self.task_id,
            "named task resumes on its original Task"
        );
        let result = if let Some(outcome) = self.outcome.take() {
            outcome
        } else if let Some(machine) = &mut self.machine {
            match machine.dispatch_chunk(budget, &mut self.interpreter) {
                Ok(ChunkProgress::More { popped }) => {
                    debug_assert_eq!(popped, budget.get());
                    return std::ops::ControlFlow::Continue(());
                }
                Ok(ChunkProgress::Waiting { popped }) => {
                    debug_assert!(popped <= budget.get());
                    return std::ops::ControlFlow::Continue(());
                }
                Ok(ChunkProgress::Finished { popped, result }) => {
                    debug_assert!(popped <= budget.get());
                    let kind = if machine.captures.is_some() {
                        CallKind::FunctionValue
                    } else {
                        CallKind::Named
                    };
                    result.and_then(|flow| flow.into_call_value(self.span, kind.label()))
                }
                Err(error) => Err(error),
            }
        } else {
            let (function, args) = self.legacy.take().expect("one legacy task call");
            self.interpreter
                .call_function_direct(function, args, self.span, 0)
        };
        self.finished = true;
        std::ops::ControlFlow::Break(result)
    }

    fn waiting(&mut self) -> Option<&mut crate::runtime::task::AwaitTicket> {
        self.machine.as_mut().and_then(|machine| {
            if machine.state == MachineState::Waiting {
                machine.awaiting.as_mut().map(|frame| &mut frame.ticket)
            } else {
                None
            }
        })
    }

    fn cleanup_wait_once(&mut self) {
        if let Some(machine) = &mut self.machine {
            if machine.state == MachineState::Waiting && machine.exits.len() != 0 {
                assert!(machine.awaiting.is_none());
                machine.wait_synchronously(&self.interpreter);
            }
        }
    }
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
        let mut release = merge_release_context(
            merge_release_context(self.aborted_release, self.finally.context()),
            self.scope_release_context(),
        );
        let guard = release.map(CleanupGuard::enter);
        let awaiting = self.awaiting.take();
        let await_drop = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(awaiting)));
        let mut first = await_drop.err();
        // Empty the actual header in place before destroying its saved Cell.
        // A destructor's tighter D therefore reaches the following real Env
        // prefix even when the header phase itself panicked or was abandoned.
        while let Some(mut exit) = self.exits.pop() {
            if let Some(panic) = exit.state.abort_release() {
                retain_first_panic(&mut first, panic);
            }
            release = merge_release_context(release, exit.state.release_context());
            let capacity = exit.handle_capacity;
            let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(exit)));
            self.release_exit_handle_capacity(capacity);
            if let Err(panic) = dropped {
                retain_first_panic(&mut first, panic);
            }
        }
        release = merge_release_context(release, current_cleanup_context());
        let exits = std::mem::replace(&mut self.exits, FrameStack::new(self.work_limits));
        drop(exits);
        let tightened = release.map(CleanupGuard::enter);
        let prefix = self.resources.take();
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(prefix)));
        self.finally.tighten(current_cleanup_context());
        // Restore before ordinary outer primary outcomes are retired.
        drop(tightened);
        drop(guard);
        if let Err(panic) = dropped {
            retain_first_panic(&mut first, panic);
        }
        if let Some(panic) = self.finally.drain() {
            retain_first_panic(&mut first, panic);
        }
        if let Some(panic) = first {
            std::panic::resume_unwind(panic);
        }
    }
}
