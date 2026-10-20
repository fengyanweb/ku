//! State-only evidence after the real scope owners and payloads are released.
//! This does not broadcast cancellation or retain a logical Task owner.

use super::*;

pub(crate) struct TaskRetirementWitness {
    state: Arc<TaskState>,
    // A foreign runtime must remain available for finite queue progress. This
    // is its existing internal Arc, not another TaskOwnerLease or queue node.
    runtime: TaskRuntime,
}

impl TaskRetirementWitness {
    pub(super) fn new(state: Arc<TaskState>, runtime: TaskRuntime) -> Self {
        Self { state, runtime }
    }

    fn observe(&self) -> (bool, Option<CancellationContext>) {
        let submission = self.state.submission.load(Ordering::Acquire);
        let owner = self.state.owner_cleanup_done.load(Ordering::Acquire);
        let completion = self.state.completion_cleanup_done.load(Ordering::Acquire);
        let execution = self.state.execution_queue_retired.load(Ordering::Acquire);
        let accounting = self.state.accounting_done.load(Ordering::Acquire);
        // The last BlockingJob guard publishes after its real captures and
        // response have dropped, including feedback of a shorter cleanup D.
        let blocking = self.state.pending_blocking.load(Ordering::Acquire);
        let cancellation = self.state.cancellation();
        let retired = match submission {
            SUBMISSION_ACCEPTED => owner && completion && execution && accounting,
            SUBMISSION_PRODUCER_RETIRED => owner,
            _ => false,
        } && blocking == 0;
        (retired, cancellation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetirementBatchPoll {
    Pending {
        first_index: usize,
        unfinished: usize,
    },
    Retired,
    TimedOut(CancellationContext),
}

/// The caller reserves and charges the actual Vec backing before collecting
/// proxies or moving its primary. Construction performs no allocation or Drop.
pub(crate) struct TaskRetirementBatch {
    witnesses: Vec<TaskRetirementWitness>,
    context: CancellationContext,
    prefix_cursor: usize,
    timed_out: bool,
}

impl TaskRetirementBatch {
    pub(crate) fn from_reserved(
        witnesses: Vec<TaskRetirementWitness>,
        context: CancellationContext,
    ) -> Self {
        Self {
            witnesses,
            context,
            prefix_cursor: 0,
            timed_out: false,
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.witnesses.capacity()
    }

    pub(crate) fn context(&self) -> CancellationContext {
        self.context
    }

    fn scan(&mut self) -> (Option<usize>, usize) {
        let mut first_index = None;
        let mut unfinished = 0;
        for (index, witness) in self.witnesses.iter().enumerate() {
            let (retired, cancellation) = witness.observe();
            if let Some(context) = cancellation {
                self.context.cleanup_deadline =
                    self.context.cleanup_deadline.min(context.cleanup_deadline);
            }
            if !retired {
                unfinished += 1;
                first_index.get_or_insert(index);
            }
        }
        if let Some(context) = current_cleanup_context() {
            self.context.cleanup_deadline =
                self.context.cleanup_deadline.min(context.cleanup_deadline);
        }
        (first_index, unfinished)
    }

    fn observe_progress(
        &mut self,
        first_index: Option<usize>,
        unfinished: usize,
        now: Instant,
    ) -> RetirementBatchPoll {
        if self.timed_out {
            return RetirementBatchPoll::TimedOut(self.context);
        }
        let Some(first_index) = first_index else {
            // Retirement is a fact, not proof that the enclosing exit met D.
            return RetirementBatchPoll::Retired;
        };
        if now.max(Instant::now()) >= self.context.cleanup_deadline {
            let runtime = &self.witnesses[first_index].runtime;
            runtime.record_cleanup_timeout();
            runtime
                .inner
                .cleanup_unfinished_tasks
                .fetch_add(unfinished, Ordering::Relaxed);
            self.timed_out = true;
            return RetirementBatchPoll::TimedOut(self.context);
        }
        RetirementBatchPoll::Pending {
            first_index,
            unfinished,
        }
    }

    pub(crate) fn poll(&mut self, now: Instant) -> RetirementBatchPoll {
        // Finish the whole scan before touching any queue. Independent state
        // locks provide an observed minimum, not a global D linearization.
        let (first_index, unfinished) = self.scan();
        let progress = self.observe_progress(first_index, unfinished, now);
        if !matches!(progress, RetirementBatchPoll::Pending { .. }) {
            return progress;
        }
        let index = self.prefix_cursor;
        self.prefix_cursor = (index + 1) % self.witnesses.len();
        {
            let _cleanup = CleanupGuard::enter(self.context);
            // One finite original G2 prefix step; no help or unrelated body.
            self.witnesses[index]
                .runtime
                .retire_cancelled_queue_prefix(self.context);
            if let Some(context) = current_cleanup_context() {
                self.context.cleanup_deadline =
                    self.context.cleanup_deadline.min(context.cleanup_deadline);
            }
        }
        let (first_index, unfinished) = self.scan();
        self.observe_progress(first_index, unfinished, now)
    }

    pub(crate) fn wait_once(&mut self, _first_index: usize, span: Span) -> KuResult<()> {
        // Notifications are finite progress hints; the caller polls the real
        // obligations again. No await claim or persistent TLS guard is kept.
        let (remaining, _) = self.scan();
        let Some(first_index) = remaining else {
            return Ok(());
        };
        if self.timed_out || Instant::now() >= self.context.cleanup_deadline {
            return Ok(());
        }
        let result = self.witnesses[first_index].state.wait(
            self.context
                .cleanup_deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(2)),
        );
        self.scan();
        result.map_err(|mut error| {
            error.span = span;
            error
        })
    }
}

impl Drop for TaskRetirementBatch {
    fn drop(&mut self) {
        // The final internal runtime Arc can own actual queue storage. Its
        // destruction must finish before this saved window is restored, also
        // when the outer exit is abandoned by a native panic. No waiting here.
        let _cleanup = CleanupGuard::enter(self.context);
        drop(std::mem::take(&mut self.witnesses));
    }
}
