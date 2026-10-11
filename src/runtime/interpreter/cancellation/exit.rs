//! Typed resource arbitration shared by the synchronous evaluator adapters.
//!
//! Only outcomes and collected Task proxies live here. The adapters still own
//! Env, leases, and temporary values; no reference, teardown closure, or TLS
//! guard is stored in these states. This is not a TaskJob/Pending interface.

use super::*;
use crate::runtime::task::{TaskCancelBatch, TaskHandle};

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

    fn prepare(&mut self, context: RuntimeTermination) {
        let Self::Collected(handles) = std::mem::replace(self, Self::Empty) else {
            unreachable!("known cancellation handles are prepared once");
        };
        *self = Self::Prepared(TaskCancelBatch::prepare(handles, context));
    }

    fn wait(&mut self, runtime: &TaskRuntime) -> KuResult<()> {
        let Self::Prepared(batch) = self else {
            unreachable!("waiting uses the already prepared batch");
        };
        runtime.wait_prepared_cancel_batch(batch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExitPhase {
    Prepare,
    WaitTerminal,
    ReleaseOwners,
    DropHandles,
    Teardown,
    DeadlineCheck,
    Arbitrate,
    Discard,
    Done,
}

/// The sole outcome owner through all scope/call/argument arbitration phases.
/// This stack-local header adds no heap storage: collected Vec backing moves
/// into the prepared batch, never into another copied owner or payload.
pub(super) struct ExitState<T: ScopeExit> {
    phase: ExitPhase,
    outcome: Option<KuResult<T>>,
    tasks: KnownTasks,
    collected: Option<KuResult<()>>,
    settled: Option<KuResult<()>>,
    cleanup: Option<KuResult<()>>,
    expired_before: bool,
    span: Span,
    // Metadata cause follows scope_cleanup_context's precedence. Actual TLS
    // drops can instead inherit an earlier ambient cleanup cause. Keep both;
    // only their absolute deadlines tighten, never overwrite one with another.
    context: Option<RuntimeTermination>,
    release_context: Option<RuntimeTermination>,
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

impl<T: ScopeExit> ExitState<T> {
    pub(super) fn new(
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
            collected: Some(collected),
            settled: None,
            cleanup: None,
            expired_before: false,
            span,
            context,
            release_context: effective_release_context(context),
        }
    }

    pub(super) fn phase(&self) -> ExitPhase {
        self.phase
    }

    pub(super) fn context(&self) -> Option<RuntimeTermination> {
        self.context
    }

    pub(super) fn phase_guard(&self) -> Option<CleanupGuard> {
        self.release_context.map(CleanupGuard::enter)
    }

    fn remember_context(&mut self) {
        if let Some(context) = self.context {
            match self.outcome.as_mut().expect("exit owns its outcome") {
                Ok(flow) => flow.remember_cleanup(context),
                Err(error) => error.remember_scope_cleanup(context),
            }
        }
    }

    pub(super) fn begin(&mut self) {
        assert_eq!(self.phase, ExitPhase::Prepare);
        tighten_release(&mut self.release_context);
        self.expired_before = self
            .context
            .is_some_and(|context| Instant::now() >= context.cleanup_deadline);
        self.remember_context();
        self.phase = ExitPhase::WaitTerminal;
    }

    pub(super) fn wait_terminal(&mut self, interpreter: &mut Interpreter, env: &Env) {
        assert_eq!(self.phase, ExitPhase::WaitTerminal);
        let _guard = self.phase_guard();
        let settled = if self.tasks.handles().is_empty() {
            Ok(())
        } else {
            let context = self.context.expect("owned scope cleanup has a context");
            if interpreter.termination.is_some() {
                interpreter.cancel_visible_children(env, context, self.span);
            }
            match &interpreter.task_runtime {
                Some(runtime) => {
                    self.tasks.prepare(context);
                    self.tasks.wait(runtime)
                }
                None => {
                    // Preserve the old no-runtime fallback: raw requests use
                    // metadata cause, with no extra terminal waiting policy.
                    for handle in self.tasks.handles() {
                        handle.request_cancel_with(context);
                    }
                    Ok(())
                }
            }
        };
        self.settled = Some(settled);
        // Capture a nested drop's tightened TLS window before restoring guards.
        tighten_release(&mut self.release_context);
        self.phase = ExitPhase::ReleaseOwners;
    }

    pub(super) fn release_owners(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::ReleaseOwners);
        let _guard = self.phase_guard();
        if !self.tasks.handles().is_empty() {
            let context = interpreter
                .scope_cleanup_context(self.context)
                .expect("scope cleanup retains its context");
            let _release_guard = CleanupGuard::enter(context);
            for handle in self.tasks.handles() {
                handle.release_scope_owner(context);
            }
            tighten_release(&mut self.release_context);
        }
        tighten_release(&mut self.release_context);
        self.phase = ExitPhase::DropHandles;
    }

    pub(super) fn drop_handles(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::DropHandles);
        let _guard = self.phase_guard();
        let collected = self.collected.take().expect("collection result");
        let settled = self.settled.take().expect("settled result");
        if collected.is_err() && settled.is_err() {
            interpreter.record_suppressed_cleanup();
        }
        self.cleanup = Some(collected.and(settled));
        drop(std::mem::replace(&mut self.tasks, KnownTasks::Empty));
        tighten_release(&mut self.release_context);
        self.context = interpreter.scope_cleanup_context(self.context);
        self.phase = ExitPhase::Teardown;
    }

    /// The adapter has performed its actual teardown while phase_guard lives.
    /// This state never owns, pops, or substitutes an empty physical Env.
    pub(super) fn finish_teardown(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::Teardown);
        tighten_release(&mut self.release_context);
        self.context = interpreter.scope_cleanup_context(self.context);
        self.phase = ExitPhase::DeadlineCheck;
    }

    pub(super) fn deadline_check(&mut self, _interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::DeadlineCheck);
        let _guard = self.phase_guard();
        self.remember_context();
        if let Some(context) = self.context {
            let cleanup = self.cleanup.as_mut().expect("cleanup result");
            if cleanup.is_ok()
                && (self.expired_before || Instant::now() >= context.cleanup_deadline)
            {
                *cleanup = Err(KuError::termination(context, self.span));
            }
        }
        tighten_release(&mut self.release_context);
        self.phase = ExitPhase::Arbitrate;
    }

    pub(super) fn arbitrate(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::Arbitrate);
        let _guard = self.phase_guard();
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
                self.install_cleanup_error();
                self.phase = ExitPhase::Done;
            }
        } else {
            self.phase = ExitPhase::Done;
        }
        tighten_release(&mut self.release_context);
    }

    fn install_cleanup_error(&mut self) {
        let Err(mut error) = self.cleanup.take().expect("cleanup failure") else {
            unreachable!("cleanup error wins only on failure");
        };
        if let Some(context) = self.context {
            error.remember_scope_cleanup(context);
        }
        if error.runtime_termination().is_some() {
            let mut mapped = KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "task",
                "shutdown_timeout",
                "scope cleanup did not finish before the bounded cleanup deadline",
                self.span,
            );
            if let Some(context) = self.context {
                mapped.remember_scope_cleanup(context);
            }
            error = mapped;
        }
        self.outcome = Some(Err(error));
    }

    pub(super) fn discard(&mut self, interpreter: &mut Interpreter) {
        assert_eq!(self.phase, ExitPhase::Discard);
        let _guard = self.phase_guard();
        let context = self.context.expect("discard carries its cleanup context");
        interpreter.discard_flow(
            self.outcome.take().expect("outgoing primary"),
            context,
            self.span,
        );
        tighten_release(&mut self.release_context);
        self.context = interpreter.scope_cleanup_context(Some(context));
        self.install_cleanup_error();
        self.phase = ExitPhase::Done;
    }

    pub(super) fn into_result(mut self) -> KuResult<T> {
        assert_eq!(self.phase, ExitPhase::Done);
        self.outcome.take().expect("final exit outcome")
    }
}

