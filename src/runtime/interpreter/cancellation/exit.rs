//! Typed resource arbitration shared by evaluator adapters and local exits.
//!
//! Only outcomes and collected Task proxies live here. The adapters still own
//! Env, leases, and temporary values; no reference, teardown closure, or TLS
//! guard is stored in these states. The claimant retains the physical frame;
//! terminal observation does not acknowledge its resource or queue retirement.

use super::*;
use crate::runtime::task::{
    CancelBatchPoll, RetirementBatchPoll, TaskCancelBatch, TaskHandle, TaskRetirementBatch,
    TaskRetirementWitness,
};
use std::cell::Cell;

enum KnownTasks {
    Collected(Vec<TaskHandle>),
    Prepared(TaskCancelBatch<Vec<TaskHandle>>),
    Empty,
}

impl KnownTasks {
    fn handles(&self) -> &[TaskHandle] {
        match self {
            Self::Collected(handles) => handles,
            Self::Prepared(batch) => batch.handles(),
            Self::Empty => &[],
        }
    }

    fn prepare_once(&mut self, context: RuntimeTermination) {
        if matches!(self, Self::Prepared(_)) {
            return;
        }
        let Self::Collected(handles) = std::mem::replace(self, Self::Empty) else {
            unreachable!("known cancellation handles are prepared once");
        };
        *self = Self::Prepared(TaskCancelBatch::prepare(handles, context));
    }

    fn poll_prepared(&mut self, runtime: &TaskRuntime, now: Instant) -> CancelBatchPoll {
        let Self::Prepared(batch) = self else {
            unreachable!("polling uses the already prepared batch");
        };
        batch.poll(runtime, now)
    }

