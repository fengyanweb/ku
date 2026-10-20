//! Shared cancellation broadcast and terminal-observation state.
//!
//! TerminalObserved preserves the existing synchronous wait predicate. It is
//! not acknowledgement that scope owners, discarded result payloads, or task
//! accounting have all been released. Those remain separate exit obligations.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancelBatchPoll {
    Pending {
        first_index: usize,
        unfinished: usize,
    },
    TerminalObserved,
    TimedOut(CancellationContext),
}

/// The synchronous caller borrows its existing handles. A future exit owner can
/// move an existing Vec into H without cloning user values or logical owners.
/// No runtime, lock, or thread-local guard survives between calls to poll.
pub(crate) struct TaskCancelBatch<H: AsRef<[TaskHandle]>> {
    handles: Option<H>,
    // Preserve the old wait snapshot independently of shorter deadlines seen
    // during resource destruction. Neither field creates a new time budget.
    context: CancellationContext,
    release_context: CancellationContext,
    completed: Option<CancelBatchPoll>,
}

impl<H: AsRef<[TaskHandle]>> TaskCancelBatch<H> {
    pub(crate) fn prepare(handles: H, context: CancellationContext) -> Self {
        let _cleanup = CleanupGuard::enter(context);
        // Match the original wait: select one effective absolute deadline and
        // first cause before broadcasting. A nested synchronous owner drop may
        // tighten its caller's TLS window, but never starts a new wait budget.
        let context = current_cleanup_context().expect("cleanup guard installs its context");
        let mut batch = Self {
            handles: Some(handles),
            context,
            release_context: context,
            completed: None,
        };
        // Broadcast even when the deadline has expired, before observing any
        // child. Queued-frame capture destruction retains its existing path.
        // Install the context owner first: a panic during broadcasting still
        // drops an owned H under this same window, including unsent siblings.
        for handle in batch.handles() {
            handle.request_cancel_with(context);
        }
        if let Some(release) = current_cleanup_context() {
            batch.release_context.cleanup_deadline = batch
                .release_context
                .cleanup_deadline
                .min(release.cleanup_deadline);
        }
        batch
    }

    pub(crate) fn context(&self) -> CancellationContext {
        self.context
    }

    pub(crate) fn handles(&self) -> &[TaskHandle] {
        self.handles
            .as_ref()
            .expect("live cancellation batch")
            .as_ref()
    }

    /// `now` is a lower bound, useful for an explicit deadline observation.
    /// Real time is sampled after the terminal scan so a large batch cannot
    /// carry a stale pre-scan clock past the original absolute deadline.
    pub(crate) fn poll(&mut self, runtime: &TaskRuntime, now: Instant) -> CancelBatchPoll {
        if let Some(completed @ CancelBatchPoll::TimedOut(_)) = self.completed {
            return completed;
        }
        // Give even an already-terminal batch one finite queue-retirement
        // opportunity. Its success predicate remains terminal observation;
        // no unrelated body runs and an expired window takes no new node.
        if now.max(Instant::now()) < self.release_context.cleanup_deadline {
            runtime.retire_cancelled_queue_prefix(self.release_context);
        }
        if let Some(completed) = self.completed {
            return completed;
        }
        let mut first_index = None;
        let mut unfinished = 0;
        for (index, handle) in self.handles().iter().enumerate() {
            if !handle.state.is_terminal() {
                unfinished += 1;
                first_index.get_or_insert(index);
            }
        }
        // Keep the original ordering: a fully terminal set succeeds even when
        // its absolute deadline has passed. Enclosing teardown checks expiry.
        let Some(first_index) = first_index else {
            self.completed = Some(CancelBatchPoll::TerminalObserved);
            return CancelBatchPoll::TerminalObserved;
        };
        let now = now.max(Instant::now());
        if now >= self.context.cleanup_deadline {
            // Count the first timeout observation for this batch, rather than
            // a fresh timeout/unfinished sample on every future poll or child.
            runtime.record_cleanup_timeout();
            runtime
                .inner
                .cleanup_unfinished_tasks
                .fetch_add(unfinished, Ordering::Relaxed);
            let completed = CancelBatchPoll::TimedOut(self.context);
            self.completed = Some(completed);
            return completed;
        }
        CancelBatchPoll::Pending {
            first_index,
            unfinished,
        }
    }
}

impl<H: AsRef<[TaskHandle]>> Drop for TaskCancelBatch<H> {
    fn drop(&mut self) {
        // An owning H can release its final Task leases outside prepare/poll.
        // Keep their actual destruction inside the stored absolute window,
        // including expiry and panic; never mint a fresh one-second budget.
        // Borrowed H performs no payload destruction. No TLS guard is retained
        // as part of the persistent batch itself.
        let _cleanup = CleanupGuard::enter(self.release_context);
        drop(self.handles.take());
    }
}
