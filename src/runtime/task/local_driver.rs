//! Flat claimant-local Runner help. Legacy callbacks retain synchronous calls.
//!
//! Frame Vec backing and typed runner headers have separate 2 MiB budgets.
//! These are backing/header bounds, not a process or runner-payload RSS cap.

use super::*;
use std::{mem::size_of, num::NonZeroUsize, ops::ControlFlow};

const MAX_LOCAL_FRAMES: usize = MAX_AWAIT_DEPTH + 1;
const FRAME_BACKING_BUDGET: usize = 2 * 1024 * 1024;
const RUNNER_HEADER_BUDGET: usize = 2 * 1024 * 1024;
pub(super) const MAX_TYPED_RUNNER_HEADER: usize = RUNNER_HEADER_BUDGET / MAX_LOCAL_FRAMES;

thread_local! {
    // Include legacy callback boundaries that recursively start another local
    // driver. There is one physical-thread budget, not one allowance per call.
    static FRAME_BACKING_BYTES: Cell<usize> = const { Cell::new(0) };
    static RUNNER_HEADER_BYTES: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn storage_error() -> KuError {
    KuError::runtime(
        "internal execution storage rejected: local task frame/header budget",
        Span::default(),
    )
}

#[derive(Clone, Copy)]
enum Stage {
    Body,
    Complete,
    Retire,
}

enum RuntimeRef<'a> {
    Root(&'a TaskRuntimeInner),
    Helper(Arc<TaskRuntimeInner>),
}

impl std::ops::Deref for RuntimeRef<'_> {
    type Target = TaskRuntimeInner;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Root(inner) => inner,
            Self::Helper(inner) => inner,
        }
    }
}

struct LocalJobFrame<'a> {
    inner: RuntimeRef<'a>,
    job: TaskJob,
    start: Option<TaskStart>,
    runner: Option<Box<dyn LocalTaskRunner>>,
    result: Option<KuResult<Value>>,
    panicked: bool,
    escaping: Option<CleanupPanic>,
    stage: Stage,
    caller_id: i64,
    caller_state: Option<Weak<TaskState>>,
    help_depth: usize,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
    inherited_cleanup: bool,
    header_bytes: usize,
}

fn tighten(saved: &mut Option<CancellationContext>, observed: Option<CancellationContext>) {
    if let Some(observed) = observed {
        match saved {
            Some(saved) => {
                saved.cleanup_deadline = saved.cleanup_deadline.min(observed.cleanup_deadline);
            }
            None => *saved = Some(observed),
        }
    }
}

// No guard lives in LocalJobFrame. Each operation restores its physical caller
// before a child is pushed, keeping unrelated helper termination out of parents.
struct Window {
    execution: ExecutionTerminationGuard,
    cleanup: Option<CleanupGuard>,
    id: i64,
    state: Option<Weak<TaskState>>,
    depth: usize,
}

impl Window {
    fn enter(frame: &LocalJobFrame, body_identity: bool) -> Self {
        let execution = ExecutionTerminationGuard::enter();
        if let Some(context) = frame.execution {
            set_execution_termination(context);
        }
        let cleanup = frame.cleanup.map(CleanupGuard::enter);
        let id = CURRENT_TASK_ID.with(|slot| {
            slot.replace(if body_identity {
                frame.job.id
            } else {
                frame.caller_id
            })
        });
        let state = CURRENT_TASK_STATE.with(|slot| {
            slot.replace(if body_identity {
                Some(Arc::downgrade(&frame.job.state))
            } else {
                frame.caller_state.clone()
            })
        });
        let depth = AWAIT_HELP_DEPTH.with(|slot| slot.replace(frame.help_depth));
        Self {
            execution,
            cleanup,
            id,
            state,
            depth,
        }
    }