    fn wait_pending_once(&self, runtime: &TaskRuntime, first_index: usize) -> KuResult<()> {
        let Self::Prepared(batch) = self else {
            unreachable!("waiting uses the already prepared batch");
        };
        runtime.wait_prepared_cancel_once(batch, first_index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum ExitPhase {
    Prepare,
    WaitTerminal,
    ReleaseOwners,
    DropHandles,
    Teardown,
    WaitRetirement,
    DeadlineCheck,
    Arbitrate,
    Discard,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum TerminalProgress {
    Pending {
        first_index: usize,
        unfinished: usize,
    },
    Settled,
}

/// The sole outcome owner through all scope/call/argument arbitration phases.
/// This header adds no hidden heap storage: collected Vec backing moves
/// into the prepared batch, never into another copied owner or payload.
pub(in crate::runtime::interpreter) struct ExitState<T: ScopeExit> {
    phase: ExitPhase,
    outcome: Option<KuResult<T>>,
    tasks: KnownTasks,
    retirement: Option<TaskRetirementBatch>,
    collected: Option<KuResult<()>>,
    settled: Option<KuResult<()>>,
    cleanup: Option<KuResult<()>>,
    expired_before: bool,
    span: Span,
    // Metadata cause follows scope_cleanup_context's precedence. Actual TLS
    // drops can instead inherit an earlier ambient cleanup cause. Keep both;
    // only their absolute deadlines tighten, never overwrite one with another.
    context: Option<RuntimeTermination>,
    release_context: Cell<Option<RuntimeTermination>>,
}

fn effective_release_context(meta: Option<RuntimeTermination>) -> Option<RuntimeTermination> {
    match (current_cleanup_context(), meta) {
        (Some(mut ambient), Some(meta)) => {
            ambient.cleanup_deadline = ambient.cleanup_deadline.min(meta.cleanup_deadline);
            Some(ambient)
        }
        (ambient, meta) => ambient.or(meta),
    }
}

fn tighten_release(saved: &mut Option<RuntimeTermination>) {
    if let Some(current) = current_cleanup_context() {
        match saved {
            Some(saved) => {
                saved.cleanup_deadline = saved.cleanup_deadline.min(current.cleanup_deadline)
            }
            None => *saved = Some(current),
        }
    }
}

/// A synchronous phase borrows only the saved context, never its resource
/// payload. The Cell adds no heap allocation and has the original Option's
/// layout. Sample before restoring TLS on both return and native unwind; no
/// guard is retained in a paused exit state.
pub(in crate::runtime::interpreter) struct PhaseWindow<'a> {
    saved: &'a Cell<Option<RuntimeTermination>>,
    guard: Option<CleanupGuard>,
}

impl<'a> PhaseWindow<'a> {
    pub(in crate::runtime::interpreter) fn enter(
        saved: &'a Cell<Option<RuntimeTermination>>,
    ) -> Self {
        Self::enter_context(saved, saved.get())
    }

    fn enter_context(
        saved: &'a Cell<Option<RuntimeTermination>>,
        context: Option<RuntimeTermination>,
    ) -> Self {
        Self {
            saved,
            guard: context.map(CleanupGuard::enter),
        }
    }
}

impl Drop for PhaseWindow<'_> {
    fn drop(&mut self) {
        let mut saved = self.saved.get();
        tighten_release(&mut saved);
        self.saved.set(saved);
        drop(self.guard.take());
    }
}

fn remember_context<T: ScopeExit>(
    outcome: &mut Option<KuResult<T>>,
    context: Option<RuntimeTermination>,
) {
    if let Some(context) = context {
        match outcome.as_mut().expect("exit owns its outcome") {
            Ok(flow) => flow.remember_cleanup(context),
            Err(error) => error.remember_scope_cleanup(context),
        }
    }
}

fn settle_terminal(
    phase: &mut ExitPhase,
    settled: &mut Option<KuResult<()>>,
    result: KuResult<()>,
) {
    assert!(settled.is_none(), "terminal wait settles once");
    *settled = Some(result);
    *phase = ExitPhase::ReleaseOwners;
}

fn install_cleanup_error<T: ScopeExit>(
    outcome: &mut Option<KuResult<T>>,
    cleanup: &mut Option<KuResult<()>>,
    context: Option<RuntimeTermination>,
    span: Span,
) {
    let Err(mut error) = cleanup.take().expect("cleanup failure") else {
        unreachable!("cleanup error wins only on failure");
    };
    if let Some(context) = context {
        error.remember_scope_cleanup(context);
    }
    if error.runtime_termination().is_some() {
        let mut mapped = KuError::structured(
            crate::error::KuErrorKind::Runtime,
            "task",
            "shutdown_timeout",
            "scope cleanup did not finish before the bounded cleanup deadline",
            span,
        );
        if let Some(context) = context {
            mapped.remember_scope_cleanup(context);
        }
        error = mapped;
    }
    *outcome = Some(Err(error));
}

impl<T: ScopeExit> ExitState<T> {
    /// Read the sole retained primary without cloning or transferring it.
    /// The execution budget uses this only at stable dispatch boundaries.
    pub(in crate::runtime::interpreter) fn outcome(&self) -> Option<&KuResult<T>> {
        self.outcome.as_ref()
    }

    pub(in crate::runtime::interpreter) fn new(
        interpreter: &mut Interpreter,
        result: KuResult<T>,
        span: Span,
        handles: Vec<TaskHandle>,
        collected: KuResult<()>,
    ) -> Self {
        interpreter.observe_termination(&result);
        let carried = match &result {
            Ok(flow) => flow.cleanup_context(),
            Err(error) => error.scope_cleanup(),
        };
        let mut context = interpreter.scope_cleanup_context(carried);
        if context.is_none() && !handles.is_empty() {
            context = Some(CancellationContext::new(TerminationReason::Cancelled));
        }
        Self {
            phase: ExitPhase::Prepare,
            outcome: Some(result),
            tasks: KnownTasks::Collected(handles),
            retirement: None,
            collected: Some(collected),
            settled: None,
            cleanup: None,
            expired_before: false,
            span,
            context,
            release_context: Cell::new(effective_release_context(context)),
        }
    }

    pub(in crate::runtime::interpreter) fn phase(&self) -> ExitPhase {
        self.phase
    }

    /// Only budgeted explicit Machine exits reserve these state witnesses.
    /// Legacy adapters keep their original constructor and wait contract.
    pub(in crate::runtime::interpreter) fn new_with_retirement(
        interpreter: &mut Interpreter,
        result: KuResult<T>,
        span: Span,
        handles: Vec<TaskHandle>,
        collected: KuResult<()>,
        witnesses: Vec<TaskRetirementWitness>,
    ) -> Self {
        let mut state = Self::new(interpreter, result, span, handles, collected);
        if !witnesses.is_empty() {
            let context = state
                .release_context
                .get()
                .expect("retirement witnesses belong to a task exit context");
            state.retirement = Some(TaskRetirementBatch::from_reserved(witnesses, context));
        }
        state
    }

    pub(in crate::runtime::interpreter) fn retirement_capacity(&self) -> usize {
        self.retirement
            .as_ref()
            .map_or(0, TaskRetirementBatch::capacity)
    }

    pub(in crate::runtime::interpreter) fn context(&self) -> Option<RuntimeTermination> {
        self.context
    }

    pub(in crate::runtime::interpreter) fn release_context(&self) -> Option<RuntimeTermination> {
        self.release_context.get()
    }

    pub(in crate::runtime::interpreter) fn phase_window(&self) -> PhaseWindow<'_> {
        PhaseWindow::enter(&self.release_context)
    }

    #[cfg(test)]
    pub(super) fn phase_guard(&self) -> Option<CleanupGuard> {
        self.release_context.get().map(CleanupGuard::enter)
    }

    pub(in crate::runtime::interpreter) fn begin(&mut self) {
        assert_eq!(self.phase, ExitPhase::Prepare);
        let _window = PhaseWindow::enter(&self.release_context);
        self.expired_before = self
            .context
            .is_some_and(|context| Instant::now() >= context.cleanup_deadline);
        remember_context(&mut self.outcome, self.context);
        self.phase = ExitPhase::WaitTerminal;
    }

    pub(in crate::runtime::interpreter) fn prepare_terminal(
        &mut self,
        interpreter: &mut Interpreter,
        env: &Env,
    ) {
        if self.phase == ExitPhase::ReleaseOwners {
            assert!(self.settled.is_some());
            return;
        }
        assert_eq!(self.phase, ExitPhase::WaitTerminal);
        let _window = PhaseWindow::enter(&self.release_context);
        if matches!(&self.tasks, KnownTasks::Prepared(_)) {
            return;
        }
        if self.tasks.handles().is_empty() {
            settle_terminal(&mut self.phase, &mut self.settled, Ok(()));
        } else {
            let context = self.context.expect("owned scope cleanup has a context");
            if interpreter.termination.is_some() {
                interpreter.cancel_visible_children(env, context, self.span);
            }
            match &interpreter.task_runtime {
                Some(_) => {
                    // runtime is deliberately not retained: the adapter or
                    // claimant-local runner owns the unchanged runtime.
                    self.tasks.prepare_once(context);
                }
                None => {
                    // Preserve the old no-runtime fallback: raw requests use
                    // metadata cause, with no extra terminal waiting policy.
                    for handle in self.tasks.handles() {
                        handle.request_cancel_with(context);
                    }
                    settle_terminal(&mut self.phase, &mut self.settled, Ok(()));
                }
            }
        }
    }

    pub(in crate::runtime::interpreter) fn poll_terminal(
        &mut self,
        runtime: &TaskRuntime,
        now: Instant,
    ) -> TerminalProgress {
        // An empty/no-runtime prepare, or a prior terminal observation, has
        // already settled. No second poll may rebroadcast or count a timeout.
        if self.phase == ExitPhase::ReleaseOwners {
            assert!(self.settled.is_some());
            return TerminalProgress::Settled;
        }
        assert_eq!(self.phase, ExitPhase::WaitTerminal);
        let _window = PhaseWindow::enter(&self.release_context);
        match self.tasks.poll_prepared(runtime, now) {
            CancelBatchPoll::Pending {
                first_index,
                unfinished,
            } => TerminalProgress::Pending {
                first_index,
                unfinished,
            },
            CancelBatchPoll::TerminalObserved => {
                settle_terminal(&mut self.phase, &mut self.settled, Ok(()));
                TerminalProgress::Settled
            }
            CancelBatchPoll::TimedOut(context) => {
                settle_terminal(
                    &mut self.phase,
                    &mut self.settled,
                    Err(KuError::termination(context, Span::default())),
                );
                TerminalProgress::Settled
            }
        }
    }

    pub(in crate::runtime::interpreter) fn wait_pending_once(
        &mut self,
        runtime: &TaskRuntime,
        first_index: usize,
    ) {
        assert_eq!(self.phase, ExitPhase::WaitTerminal);
        let _window = PhaseWindow::enter(&self.release_context);
        if let Err(error) = self.tasks.wait_pending_once(runtime, first_index) {
            settle_terminal(&mut self.phase, &mut self.settled, Err(error));
        }
    }

    pub(in crate::runtime::interpreter) fn wait_terminal(
        &mut self,
        interpreter: &mut Interpreter,
        env: &Env,
    ) {
        self.prepare_terminal(interpreter, env);
        let Some(runtime) = &interpreter.task_runtime else {
            return;
        };
        while self.phase == ExitPhase::WaitTerminal {
            match self.poll_terminal(runtime, Instant::now()) {
                TerminalProgress::Pending { first_index, .. } => {
                    self.wait_pending_once(runtime, first_index);
                }
                TerminalProgress::Settled => return,
            }
        }
    }

    pub(in crate::runtime::interpreter) fn release_owners(
        &mut self,
        interpreter: &mut Interpreter,
    ) {
        assert_eq!(self.phase, ExitPhase::ReleaseOwners);
        let _window = PhaseWindow::enter(&self.release_context);
        if !self.tasks.handles().is_empty() {
            let context = interpreter
                .scope_cleanup_context(self.context)
                .expect("scope cleanup retains its context");
            let _release_guard = PhaseWindow::enter_context(&self.release_context, Some(context));
            for handle in self.tasks.handles() {
                handle.release_scope_owner(context);
            }
        }
        self.phase = ExitPhase::DropHandles;
    }

    pub(in crate::runtime::interpreter) fn drop_handles(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::DropHandles);
        let _window = PhaseWindow::enter(&self.release_context);
        let collected = self.collected.take().expect("collection result");
        let settled = self.settled.take().expect("settled result");
        if collected.is_err() && settled.is_err() {
            interpreter.record_suppressed_cleanup();
        }
        self.cleanup = Some(collected.and(settled));
        drop(std::mem::replace(&mut self.tasks, KnownTasks::Empty));
        self.context = interpreter.scope_cleanup_context(self.context);
        self.phase = ExitPhase::Teardown;
    }