impl<T: ScopeExit> Drop for ExitState<T> {
    fn drop(&mut self) {
        // Panic/abandonment of this header can only destroy what it owns. Actual
        // Env/lease/value ownership remains in the synchronous adapter. Keep
        // both known proxies and the sole primary inside the saved TLS window;
        // an expired deadline never skips necessary destruction.
        let _guard = self.phase_guard();
        // Take both before invoking either destructor. Tuple drop glue keeps
        // the remaining outcome inside this guard when a Task destructor
        // panics, preserving the original handles-before-outcome order.
        drop((
            std::mem::replace(&mut self.tasks, KnownTasks::Empty),
            self.outcome.take(),
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
    release_context: Option<RuntimeTermination>,
    span: Span,
}

impl<T: ScopeExit> DiscardState<T> {
    pub(super) fn new(result: KuResult<T>, context: RuntimeTermination, span: Span) -> Self {
        Self {
            phase: DiscardPhase::Collect,
            outcome: Some(result),
            tasks: KnownTasks::Collected(Vec::new()),
            context,
            release_context: effective_release_context(Some(context)),
            span,
        }
    }

    pub(super) fn run_sync(&mut self, interpreter: &Interpreter) {
        while self.phase != DiscardPhase::Done {
            let _guard = self.release_context.map(CleanupGuard::enter);
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
            tighten_release(&mut self.release_context);
        }
    }
}

impl<T: ScopeExit> Drop for DiscardState<T> {
    fn drop(&mut self) {
        let _guard = self.release_context.map(CleanupGuard::enter);
        drop((
            std::mem::replace(&mut self.tasks, KnownTasks::Empty),
            self.outcome.take(),
        ));
    }
}