    fn remember(&self, frame: &mut LocalJobFrame) {
        tighten(&mut frame.cleanup, current_cleanup_context());
        tighten(&mut frame.execution, current_execution_termination());
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        CURRENT_TASK_STATE.with(|slot| {
            slot.replace(self.state.take());
        });
        CURRENT_TASK_ID.with(|slot| slot.set(self.id));
        AWAIT_HELP_DEPTH.with(|slot| slot.set(self.depth));
        // Named fields restore cleanup, then execution after this method.
        // Explicit takes make that order independent of field declaration.
        drop(self.cleanup.take());
        let _ = &self.execution;
    }
}

impl<'a> LocalJobFrame<'a> {
    fn claim(
        inner: RuntimeRef<'a>,
        job: TaskJob,
        caller_id: i64,
        caller_state: Option<Weak<TaskState>>,
        help_depth: usize,
        cleanup: Option<CancellationContext>,
    ) -> Self {
        let start = job.state.claim_run();
        let stage = if start.is_some() {
            Stage::Body
        } else {
            Stage::Retire
        };
        let header_bytes = start.as_ref().map_or(0, TaskStart::runner_header_bytes);
        Self {
            inner,
            job,
            start,
            runner: None,
            result: None,
            panicked: false,
            escaping: None,
            stage,
            caller_id,
            caller_state,
            help_depth,
            cleanup,
            execution: None,
            header_bytes,
            inherited_cleanup: cleanup.is_some(),
        }
    }

    fn body(&mut self) -> bool {
        let mut local = self.runner.take();
        if let Some(start) = self.start.take() {
            match start {
                TaskStart::Callback(run) => {
                    self.result = Some(run());
                    self.stage = Stage::Complete;
                    return false;
                }
                TaskStart::Runner { factory, .. } => local = Some(factory()),
            }
        }
        let mut runner = local.expect("live local runner body");
        match runner.dispatch(NonZeroUsize::new(256).expect("positive dispatch chunk")) {
            ControlFlow::Break(result) => {
                // Drop before switching to caller identity for completion. An
                // unwind here is classified by the same body panic catch.
                drop(runner);
                self.result = Some(result);
                self.stage = Stage::Complete;
                false
            }
            ControlFlow::Continue(()) => {
                let waiting = runner.waiting().is_some();
                self.runner = Some(runner);
                waiting
            }
        }
    }

    fn body_panic(&mut self, panic: CleanupPanic) {
        // Persistent runner ownership must end inside the original body catch.
        // Its local ownership was taken before dispatch, so unwind already ran
        // real runner Drop. Standard Rust double-panic abort stays unchanged.
        drop(panic);
        self.panicked = true;
        self.result = Some(Ok(task_error("panic", "async task panicked")));
        self.stage = Stage::Complete;
    }
}

struct Frames<'a> {
    frames: Vec<LocalJobFrame<'a>>,
    max_frames: usize,
    header_bytes: usize,
    backing_bytes: usize,
}

impl<'a> Frames<'a> {
    fn new(depth: usize) -> Self {
        Self {
            frames: Vec::new(),
            max_frames: MAX_LOCAL_FRAMES.saturating_sub(depth).max(1),
            header_bytes: 0,
            backing_bytes: 0,
        }
    }