    /// The adapter has performed its actual teardown in a short phase window.
    /// This state never owns, pops, or substitutes an empty physical Env.
    pub(in crate::runtime::interpreter) fn finish_teardown(
        &mut self,
        interpreter: &mut Interpreter,
    ) {
        assert_eq!(self.phase, ExitPhase::Teardown);
        let _window = PhaseWindow::enter(&self.release_context);
        self.context = interpreter.scope_cleanup_context(self.context);
        self.phase = if self.retirement.is_some() {
            ExitPhase::WaitRetirement
        } else {
            ExitPhase::DeadlineCheck
        };
    }

    fn remember_retirement_context(&mut self, context: RuntimeTermination) {
        if let Some(mut saved) = self.release_context.get() {
            saved.cleanup_deadline = saved.cleanup_deadline.min(context.cleanup_deadline);
            self.release_context.set(Some(saved));
        }
        if let Some(saved) = &mut self.context {
            // Metadata and ambient first causes remain separate. Only D joins.
            saved.cleanup_deadline = saved.cleanup_deadline.min(context.cleanup_deadline);
        }
    }

    fn finish_retirement(&mut self, interpreter: &Interpreter, result: KuResult<()>) {
        let cleanup = self
            .cleanup
            .as_mut()
            .expect("actual teardown precedes retirement");
        if cleanup.is_err() && result.is_err() {
            interpreter.record_suppressed_cleanup();
        }
        if cleanup.is_ok() {
            *cleanup = result;
        }
        self.phase = ExitPhase::DeadlineCheck;
        // Releasing the final runtime Arc can destroy its real queue storage.
        // Keep that actual Drop in the saved window, including native unwind.
        let _window = PhaseWindow::enter(&self.release_context);
        drop(self.retirement.take());
        self.context = interpreter.scope_cleanup_context(self.context);
    }

    pub(in crate::runtime::interpreter) fn poll_retirement(
        &mut self,
        interpreter: &Interpreter,
        now: Instant,
    ) -> TerminalProgress {
        assert_eq!(self.phase, ExitPhase::WaitRetirement);
        self.context = interpreter.scope_cleanup_context(self.context);
        if let Some(context) = self.context {
            self.remember_retirement_context(context);
        }
        let progress;
        let context;
        {
            let _window = PhaseWindow::enter(&self.release_context);
            let retirement = self.retirement.as_mut().expect("budgeted retirement batch");
            progress = retirement.poll(now);
            context = retirement.context();
        }
        self.remember_retirement_context(context);
        match progress {
            RetirementBatchPoll::Pending {
                first_index,
                unfinished,
            } => TerminalProgress::Pending {
                first_index,
                unfinished,
            },
            RetirementBatchPoll::Retired => {
                self.finish_retirement(interpreter, Ok(()));
                TerminalProgress::Settled
            }
            RetirementBatchPoll::TimedOut(context) => {
                self.finish_retirement(interpreter, Err(KuError::termination(context, self.span)));
                TerminalProgress::Settled
            }
        }
    }

    pub(in crate::runtime::interpreter) fn wait_retirement_once(
        &mut self,
        interpreter: &Interpreter,
        first_index: usize,
    ) {
        assert_eq!(self.phase, ExitPhase::WaitRetirement);
        self.context = interpreter.scope_cleanup_context(self.context);
        if let Some(context) = self.context {
            self.remember_retirement_context(context);
        }
        let (result, context) = {
            let _window = PhaseWindow::enter(&self.release_context);
            let retirement = self.retirement.as_mut().expect("budgeted retirement batch");
            let result = retirement.wait_once(first_index, self.span);
            (result, retirement.context())
        };
        self.remember_retirement_context(context);
        if let Err(error) = result {
            self.finish_retirement(interpreter, Err(error));
        }
    }