    fn reserve_next(&mut self) -> KuResult<()> {
        let required = self.frames.len().checked_add(1).ok_or_else(storage_error)?;
        // Reserve for the largest admitted typed header before taking any next
        // queue node. After claim only actual size_of<R> is charged.
        if required > self.max_frames
            || RUNNER_HEADER_BYTES
                .with(Cell::get)
                .checked_add(MAX_TYPED_RUNNER_HEADER)
                .is_none_or(|bytes| bytes > RUNNER_HEADER_BUDGET)
        {
            return Err(storage_error());
        }
        if required <= self.frames.capacity() {
            return Ok(());
        }
        let old = self
            .frames
            .capacity()
            .checked_mul(size_of::<LocalJobFrame>())
            .ok_or_else(storage_error)?;
        let target = self
            .frames
            .capacity()
            .saturating_mul(2)
            .max(required)
            .min(self.max_frames);
        let mut replacement = Vec::new();
        // Charge old + candidate before and after its actual capacity is known.
        if FRAME_BACKING_BYTES
            .with(Cell::get)
            .checked_add(
                target
                    .checked_mul(size_of::<LocalJobFrame>())
                    .ok_or_else(storage_error)?,
            )
            .is_none_or(|bytes| bytes > FRAME_BACKING_BUDGET)
        {
            return Err(storage_error());
        }
        replacement
            .try_reserve_exact(target)
            .map_err(|_| storage_error())?;
        let new_bytes = replacement
            .capacity()
            .checked_mul(size_of::<LocalJobFrame>())
            .ok_or_else(storage_error)?;
        let peak = new_bytes
            .checked_add(FRAME_BACKING_BYTES.with(Cell::get))
            .ok_or_else(storage_error)?;
        if replacement.capacity() > self.max_frames || peak > FRAME_BACKING_BUDGET {
            return Err(storage_error());
        }
        replacement.append(&mut self.frames);
        self.frames = replacement;
        FRAME_BACKING_BYTES.with(|bytes| bytes.set(bytes.get() - old + new_bytes));
        self.backing_bytes = new_bytes;
        Ok(())
    }

    fn push(&mut self, frame: LocalJobFrame<'a>) {
        self.header_bytes += frame.header_bytes;
        RUNNER_HEADER_BYTES.with(|bytes| bytes.set(bytes.get() + frame.header_bytes));
        assert!(self.frames.len() < self.frames.capacity());
        assert!(self.header_bytes <= RUNNER_HEADER_BUDGET);
        self.frames.push(frame);
    }
}

struct BudgetRelease {
    backing: usize,
    headers: usize,
}

impl Drop for BudgetRelease {
    fn drop(&mut self) {
        FRAME_BACKING_BYTES.with(|bytes| bytes.set(bytes.get() - self.backing));
        RUNNER_HEADER_BYTES.with(|bytes| bytes.set(bytes.get() - self.headers));
    }
}

impl Drop for Frames<'_> {
    fn drop(&mut self) {
        // Release primitive charges only after the actual Vec/resource drop,
        // including an unwind. Normal retirement has already drained frames.
        let _release = BudgetRelease {
            backing: self.backing_bytes,
            headers: self.header_bytes,
        };
        drop(std::mem::take(&mut self.frames));
    }
}

fn take_help(runtime: &TaskRuntime) -> KuResult<Option<TaskJob>> {
    let receiver = match runtime.inner.task_rx.try_lock() {
        Ok(receiver) => receiver,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Err(KuError::runtime(
                "async task queue is poisoned",
                Span::default(),
            ));
        }
    };
    match receiver.try_recv() {
        Ok(job) => {
            runtime.inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
            Ok(Some(job))
        }
        Err(TryRecvError::Empty) => Ok(None),
        Err(TryRecvError::Disconnected) => Err(KuError::runtime(
            "async task queue is disconnected",
            Span::default(),
        )),
    }
}