    pub(in crate::runtime::interpreter) fn teardown(
        &mut self,
        interpreter: &mut Interpreter,
        env: &mut Env,
        teardown: impl FnOnce(&mut Env),
    ) {
        assert_eq!(self.phase, ExitPhase::Teardown);
        {
            let _window = self.phase_window();
            teardown(env);
        }
        self.finish_teardown(interpreter);
    }

    pub(in crate::runtime::interpreter) fn deadline_check(
        &mut self,
        _interpreter: &mut Interpreter,
    ) {
        assert_eq!(self.phase, ExitPhase::DeadlineCheck);
        let _window = PhaseWindow::enter(&self.release_context);
        remember_context(&mut self.outcome, self.context);
        if let Some(context) = self.context {
            let cleanup = self.cleanup.as_mut().expect("cleanup result");
            if cleanup.is_ok()
                && (self.expired_before || Instant::now() >= context.cleanup_deadline)
            {
                *cleanup = Err(KuError::termination(context, self.span));
            }
        }
        self.phase = ExitPhase::Arbitrate;
    }

    pub(in crate::runtime::interpreter) fn arbitrate(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::Arbitrate);
        let _window = PhaseWindow::enter(&self.release_context);
        let typed_primary = matches!(self.outcome.as_ref(),
            Some(Err(error)) if error.runtime_termination().is_some());
        if typed_primary {
            if self.cleanup.as_ref().expect("cleanup result").is_err() {
                interpreter.record_suppressed_cleanup();
            }
            self.phase = ExitPhase::Done;
        } else if self.cleanup.as_ref().expect("cleanup result").is_err() {
            if self.context.is_some() {
                self.phase = ExitPhase::Discard;
            } else {
                // Same context-free cleanup failure policy as the evaluator.
                drop(self.outcome.take());
                install_cleanup_error(
                    &mut self.outcome,
                    &mut self.cleanup,
                    self.context,
                    self.span,
                );
                self.phase = ExitPhase::Done;
            }
        } else {
            self.phase = ExitPhase::Done;
        }
    }

    pub(in crate::runtime::interpreter) fn discard(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::Discard);
        let _window = PhaseWindow::enter(&self.release_context);
        let context = self.context.expect("discard carries its cleanup context");
        interpreter.discard_flow(
            self.outcome.take().expect("outgoing primary"),
            context,
            self.span,
        );
        self.context = interpreter.scope_cleanup_context(Some(context));
        install_cleanup_error(
            &mut self.outcome,
            &mut self.cleanup,
            self.context,
            self.span,
        );
        self.phase = ExitPhase::Done;
    }

    pub(in crate::runtime::interpreter) fn into_result(mut self) -> KuResult<T> {
        assert_eq!(self.phase, ExitPhase::Done);
        self.outcome.take().expect("final exit outcome")
    }

    /// Release an abandoned header while its saved deadline remains readable.
    /// The caller still owns the real Env and must apply this window to that
    /// later destruction. Never retry a phase that already panicked.
    pub(in crate::runtime::interpreter) fn abort_release(
        &mut self,
    ) -> Option<Box<dyn std::any::Any + Send + 'static>> {
        let _window = PhaseWindow::enter(&self.release_context);
        let resources = (
            std::mem::replace(&mut self.tasks, KnownTasks::Empty),
            self.outcome.take(),
            self.retirement.take(),
        );
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(resources))).err()
    }
}

impl<T: ScopeExit> Drop for ExitState<T> {
    fn drop(&mut self) {
        // Panic/abandonment of this header can only destroy what it owns. Actual
        // Env/lease/value ownership remains in the synchronous adapter. Keep
        // both known proxies and the sole primary inside the saved TLS window;
        // an expired deadline never skips necessary destruction.
        let _window = PhaseWindow::enter(&self.release_context);
        // Take both before invoking either destructor. Tuple drop glue keeps
        // the remaining outcome inside this guard when a Task destructor
        // panics, preserving the original handles-before-outcome order.
        drop((
            std::mem::replace(&mut self.tasks, KnownTasks::Empty),
            self.outcome.take(),
            self.retirement.take(),
        ));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscardPhase {
    Collect,
    WaitTerminal,
    DropHandles,
    DropOutcome,
    Done,
}

/// Discard intentionally retains its narrower original policy: a collection
/// error skips explicit broadcast/wait of partial handles, and no phase calls
/// release_scope_owner. The outcome's actual ordered Drop remains responsible.
pub(super) struct DiscardState<T: ScopeExit> {
    phase: DiscardPhase,
    outcome: Option<KuResult<T>>,
    tasks: KnownTasks,
    context: RuntimeTermination,
    release_context: Cell<Option<RuntimeTermination>>,
    span: Span,
}

impl<T: ScopeExit> DiscardState<T> {
    pub(super) fn new(result: KuResult<T>, context: RuntimeTermination, span: Span) -> Self {
        Self {
            phase: DiscardPhase::Collect,
            outcome: Some(result),
            tasks: KnownTasks::Collected(Vec::new()),
            context,
            release_context: Cell::new(effective_release_context(Some(context))),
            span,
        }
    }

    pub(super) fn run_sync(&mut self, interpreter: &Interpreter) {
        while self.phase != DiscardPhase::Done {
            let _window = PhaseWindow::enter(&self.release_context);
            match self.phase {
                DiscardPhase::Collect => {
                    let KnownTasks::Collected(tasks) = &mut self.tasks else {
                        unreachable!("discard collection owns its original Vec");
                    };
                    if let Some(Ok(flow)) = &self.outcome {
                        if flow.collect_tasks(tasks, self.span).is_ok() {
                            self.phase = DiscardPhase::WaitTerminal;
                        } else {
                            interpreter.record_suppressed_cleanup();
                            self.phase = DiscardPhase::DropHandles;
                        }
                    } else {
                        self.phase = DiscardPhase::DropHandles;
                    }
                }
                DiscardPhase::WaitTerminal => {
                    match &interpreter.task_runtime {
                        Some(runtime) => {
                            // Discard owns its collected proxies throughout this
                            // synchronous wait. The existing slice adapter uses
                            // the same prepared batch, with one broadcast only.
                            if runtime
                                .cancel_handles_and_wait(self.tasks.handles(), self.context)
                                .is_err()
                            {
                                interpreter.record_suppressed_cleanup();
                            }
                        }
                        None => {
                            for task in self.tasks.handles() {
                                task.request_cancel_with(self.context);
                            }
                        }
                    }
                    self.phase = DiscardPhase::DropHandles;
                }
                DiscardPhase::DropHandles => {
                    drop(std::mem::replace(&mut self.tasks, KnownTasks::Empty));
                    self.phase = DiscardPhase::DropOutcome;
                }
                DiscardPhase::DropOutcome => {
                    drop(self.outcome.take());
                    self.phase = DiscardPhase::Done;
                }
                DiscardPhase::Done => unreachable!("done discard is not dispatched"),
            }
        }
    }
}

impl<T: ScopeExit> Drop for DiscardState<T> {
    fn drop(&mut self) {
        let _window = PhaseWindow::enter(&self.release_context);
        drop((
            std::mem::replace(&mut self.tasks, KnownTasks::Empty),
            self.outcome.take(),
        ));
    }
}