pub(super) fn execute(inner: &TaskRuntimeInner, job: TaskJob) {
    let base_depth = await_help_depth();
    let mut stack = Frames::new(base_depth);
    if let Err(error) = stack.reserve_next() {
        let _execution = ExecutionTerminationGuard::enter();
        let _retirement = QueueRetirement {
            inner,
            state: &job.state,
        };
        job.state.reject_queued(job.id, Err(error));
        return;
    }
    let caller = current_task_id();
    let caller_state = CURRENT_TASK_STATE.with(|slot| slot.borrow().clone());
    stack.push(LocalJobFrame::claim(
        RuntimeRef::Root(inner),
        job,
        caller,
        caller_state,
        base_depth,
        current_cleanup_context(),
    ));
    while !stack.frames.is_empty() {
        let top = stack.frames.len() - 1;
        let stage = stack.frames[top].stage;
        match stage {
            Stage::Body => {
                let window = Window::enter(&stack.frames[top], true);
                let body = catch_unwind(AssertUnwindSafe(|| stack.frames[top].body()));
                let waiting = match body {
                    Ok(waiting) => waiting,
                    Err(panic) => {
                        stack.frames[top].body_panic(panic);
                        false
                    }
                };
                let mut helper = None;
                if waiting {
                    let observed = catch_unwind(AssertUnwindSafe(|| {
                        let mut runner = stack.frames[top].runner.take().expect("waiting runner");
                        let ticket = runner.waiting().expect("waiting ticket");
                        if ticket.poll() == AwaitPoll::Ready {
                            stack.frames[top].runner = Some(runner);
                            return;
                        }
                        if let Err(error) = stack.reserve_next() {
                            ticket.helper_error(error);
                            stack.frames[top].runner = Some(runner);
                            return;
                        }
                        match take_help(ticket.runtime()) {
                            Ok(Some(job)) => {
                                let runtime = ticket.runtime().inner.clone();
                                helper = Some((runtime, job, current_cleanup_context()));
                            }
                            Ok(None) => ticket.wait_once(),
                            Err(error) => ticket.helper_error(error),
                        }
                        stack.frames[top].runner = Some(runner);
                    }));
                    if let Err(panic) = observed {
                        stack.frames[top].body_panic(panic);
                    }
                }
                window.remember(&mut stack.frames[top]);
                drop(window);
                if let Some((runtime, job, cleanup)) = helper {
                    let parent = &stack.frames[top];
                    let caller_id = parent.job.id;
                    let caller_state = Some(Arc::downgrade(&parent.job.state));
                    let help_depth = parent.help_depth + 1;
                    stack.push(LocalJobFrame::claim(
                        RuntimeRef::Helper(runtime),
                        job,
                        caller_id,
                        caller_state,
                        help_depth,
                        cleanup,
                    ));
                }
            }
            Stage::Complete => {
                let window = Window::enter(&stack.frames[top], false);
                let frame = &mut stack.frames[top];
                let result = frame.result.take().expect("one task body outcome");
                let completed = catch_unwind(AssertUnwindSafe(|| {
                    frame.job.state.complete_with_status(
                        frame.job.id,
                        result,
                        frame.panicked.then_some(TASK_PANICKED),
                    );
                }));
                window.remember(frame);
                if let Err(panic) = completed {
                    frame.escaping = Some(panic);
                }
                frame.stage = Stage::Retire;
                drop(window);
            }
            Stage::Retire => {
                let window = Window::enter(&stack.frames[top], false);
                finish_task_job(&stack.frames[top].inner, &stack.frames[top].job.state);
                drop(window);
                let mut frame = stack.frames.pop().expect("retired frame");
                stack.header_bytes -= frame.header_bytes;
                RUNNER_HEADER_BYTES.with(|bytes| bytes.set(bytes.get() - frame.header_bytes));
                if frame.inherited_cleanup {
                    if let Some(parent) = stack.frames.last_mut() {
                        // Only an already-shared cleanup window receives LIFO
                        // deadline feedback. Child execution termination never
                        // creates or replaces a parent's context.
                        if parent.cleanup.is_some() {
                            tighten(&mut parent.cleanup, frame.cleanup);
                        }
                    }
                }
                if let Some(panic) = frame.escaping.take() {
                    drop(frame);
                    let Some(parent) = stack.frames.last_mut() else {
                        std::panic::resume_unwind(panic);
                    };
                    let window = Window::enter(parent, true);
                    // Move the runner into this catch so its real Drop follows
                    // abort_native's unwind before classifying the parent body.
                    let mut runner = parent.runner.take().expect("waiting parent runner");
                    let aborted = catch_unwind(AssertUnwindSafe(move || {
                        runner
                            .waiting()
                            .expect("waiting parent ticket")
                            .abort_native(panic);
                    }));
                    if let Err(panic) = aborted {
                        parent.body_panic(panic);
                    }
                    window.remember(parent);
                    drop(window);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "local_driver_tests.rs"]
mod tests;
