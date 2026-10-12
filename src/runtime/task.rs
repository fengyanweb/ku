use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        Arc, Condvar, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    error::{KuError, KuResult, TerminationReason},
    span::Span,
    stdlib::errors,
    value::Value,
};

mod await_ticket;
pub(crate) use await_ticket::{AwaitPoll, AwaitTicket};
#[cfg(test)]
mod await_ticket_tests;
mod cancel_batch;
mod cancellation;
pub(crate) use cancel_batch::{CancelBatchPoll, TaskCancelBatch};
#[cfg(test)]
mod cancel_batch_tests;
#[cfg(test)]
mod cleanup_panic_tests;
#[cfg(test)]
mod help_panic_tests;
mod local_driver;
#[cfg(test)]
mod owner_cleanup_ack_tests;
#[cfg(test)]
mod queue_retirement_tests;
mod runner;
pub(crate) use runner::{LocalTaskRunner, OnceTaskRunner};
#[cfg(test)]
mod runner_tests;
#[cfg(test)]
mod submission_panic_tests;
pub(crate) use cancellation::{
    current_cleanup_context, current_execution_termination, ensure_task_operations_allowed,
    set_execution_termination, CancellationContext, CleanupGuard, ExecutionTerminationGuard,
};

pub const MAX_TASKS: usize = 1024;
pub const MAX_TASK_QUEUE: usize = 1024;
pub const MAX_BLOCKING_QUEUE: usize = 1024;
pub const MAX_AWAIT_DEPTH: usize = 64;

const TASK_PENDING: u8 = 0;
const TASK_RUNNING: u8 = 1;
const TASK_WAITING: u8 = 2;
const TASK_COMPLETED: u8 = 3;
const TASK_FAILED: u8 = 4;
const TASK_CANCELLED: u8 = 5;
const TASK_CANCELLING: u8 = 6;
const TASK_PANICKED: u8 = 7;
const TASK_TIMED_OUT: u8 = 8;

thread_local! {
    static CURRENT_TASK_ID: Cell<i64> = const { Cell::new(0) };
    static CURRENT_TASK_STATE: RefCell<Option<Weak<TaskState>>> = const { RefCell::new(None) };
    static AWAIT_HELP_DEPTH: Cell<usize> = const { Cell::new(0) };
    // Released results can own other completed Tasks. Their final lease drops
    // enqueue an existing state, instead of recursively destroying its result.
    static OWNER_RESULT_RELEASE_HEAD: RefCell<Option<Arc<TaskState>>> = const { RefCell::new(None) };
    // Completed nodes reuse their original inline slot until the outermost
    // drain has really destroyed every detached descendant. No new node or
    // backing allocation is needed to retain these acknowledgement owners.
    static OWNER_RESULT_FINISHED_HEAD: RefCell<Option<Arc<TaskState>>> = const { RefCell::new(None) };
    static OWNER_RESULT_FLUSH_DEPTH: Cell<usize> = const { Cell::new(0) };
    static OWNER_RESULT_RELEASE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

type TaskFn = Box<dyn FnOnce() -> KuResult<Value> + Send + 'static>;
type RunnerFactory = Box<dyn FnOnce() -> Box<dyn LocalTaskRunner> + Send + 'static>;

enum TaskStart {
    Callback(TaskFn),
    Runner {
        factory: RunnerFactory,
        header_bytes: usize,
    },
}

impl<F: FnOnce() -> KuResult<Value> + Send + 'static> From<Box<F>> for TaskStart {
    fn from(run: Box<F>) -> Self {
        Self::Callback(run)
    }
}

impl TaskStart {
    fn runner_header_bytes(&self) -> usize {
        match self {
            Self::Callback(_) => 0,
            Self::Runner { header_bytes, .. } => *header_bytes,
        }
    }
}
type BlockingFn = Box<dyn FnOnce() -> KuResult<Value> + Send + 'static>;
type CleanupPanic = Box<dyn std::any::Any + Send + 'static>;

struct AwaitHelpDepthGuard {
    previous: usize,
    _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for AwaitHelpDepthGuard {
    fn drop(&mut self) {
        AWAIT_HELP_DEPTH.with(|depth| depth.set(self.previous));
    }
}

struct TaskWaitGuard {
    runtime: TaskRuntime,
    current: i64,
    state: Option<Arc<TaskState>>,
}

impl Drop for TaskWaitGuard {
    fn drop(&mut self) {
        if self.current != 0 {
            self.runtime.clear_wait(self.current);
        }
        if let Some(state) = &self.state {
            if !state.is_cancelled() {
                state.set_status(TASK_RUNNING);
            }
        }
    }
}

struct TaskJob {
    id: i64,
    state: Arc<TaskState>,
}

// Inspecting a cancellation prefix can encounter an unrelated live job. Keep
// that one node in place, ahead of the channel, rather than executing or
// re-enqueuing it. Its original queue credit remains reserved while cached.
struct TaskQueueReceiver {
    // Keep the retained first node ahead of channel nodes on final Drop too.
    head: Option<TaskJob>,
    receiver: Receiver<TaskJob>,
}

impl TaskQueueReceiver {
    fn recv(&mut self) -> Result<TaskJob, mpsc::RecvError> {
        match self.head.take() {
            Some(job) => Ok(job),
            None => self.receiver.recv(),
        }
    }

    fn try_recv(&mut self) -> Result<TaskJob, TryRecvError> {
        match self.head.take() {
            Some(job) => Ok(job),
            None => self.receiver.try_recv(),
        }
    }
}

struct BlockingJob {
    run: BlockingFn,
    response: SyncSender<KuResult<Value>>,
    cancelled: Option<Arc<TaskState>>,
}

struct RunningBlockingJob<'a> {
    inner: &'a TaskRuntimeInner,
}

impl<'a> RunningBlockingJob<'a> {
    fn claim(inner: &'a TaskRuntimeInner) -> Self {
        // Publish running before removing queued. A shutdown that observes the
        // queue decrement with Acquire must also observe this earlier increment.
        inner.running_blocking_jobs.fetch_add(1, Ordering::AcqRel);
        Self { inner }
    }
}

impl Drop for RunningBlockingJob<'_> {
    fn drop(&mut self) {
        self.inner
            .running_blocking_jobs
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct TaskRuntime {
    inner: Arc<TaskRuntimeInner>,
}

struct TaskRuntimeInner {
    task_tx: SyncSender<TaskJob>,
    task_rx: Arc<Mutex<TaskQueueReceiver>>,
    blocking_tx: SyncSender<BlockingJob>,
    states: Mutex<HashMap<i64, Weak<TaskState>>>,
    wait_edges: Mutex<HashMap<i64, i64>>,
    active_tasks: AtomicUsize,
    queued_tasks: AtomicUsize,
    queued_blocking_jobs: AtomicUsize,
    running_blocking_jobs: AtomicUsize,
    total_submissions: AtomicUsize,
    accepted_submissions: AtomicUsize,
    rejected_task_limit: AtomicUsize,
    rejected_task_queue: AtomicUsize,
    rejected_task_internal: AtomicUsize,
    finished_tasks: AtomicUsize,
    suppressed_cleanup_outcomes: AtomicUsize,
    cleanup_timeouts: AtomicUsize,
    cleanup_unfinished_tasks: AtomicUsize,
    next_task_id: AtomicI64,
    shutdown: AtomicBool,
    closing: AtomicBool,
    max_tasks: usize,
    task_queue_limit: usize,
    blocking_queue_limit: usize,
    task_workers: AtomicUsize,
    blocking_workers: AtomicUsize,
}

enum TaskRegistration {
    Ready,
    Closing,
    Poisoned,
}

// Producer accounting before a queue node is accepted. No user payload or
// logical owner is retained, and rollback never fabricates worker completion.
struct TaskSubmissionReservation<'a> {
    runtime: &'a TaskRuntime,
    id: i64,
    active_owned: bool,
    queued_reserved: bool,
}

impl<'a> TaskSubmissionReservation<'a> {
    fn new(runtime: &'a TaskRuntime, id: i64) -> Self {
        Self {
            runtime,
            id,
            active_owned: true,
            queued_reserved: false,
        }
    }

    fn reserve_queue(&mut self) -> bool {
        // A cached live head has vacated a physical channel slot without
        // relinquishing its logical capacity. Count reservations and that
        // retained head together so inspection cannot grow the bounded queue.
        if self.runtime.inner.task_queue_limit != 0
            && self
                .runtime
                .inner
                .queued_tasks
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    (current < self.runtime.inner.task_queue_limit).then_some(current + 1)
                })
                .is_err()
        {
            return false;
        }
        self.queued_reserved = true;
        if self.runtime.inner.task_queue_limit == 0 {
            // Keep sync_channel(0)'s original rendezvous admission. A zero
            // credit limit would reject even a waiting receiver's handoff.
            self.runtime
                .inner
                .queued_tasks
                .fetch_add(1, Ordering::AcqRel);
        }
        true
    }

    fn commit_to_worker(&mut self) {
        self.active_owned = false;
        self.queued_reserved = false;
    }

    fn rollback(&mut self) {
        // Relinquish each responsibility first; explicit rollback and Drop
        // cannot decrement it twice even when a later stage unwinds.
        if std::mem::take(&mut self.queued_reserved) {
            self.runtime
                .inner
                .queued_tasks
                .fetch_sub(1, Ordering::AcqRel);
        }
        if std::mem::take(&mut self.active_owned) {
            self.runtime
                .inner
                .active_tasks
                .fetch_sub(1, Ordering::AcqRel);
            self.runtime.remove_state(self.id);
        }
    }
}

impl Drop for TaskSubmissionReservation<'_> {
    fn drop(&mut self) {
        self.rollback();
    }
}

pub struct TaskHandle {
    // Rust drops fields in declaration order. The final owner lease must detach
    // its payload while this handle still keeps TaskState alive; otherwise the
    // state's Arc drop recursively destroys nested results before the lease can
    // enter the iterative owner drain.
    owner: Arc<TaskOwnerLease>,
    id: i64,
    state: Arc<TaskState>,
    runtime: TaskRuntime,
}

struct TaskOwnerLease {
    state: Weak<TaskState>,
}

enum OwnerResultRelease {
    Pending {
        result: KuResult<Value>,
        context: CancellationContext,
        next: Option<Arc<TaskState>>,
    },
    Finished {
        next: Option<Arc<TaskState>>,
    },
}

struct OwnerResultFlushGuard {
    previous: usize,
}

impl OwnerResultFlushGuard {
    fn enter() -> Self {
        Self {
            previous: OWNER_RESULT_FLUSH_DEPTH.with(|depth| {
                let previous = depth.get();
                depth.set(previous + 1);
                previous
            }),
        }
    }

    fn is_outermost(&self) -> bool {
        self.previous == 0
    }
}

impl Drop for OwnerResultFlushGuard {
    fn drop(&mut self) {
        OWNER_RESULT_FLUSH_DEPTH.with(|depth| depth.set(self.previous));
    }
}

struct SynchronousOwnerDropGuard {
    previous: bool,
}

impl SynchronousOwnerDropGuard {
    fn enter() -> Self {
        Self {
            previous: OWNER_RESULT_RELEASE_ACTIVE.with(|active| active.replace(false)),
        }
    }
}

impl Drop for SynchronousOwnerDropGuard {
    fn drop(&mut self) {
        OWNER_RESULT_RELEASE_ACTIVE.with(|active| active.set(self.previous));
    }
}

fn drop_owner_synchronously<T>(value: T) {
    // A container/closure can own ordered siblings and non-Task resources. Each
    // child must finish its ordinary depth-first drop before the next sibling;
    // a deferred LIFO/FIFO batch would change that observable order. Restore the
    // outer drain mode on both success and destructor panic. This deliberately
    // retains host recursion for unclassified Value trees and queued captures.
    let _synchronous = SynchronousOwnerDropGuard::enter();
    drop(value);
}

fn has_single_task_leaf(mut value: &Value) -> bool {
    loop {
        value = match value {
            Value::Task(_) => return true,
            Value::Result { value, .. } => value,
            Value::Array(values) | Value::Enum { fields: values, .. } if values.len() == 1 => {
                &values[0]
            }
            Value::Object(fields) | Value::Struct { fields, .. } if fields.len() == 1 => {
                fields.values().next().expect("one field has one value")
            }
            // Do not inspect closure environments, weak borrowed roots, or a
            // wider container even if it happens to contain only one Task.
            _ => return false,
        };
    }
}

fn take_single_owner_field(fields: HashMap<String, Value>) -> Value {
    let mut fields = fields.into_iter();
    let (key, value) = fields
        .next()
        .expect("classified owner wrapper has one field");
    drop(key);
    drop(fields);
    value
}

// Wrapper storage is consumed without a cleanup Vec or owner clones. Backing
// allocations can be freed before the Task; exact allocator free order is not
// preserved. Accepted metadata is only bool/String, never a sibling resource
// lease or closure capture.
fn drop_single_task_leaf(mut value: Value) {
    loop {
        value = match value {
            Value::Task(task) => {
                drop(task);
                return;
            }
            Value::Result { value, .. } => *value,
            Value::Array(mut values) if values.len() == 1 => {
                let child = values
                    .pop()
                    .expect("classified owner wrapper has one child");
                drop(values);
                child
            }
            Value::Object(fields) if fields.len() == 1 => take_single_owner_field(fields),
            Value::Struct { name, fields } if fields.len() == 1 => {
                drop(name);
                take_single_owner_field(fields)
            }
            Value::Enum {
                name,
                variant,
                mut fields,
            } if fields.len() == 1 => {
                drop(name);
                drop(variant);
                let child = fields
                    .pop()
                    .expect("classified owner wrapper has one child");
                drop(fields);
                child
            }
            other => {
                // Classification precedes mutation of an exclusively owned
                // payload. Keep an ordinary ordered drop if another caller
                // ever supplies a shape outside that contract.
                drop_owner_synchronously(other);
                return;
            }
        };
    }
}

fn drop_owner_result_synchronously(result: KuResult<Value>) {
    // Finish this discarded result before another payload or accounting step.
    // The leaf Task may start its own iterative drain, but cannot defer its
    // cleanup into an outer drain and overtake an ordered sibling or ACK.
    let _synchronous = SynchronousOwnerDropGuard::enter();
    match result {
        Ok(value) if has_single_task_leaf(&value) => drop_single_task_leaf(value),
        result => drop(result),
    }
}

fn drain_owner_results() {
    if OWNER_RESULT_RELEASE_ACTIVE.with(|active| active.replace(true)) {
        return;
    }
    let drained = catch_unwind(AssertUnwindSafe(flush_owner_results));
    OWNER_RESULT_RELEASE_ACTIVE.with(|active| active.set(false));
    if let Err(panic) = drained {
        std::panic::resume_unwind(panic);
    }
}

fn flush_owner_results() {
    // A queued frame may capture completed owners while an outer result drain
    // is active. Its ACK/accounting must not overtake detached single-Task-leaf
    // payloads. Re-entering this loop preserves that order. Only strict linear
    // wrapper paths ending in a Task are flat; branches, other Value trees and
    // queued captures keep their synchronous cleanup and recursion risks.
    let flush = OwnerResultFlushGuard::enter();
    let mut first_panic = None;
    loop {
        let Some(state) = OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow_mut().take()) else {
            break;
        };
        let work = state
            .owner_result_release
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("only a unique detached owner result is queued");
        let OwnerResultRelease::Pending {
            result,
            context,
            next,
        } = work
        else {
            unreachable!("pending drain owns only pending payload nodes");
        };
        OWNER_RESULT_RELEASE_HEAD.with(|head| *head.borrow_mut() = next);
        // Neither the state mutex nor the TLS RefCell borrow survives a drop.
        // A nested lease therefore only enqueues; every queued node has already
        // been allocated as part of its TaskState, even during OOM cleanup.
        let dropped = catch_unwind(AssertUnwindSafe(|| {
            let _cleanup = CleanupGuard::enter(context);
            match result {
                Ok(value) => drop_single_task_leaf(value),
                Err(error) => drop_owner_synchronously(error),
            }
        }));
        if let Err(panic) = dropped {
            if first_panic.is_none() {
                first_panic = Some(panic);
            }
        }
        OWNER_RESULT_FINISHED_HEAD.with(|head| {
            let mut head = head.borrow_mut();
            *state
                .owner_result_release
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                Some(OwnerResultRelease::Finished { next: head.take() });
            *head = Some(state);
        });
    }
    if flush.is_outermost() {
        // A nested single-Task leaf drop may only have enqueued its child.
        // Reentrant flushes can also finish nodes while an enclosing native
        // destructor is still live. Publish nothing until the actual outer
        // pending drain has reached quiescence, including panic cleanup.
        loop {
            let Some(state) = OWNER_RESULT_FINISHED_HEAD.with(|head| head.borrow_mut().take())
            else {
                break;
            };
            let finished = state
                .owner_result_release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("finished drain owns its original inline node");
            let OwnerResultRelease::Finished { next } = finished else {
                unreachable!("finished drain owns only destroyed payload nodes");
            };
            OWNER_RESULT_FINISHED_HEAD.with(|head| *head.borrow_mut() = next);
            state.owner_payload_done.store(true, Ordering::SeqCst);
            state.finish_owner_cleanup();
        }
    }
    // A destructor panic must not leave TLS owning an undrained linked list or
    // make a later independent release silently skip its cleanup. Keep the
    // existing panic boundary, after draining all already-detached results.
    if let Some(panic) = first_panic {
        std::panic::resume_unwind(panic);
    }
}

impl Drop for TaskOwnerLease {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.release_owner(
                current_cleanup_context()
                    .or_else(current_execution_termination)
                    .or_else(current_task_cancellation)
                    .unwrap_or_else(|| CancellationContext::new(TerminationReason::Cancelled)),
            );
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRuntimeSnapshot {
    pub active_tasks: usize,
    pub registered_tasks: usize,
    pub queued_tasks: usize,
    pub pending_tasks: usize,
    pub running_tasks: usize,
    pub waiting_tasks: usize,
    pub cancelling_tasks: usize,
    pub completed_tasks: usize,
    pub failed_tasks: usize,
    pub cancelled_tasks: usize,
    pub panicked_tasks: usize,
    pub wait_edges: usize,
    pub queued_blocking_jobs: usize,
    pub running_blocking_jobs: usize,
    pub task_workers: usize,
    pub blocking_workers: usize,
    pub shutdown: bool,
    pub total_submissions: usize,
    pub accepted_submissions: usize,
    pub rejected_task_limit: usize,
    pub rejected_task_queue: usize,
    pub rejected_task_internal: usize,
    pub finished_tasks: usize,
    pub suppressed_cleanup_outcomes: usize,
    pub cleanup_timeouts: usize,
    pub cleanup_unfinished_tasks: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskStressReport {
    pub demand: usize,
    pub producers: usize,
    pub hold_ms: u64,
    pub peak_active: usize,
    pub accepted: usize,
    pub rejected_limit: usize,
    pub rejected_queue: usize,
    pub rejected_internal: usize,
    pub finished: usize,
    pub submit_ms: u128,
    pub total_ms: u128,
    pub task_workers: usize,
    pub blocking_workers: usize,
}

struct TaskState {
    id: i64,
    runtime: Weak<TaskRuntimeInner>,
    queue_consumed: AtomicBool,
    // Execution/queue retirement and completion cleanup are independent of
    // public terminal result delivery and of the accounting once-claim.
    execution_queue_retired: AtomicBool,
    completion_cleanup_done: AtomicBool,
    accounting_finished: AtomicBool,
    accounting_done: AtomicBool,
    result: Mutex<Option<KuResult<Value>>>,
    // Always acquire result before pending_run. Claiming execution and taking a
    // queued frame for cancellation must have one arbitration point.
    pending_run: Mutex<Option<TaskStart>>,
    owner_released: AtomicBool,
    // Release arbitration is not acknowledgement. The first release call
    // and its detached payload drain can finish on different threads; both
    // actual obligations must finish before owner cleanup is published.
    owner_call_done: AtomicBool,
    owner_payload_done: AtomicBool,
    owner_cleanup_done: AtomicBool,
    owner_result_release: Mutex<Option<OwnerResultRelease>>,
    ready: Condvar,
    cancelled: AtomicBool,
    cancellation: Mutex<Option<CancellationContext>>,
    awaited: AtomicBool,
    status: AtomicU8,
}

impl TaskRuntime {
    pub fn new() -> Self {
        Self::with_limits(
            runtime_worker_count(),
            MAX_TASK_QUEUE,
            blocking_worker_count(),
            MAX_BLOCKING_QUEUE,
            MAX_TASKS,
        )
    }

    pub(crate) fn with_limits(
        task_workers: usize,
        task_queue_limit: usize,
        blocking_workers: usize,
        blocking_queue_limit: usize,
        max_tasks: usize,
    ) -> Self {
        let (task_tx, task_rx) = mpsc::sync_channel(task_queue_limit);
        let (blocking_tx, blocking_rx) = mpsc::sync_channel(blocking_queue_limit);
        let inner = Arc::new(TaskRuntimeInner {
            task_tx,
            task_rx: Arc::new(Mutex::new(TaskQueueReceiver {
                receiver: task_rx,
                head: None,
            })),
            blocking_tx,
            states: Mutex::new(HashMap::new()),
            wait_edges: Mutex::new(HashMap::new()),
            active_tasks: AtomicUsize::new(0),
            queued_tasks: AtomicUsize::new(0),
            queued_blocking_jobs: AtomicUsize::new(0),
            running_blocking_jobs: AtomicUsize::new(0),
            total_submissions: AtomicUsize::new(0),
            accepted_submissions: AtomicUsize::new(0),
            rejected_task_limit: AtomicUsize::new(0),
            rejected_task_queue: AtomicUsize::new(0),
            rejected_task_internal: AtomicUsize::new(0),
            finished_tasks: AtomicUsize::new(0),
            suppressed_cleanup_outcomes: AtomicUsize::new(0),
            cleanup_timeouts: AtomicUsize::new(0),
            cleanup_unfinished_tasks: AtomicUsize::new(0),
            next_task_id: AtomicI64::new(1),
            shutdown: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            max_tasks,
            task_queue_limit,
            blocking_queue_limit,
            task_workers: AtomicUsize::new(0),
            blocking_workers: AtomicUsize::new(0),
        });
        spawn_task_workers(&inner, task_workers);
        spawn_blocking_workers(&inner, blocking_workers, blocking_rx);
        Self { inner }
    }

    pub(crate) fn spawn<F>(&self, run: F) -> KuResult<TaskHandle>
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        self.spawn_deferred(|| run)
    }

    pub(crate) fn spawn_deferred<B, F>(&self, build: B) -> KuResult<TaskHandle>
    where
        B: FnOnce() -> F,
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        self.spawn_start_deferred(|| TaskStart::Callback(Box::new(build())))
    }

    fn spawn_start_deferred<B>(&self, build: B) -> KuResult<TaskHandle>
    where
        B: FnOnce() -> TaskStart,
    {
        ensure_task_operations_allowed(Span::default())?;
        self.inner.total_submissions.fetch_add(1, Ordering::Relaxed);
        let id = self.inner.next_task_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(TaskState {
            id,
            runtime: Arc::downgrade(&self.inner),
            queue_consumed: AtomicBool::new(false),
            execution_queue_retired: AtomicBool::new(false),
            completion_cleanup_done: AtomicBool::new(false),
            accounting_finished: AtomicBool::new(false),
            accounting_done: AtomicBool::new(false),
            result: Mutex::new(None),
            pending_run: Mutex::new(None),
            owner_released: AtomicBool::new(false),
            owner_call_done: AtomicBool::new(false),
            owner_payload_done: AtomicBool::new(false),
            owner_cleanup_done: AtomicBool::new(false),
            owner_result_release: Mutex::new(None),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cancellation: Mutex::new(None),
            awaited: AtomicBool::new(false),
            status: AtomicU8::new(TASK_PENDING),
        });
        let handle = TaskHandle {
            id,
            state: Arc::clone(&state),
            runtime: self.clone(),
            owner: Arc::new(TaskOwnerLease {
                state: Arc::downgrade(&state),
            }),
        };
        if self.inner.closing.load(Ordering::Acquire)
            || self.inner.task_workers.load(Ordering::Acquire) == 0
        {
            self.inner
                .rejected_task_internal
                .fetch_add(1, Ordering::Relaxed);
            state.complete(
                id,
                Ok(task_error(
                    "runtime_stopped",
                    "async task runtime has no available workers",
                )),
            );
            return Ok(handle);
        }
        if self
            .inner
            .active_tasks
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.inner.max_tasks).then_some(current + 1)
            })
            .is_err()
        {
            self.inner
                .rejected_task_limit
                .fetch_add(1, Ordering::Relaxed);
            state.complete(
                id,
                Ok(task_error(
                    "too_many_tasks",
                    format!("async task limit {} reached", self.inner.max_tasks),
                )),
            );
            return Ok(handle);
        }
        let mut reservation = TaskSubmissionReservation::new(self, id);
        // The sole producer handle stays outside this unwind boundary. It is
        // dropped after rollback under the already established cleanup context.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let registration = {
                match self.inner.states.lock() {
                    Ok(mut states) => {
                        if self.inner.closing.load(Ordering::Acquire) {
                            TaskRegistration::Closing
                        } else {
                            states.insert(id, Arc::downgrade(&state));
                            TaskRegistration::Ready
                        }
                    }
                    Err(_) => TaskRegistration::Poisoned,
                }
            };
            // No rollback, completion, flush or user cleanup under the registry lock.
            match registration {
                TaskRegistration::Ready => {}
                TaskRegistration::Closing => {
                    reservation.rollback();
                    self.inner
                        .rejected_task_internal
                        .fetch_add(1, Ordering::Relaxed);
                    state.complete(
                        id,
                        Ok(task_error(
                            "runtime_stopped",
                            "async task runtime is closing",
                        )),
                    );
                    return;
                }
                TaskRegistration::Poisoned => {
                    reservation.rollback();
                    self.inner
                        .rejected_task_internal
                        .fetch_add(1, Ordering::Relaxed);
                    state.complete(
                        id,
                        Err(KuError::runtime(
                            "async task registry is poisoned",
                            Span::default(),
                        )),
                    );
                    return;
                }
            }
            let run = match catch_unwind(AssertUnwindSafe(build)) {
                Ok(run) => run,
                Err(_) => {
                    reservation.rollback();
                    self.inner
                        .rejected_task_internal
                        .fetch_add(1, Ordering::Relaxed);
                    state.complete(
                        id,
                        Ok(task_error(
                            "spawn_panic",
                            "async task construction panicked",
                        )),
                    );
                    return;
                }
            };
            let header_overflow = run.runner_header_bytes() > local_driver::MAX_TYPED_RUNNER_HEADER;
            state.install_run(run);
            if header_overflow {
                reservation.rollback();
                self.inner
                    .rejected_task_internal
                    .fetch_add(1, Ordering::Relaxed);
                state.reject_queued(id, Err(local_driver::storage_error()));
                return;
            }
            if !reservation.reserve_queue() {
                self.inner
                    .rejected_task_queue
                    .fetch_add(1, Ordering::Relaxed);
                reservation.rollback();
                state.reject_queued(
                    id,
                    Ok(task_error(
                        "queue_full",
                        format!(
                            "async task queue limit {} reached",
                            self.inner.task_queue_limit
                        ),
                    )),
                );
                return;
            }
            match self.inner.task_tx.try_send(TaskJob {
                id,
                state: Arc::clone(&state),
            }) {
                Ok(()) => {
                    // A worker may already own this accepted node. Transfer the
                    // accounting responsibility before any subsequent operation.
                    reservation.commit_to_worker();
                    self.inner
                        .accepted_submissions
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    self.inner
                        .rejected_task_queue
                        .fetch_add(1, Ordering::Relaxed);
                    reservation.rollback();
                    state.reject_queued(
                        id,
                        Ok(task_error(
                            "queue_full",
                            format!(
                                "async task queue limit {} reached",
                                self.inner.task_queue_limit
                            ),
                        )),
                    );
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.inner
                        .rejected_task_internal
                        .fetch_add(1, Ordering::Relaxed);
                    reservation.rollback();
                    state.reject_queued(
                        id,
                        Ok(task_error(
                            "runtime_stopped",
                            "async task runtime is stopped",
                        )),
                    );
                }
            }
        }));
        if let Err(panic) = outcome {
            let _cleanup = state.cancellation().map(CleanupGuard::enter);
            let mut first_panic = Some(panic);
            reservation.rollback();
            state.catch_cleanup_panic(&mut first_panic, || drop(handle));
            std::panic::resume_unwind(first_panic.expect("producer owns its first panic"));
        }
        Ok(handle)
    }

    pub(crate) fn spawn_runner_deferred<B, F, R>(&self, build: B) -> KuResult<TaskHandle>
    where
        B: FnOnce() -> F,
        F: FnOnce() -> R + Send + 'static,
        R: LocalTaskRunner + 'static,
    {
        self.spawn_start_deferred(|| {
            let factory = build();
            TaskStart::Runner {
                factory: Box::new(move || Box::new(factory())),
                header_bytes: std::mem::size_of::<R>(),
            }
        })
    }

    pub(crate) fn has_task_submissions(&self) -> bool {
        // Monotonic: active_tasks can return to zero while an unawaited completed
        // Task still owns a payload. Rejected submissions also return Task handles.
        self.inner.total_submissions.load(Ordering::Acquire) != 0
    }

    pub fn snapshot(&self) -> KuResult<TaskRuntimeSnapshot> {
        let states =
            self.inner.states.lock().map_err(|_| {
                KuError::runtime("async task registry is poisoned", Span::default())
            })?;
        let mut snapshot = TaskRuntimeSnapshot {
            active_tasks: self.inner.active_tasks.load(Ordering::Acquire),
            registered_tasks: 0,
            queued_tasks: self.inner.queued_tasks.load(Ordering::Acquire),
            pending_tasks: 0,
            running_tasks: 0,
            waiting_tasks: 0,
            cancelling_tasks: 0,
            completed_tasks: 0,
            failed_tasks: 0,
            cancelled_tasks: 0,
            panicked_tasks: 0,
            wait_edges: 0,
            queued_blocking_jobs: self.inner.queued_blocking_jobs.load(Ordering::Acquire),
            running_blocking_jobs: self.inner.running_blocking_jobs.load(Ordering::Acquire),
            task_workers: self.inner.task_workers.load(Ordering::Acquire),
            blocking_workers: self.inner.blocking_workers.load(Ordering::Acquire),
            shutdown: self.inner.shutdown.load(Ordering::Acquire)
                || self.inner.closing.load(Ordering::Acquire),
            total_submissions: self.inner.total_submissions.load(Ordering::Relaxed),
            accepted_submissions: self.inner.accepted_submissions.load(Ordering::Relaxed),
            rejected_task_limit: self.inner.rejected_task_limit.load(Ordering::Relaxed),
            rejected_task_queue: self.inner.rejected_task_queue.load(Ordering::Relaxed),
            rejected_task_internal: self.inner.rejected_task_internal.load(Ordering::Relaxed),
            finished_tasks: self.inner.finished_tasks.load(Ordering::Relaxed),
            suppressed_cleanup_outcomes: self
                .inner
                .suppressed_cleanup_outcomes
                .load(Ordering::Relaxed),
            cleanup_timeouts: self.inner.cleanup_timeouts.load(Ordering::Relaxed),
            cleanup_unfinished_tasks: self.inner.cleanup_unfinished_tasks.load(Ordering::Relaxed),
        };
        for state in states.values().filter_map(Weak::upgrade) {
            snapshot.registered_tasks += 1;
            match state.status.load(Ordering::Acquire) {
                TASK_PENDING => snapshot.pending_tasks += 1,
                TASK_RUNNING => snapshot.running_tasks += 1,
                TASK_WAITING => snapshot.waiting_tasks += 1,
                TASK_CANCELLING => snapshot.cancelling_tasks += 1,
                TASK_COMPLETED => snapshot.completed_tasks += 1,
                TASK_FAILED => snapshot.failed_tasks += 1,
                TASK_CANCELLED => snapshot.cancelled_tasks += 1,
                TASK_PANICKED => snapshot.panicked_tasks += 1,
                _ => {}
            }
        }
        drop(states);
        snapshot.wait_edges = self
            .inner
            .wait_edges
            .lock()
            .map_err(|_| KuError::runtime("async wait graph is poisoned", Span::default()))?
            .len();
        Ok(snapshot)
    }

    pub fn stress_concurrent_demand(
        &self,
        demand: usize,
        producers: usize,
        hold: Duration,
    ) -> KuResult<TaskStressReport> {
        const MAX_STRESS_DEMAND: usize = 10_000_000;
        const MAX_STRESS_PRODUCERS: usize = 64;
        const MAX_STRESS_HOLD: Duration = Duration::from_secs(60);
        const STRESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

        if demand == 0 || demand > MAX_STRESS_DEMAND {
            return Err(KuError::runtime(
                format!("task.stress demand must be between 1 and {MAX_STRESS_DEMAND}"),
                Span::default(),
            ));
        }
        if producers == 0 || producers > MAX_STRESS_PRODUCERS {
            return Err(KuError::runtime(
                format!("task.stress producers must be between 1 and {MAX_STRESS_PRODUCERS}"),
                Span::default(),
            ));
        }
        if hold > MAX_STRESS_HOLD {
            return Err(KuError::runtime(
                "task.stress hold_ms must be between 0 and 60000",
                Span::default(),
            ));
        }

        let before = self.snapshot()?;
        if before.active_tasks != 0
            || before.queued_tasks != 0
            || before.queued_blocking_jobs != 0
            || before.running_blocking_jobs != 0
        {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "task",
                "stress_runtime_busy",
                "task.stress requires an idle task runtime so metrics do not mix with application tasks",
                Span::default(),
            ));
        }
        let release = Arc::new(AtomicBool::new(false));
        let peak_active = Arc::new(AtomicUsize::new(before.active_tasks));
        // The stress harness is the owner until its release/drain phase. Dropping
        // each submitted handle would now correctly cancel that task immediately.
        let retained = Arc::new(Mutex::new(Vec::with_capacity(self.inner.max_tasks)));
        let began = Instant::now();
        let mut producers_threads = Vec::with_capacity(producers);
        for producer in 0..producers {
            let runtime = self.clone();
            let producer_release = Arc::clone(&release);
            let peak_active = Arc::clone(&peak_active);
            let retained = Arc::clone(&retained);
            let count = demand / producers + usize::from(producer < demand % producers);
            let producer_thread = thread::Builder::new()
                .name(format!("ku-stress-producer-{producer}"))
                .spawn(move || -> KuResult<()> {
                    for _ in 0..count {
                        let release = Arc::clone(&producer_release);
                        let task = runtime.spawn(move || {
                            while !release.load(Ordering::Acquire) && !current_task_cancelled() {
                                thread::yield_now();
                            }
                            Ok(Value::Null)
                        })?;
                        if !task.state.is_terminal() {
                            let mut retained = retained
                                .lock()
                                .map_err(|_| KuError::message("stress owner registry poisoned"))?;
                            if retained.len() >= runtime.inner.max_tasks {
                                return Err(KuError::message(
                                    "stress retained-task budget exceeded",
                                ));
                            }
                            retained.push(task);
                        }
                        peak_active.fetch_max(
                            runtime.inner.active_tasks.load(Ordering::Acquire),
                            Ordering::AcqRel,
                        );
                    }
                    Ok(())
                });
            match producer_thread {
                Ok(producer_thread) => producers_threads.push(producer_thread),
                Err(err) => {
                    release.store(true, Ordering::Release);
                    for producer_thread in producers_threads {
                        let _ = producer_thread.join();
                    }
                    return Err(KuError::runtime(
                        format!("failed to start task stress producer: {err}"),
                        Span::default(),
                    ));
                }
            }
        }
        let mut producer_panicked = false;
        for producer in producers_threads {
            producer_panicked |= !matches!(producer.join(), Ok(Ok(())));
        }
        if producer_panicked {
            release.store(true, Ordering::Release);
            return Err(KuError::runtime(
                "task stress producer panicked",
                Span::default(),
            ));
        }
        let submit_elapsed = began.elapsed();
        if !hold.is_zero() {
            thread::sleep(hold);
        }
        release.store(true, Ordering::Release);

        let drain_deadline = Instant::now() + STRESS_DRAIN_TIMEOUT;
        while self.inner.active_tasks.load(Ordering::Acquire) > before.active_tasks {
            if Instant::now() >= drain_deadline {
                return Err(KuError::structured(
                    crate::error::KuErrorKind::Runtime,
                    "task",
                    "stress_timeout",
                    "task stress workload did not drain before the bounded timeout",
                    Span::default(),
                ));
            }
            if !self.help_one_bounded()? {
                thread::sleep(Duration::from_millis(2));
            }
        }

        let after = self.snapshot()?;
        drop(retained);
        Ok(TaskStressReport {
            demand,
            producers,
            hold_ms: hold.as_millis() as u64,
            peak_active: peak_active
                .load(Ordering::Acquire)
                .saturating_sub(before.active_tasks),
            accepted: after
                .accepted_submissions
                .saturating_sub(before.accepted_submissions),
            rejected_limit: after
                .rejected_task_limit
                .saturating_sub(before.rejected_task_limit),
            rejected_queue: after
                .rejected_task_queue
                .saturating_sub(before.rejected_task_queue),
            rejected_internal: after
                .rejected_task_internal
                .saturating_sub(before.rejected_task_internal),
            finished: after.finished_tasks.saturating_sub(before.finished_tasks),
            submit_ms: submit_elapsed.as_millis(),
            total_ms: began.elapsed().as_millis(),
            task_workers: after.task_workers,
            blocking_workers: after.blocking_workers,
        })
    }

    pub fn await_task(&self, handle: &TaskHandle) -> KuResult<Value> {
        await_ticket::await_borrowed(self.clone(), handle, await_ticket::AwaitMode::Ordinary)
    }

    pub fn await_task_timeout(&self, handle: &TaskHandle, timeout: Duration) -> KuResult<Value> {
        await_ticket::await_borrowed(
            self.clone(),
            handle,
            await_ticket::AwaitMode::Diagnostic(timeout),
        )
    }

    #[cfg(test)]
    fn await_task_until(
        &self,
        handle: &TaskHandle,
        deadline: Option<Instant>,
        consume: bool,
    ) -> KuResult<Value> {
        await_ticket::await_borrowed(
            self.clone(),
            handle,
            await_ticket::AwaitMode::AlreadyClaimed { deadline, consume },
        )
    }

    pub fn cancel_all(&self) -> usize {
        self.cancel_all_with(CancellationContext::new(TerminationReason::Cancelled))
    }

    fn cancel_all_with(&self, context: CancellationContext) -> usize {
        let states = self
            .inner
            .states
            .lock()
            .map(|states| {
                states
                    .values()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        states
            .into_iter()
            .filter(|state| state.request_cancel_with(context))
            .count()
    }

    pub fn cancel_all_and_wait(&self, timeout: Duration) -> KuResult<usize> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            KuError::runtime(
                "task shutdown timeout is too large for this platform",
                Span::default(),
            )
        })?;
        self.inner.closing.store(true, Ordering::Release);
        let context = CancellationContext::new(TerminationReason::Cancelled);
        let context = CancellationContext {
            cleanup_deadline: context.cleanup_deadline.min(deadline),
            ..context
        };
        let _cleanup = CleanupGuard::enter(context);
        let cancelled = self.cancel_all_with(context);
        while self.inner.active_tasks.load(Ordering::Acquire) != 0
            || self.inner.queued_blocking_jobs.load(Ordering::Acquire) != 0
            || self.inner.running_blocking_jobs.load(Ordering::Acquire) != 0
        {
            if Instant::now() >= context.cleanup_deadline {
                self.record_cleanup_timeout();
                self.inner.cleanup_unfinished_tasks.fetch_add(
                    self.inner.active_tasks.load(Ordering::Acquire),
                    Ordering::Relaxed,
                );
                return Err(KuError::structured(
                    crate::error::KuErrorKind::Runtime,
                    "task",
                    "shutdown_timeout",
                    "async tasks did not stop before the bounded shutdown timeout",
                    Span::default(),
                ));
            }
            if self.retire_cancelled_queue_prefix(context) {
                continue;
            }
            // Cleanup must not execute an unrelated queued user continuation.
            // A live head or busy receiver remains the existing workers'
            // responsibility; cancellation does not invoke scheduler help.
            thread::sleep(
                Duration::from_millis(2).min(
                    context
                        .cleanup_deadline
                        .saturating_duration_since(Instant::now()),
                ),
            );
        }
        Ok(cancelled)
    }

    /// Retire at most one already-cleaned cancellation node without entering a
    /// user body. A live head remains ahead of every later channel node, with
    /// its original logical queue credit. This is progress, not a full owner
    /// or runtime cleanup acknowledgement.
    pub(crate) fn retire_cancelled_queue_prefix(&self, context: CancellationContext) -> bool {
        let _cleanup = CleanupGuard::enter(context);
        let context = current_cleanup_context().expect("cleanup guard installs its context");
        if Instant::now() >= context.cleanup_deadline {
            return false;
        }
        let job = {
            let mut receiver = match self.inner.task_rx.try_lock() {
                Ok(receiver) => receiver,
                // A waiting worker already owns recv. Poison is still reported
                // by ordinary consumers; cleanup must not claim retirement
                // from a receiver it cannot safely inspect this turn.
                Err(_) => return false,
            };
            if Instant::now() >= context.cleanup_deadline {
                return false;
            }
            let job = match receiver.try_recv() {
                Ok(job) => job,
                Err(_) => return false,
            };
            if !job.state.is_terminal()
                || !job.state.is_cancelled()
                || !job.state.completion_cleanup_done.load(Ordering::SeqCst)
            {
                // try_recv took the only cached head, if present. Installing
                // this same node neither drops it nor adds another queue slot.
                receiver.head = Some(job);
                return false;
            }
            self.inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
            job
        };
        // Keep the internal state alive until the actual queue node is gone.
        // The guard follows real Drop on normal return and native unwind; no
        // payload destructor or registry update runs under the receiver lock.
        let state = Arc::clone(&job.state);
        let _retirement = QueueRetirement {
            inner: &self.inner,
            state: &state,
        };
        let job = job;
        drop(job);
        true
    }

    pub(crate) fn record_cleanup_suppressed(&self) {
        self.inner
            .suppressed_cleanup_outcomes
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_cleanup_timeout(&self) {
        self.inner.cleanup_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn cancel_handles_and_wait(
        &self,
        handles: &[TaskHandle],
        context: CancellationContext,
    ) -> KuResult<()> {
        let _cleanup = CleanupGuard::enter(context);
        let mut batch = TaskCancelBatch::prepare(handles, context);
        self.wait_prepared_cancel_batch(&mut batch)
    }

    /// Drive the same terminal observation after an exit state has already
    /// broadcast cancellation. The caller supplies its synchronous guard; this
    /// helper neither rebroadcasts nor claims await or runs scheduler help.
    pub(crate) fn wait_prepared_cancel_batch<H: AsRef<[TaskHandle]>>(
        &self,
        batch: &mut TaskCancelBatch<H>,
    ) -> KuResult<()> {
        loop {
            let now = Instant::now();
            match batch.poll(self, now) {
                CancelBatchPoll::TerminalObserved => return Ok(()),
                CancelBatchPoll::TimedOut(context) => {
                    return Err(KuError::termination(context, Span::default()));
                }
                CancelBatchPoll::Pending { first_index, .. } => {
                    self.wait_prepared_cancel_once(batch, first_index)?;
                }
            }
        }
    }

    /// One original bounded Condvar wait. A persistent exit resumes its own
    /// phase afterward; this never claims await or executes unrelated help.
    pub(crate) fn wait_prepared_cancel_once<H: AsRef<[TaskHandle]>>(
        &self,
        batch: &TaskCancelBatch<H>,
        first_index: usize,
    ) -> KuResult<()> {
        batch.handles()[first_index].state.wait(
            batch
                .context()
                .cleanup_deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(2)),
        )
    }

    pub fn run_blocking<F>(&self, run: F, _span: Span) -> KuResult<Value>
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        ensure_task_operations_allowed(_span)?;
        if self.inner.closing.load(Ordering::Acquire) {
            return Ok(task_error(
                "blocking_pool_stopped",
                "blocking pool is closing",
            ));
        }
        if self.inner.blocking_workers.load(Ordering::Acquire) == 0 {
            return Ok(task_error(
                "blocking_pool_stopped",
                "blocking pool has no available workers",
            ));
        }
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        // Share the task admission lock with shutdown's registry snapshot. A
        // shutdown cannot observe an empty pool just before this submission.
        let admission = self
            .inner
            .states
            .lock()
            .map_err(|_| KuError::runtime("async task registry is poisoned", _span))?;
        if self.inner.closing.load(Ordering::Acquire) {
            return Ok(task_error(
                "blocking_pool_stopped",
                "blocking pool is closing",
            ));
        }
        self.inner
            .queued_blocking_jobs
            .fetch_add(1, Ordering::AcqRel);
        let submitted = self.inner.blocking_tx.try_send(BlockingJob {
            run: Box::new(run),
            response: response_tx,
            cancelled: CURRENT_TASK_STATE
                .with(|current| current.borrow().as_ref().and_then(Weak::upgrade)),
        });
        drop(admission);
        match submitted {
            Ok(()) => loop {
                if let Some(context) = current_task_cancellation() {
                    break Err(KuError::termination(context, _span));
                }
                match response_rx.try_recv() {
                    Ok(result) => {
                        if let Some(context) = current_task_cancellation() {
                            let _cleanup = CleanupGuard::enter(context);
                            drop_owner_result_synchronously(result);
                            break Err(KuError::termination(context, _span));
                        }
                        break result;
                    }
                    Err(TryRecvError::Disconnected) => {
                        break Ok(task_error(
                            "blocking_pool_stopped",
                            "blocking pool stopped before returning a result",
                        ))
                    }
                    Err(TryRecvError::Empty) => {
                        if !self.help_one_bounded()? {
                            match response_rx.recv_timeout(Duration::from_millis(2)) {
                                Ok(result) => {
                                    if let Some(context) = current_task_cancellation() {
                                        let _cleanup = CleanupGuard::enter(context);
                                        drop_owner_result_synchronously(result);
                                        break Err(KuError::termination(context, _span));
                                    }
                                    break result;
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    break Ok(task_error(
                                        "blocking_pool_stopped",
                                        "blocking pool stopped before returning a result",
                                    ))
                                }
                            }
                        }
                    }
                }
            },
            Err(TrySendError::Full(_)) => {
                self.inner
                    .queued_blocking_jobs
                    .fetch_sub(1, Ordering::AcqRel);
                Ok(task_error(
                    "queue_full",
                    format!(
                        "blocking queue limit {} reached",
                        self.inner.blocking_queue_limit
                    ),
                ))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner
                    .queued_blocking_jobs
                    .fetch_sub(1, Ordering::AcqRel);
                Ok(task_error(
                    "blocking_pool_stopped",
                    "blocking pool is stopped",
                ))
            }
        }
    }

    fn help_one_bounded(&self) -> KuResult<bool> {
        let previous = AWAIT_HELP_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= MAX_AWAIT_DEPTH {
                None
            } else {
                depth.set(current + 1);
                Some(current)
            }
        });
        let Some(previous) = previous else {
            return Ok(false);
        };
        let _depth = AwaitHelpDepthGuard {
            previous,
            _same_thread: std::marker::PhantomData,
        };
        self.help_one()
    }

    fn help_one(&self) -> KuResult<bool> {
        let job = {
            // An idle worker blocks in `recv()` while holding the single-consumer
            // receiver lock. In that case it will wake for the queued job itself,
            // so an awaiting worker must not block behind it. When every worker is
            // busy (notably the one-worker nested-await case), the lock is free and
            // this helper can execute one queued child to avoid starvation.
            let mut receiver = match self.inner.task_rx.try_lock() {
                Ok(receiver) => receiver,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(KuError::runtime(
                        "async task queue is poisoned",
                        Span::default(),
                    ));
                }
            };
            match receiver.try_recv() {
                Ok(job) => Some(job),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    return Err(KuError::runtime(
                        "async task queue is disconnected",
                        Span::default(),
                    ))
                }
            }
        };
        if let Some(job) = job {
            self.inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
            execute_task_job(&self.inner, job);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn state(&self, id: i64) -> KuResult<Option<Arc<TaskState>>> {
        self.inner
            .states
            .lock()
            .map(|states| states.get(&id).and_then(Weak::upgrade))
            .map_err(|_| KuError::runtime("async task registry is poisoned", Span::default()))
    }

    fn remove_state(&self, id: i64) {
        if let Ok(mut states) = self.inner.states.lock() {
            states.remove(&id);
        }
    }

    fn register_wait(&self, current: i64, target: i64) -> KuResult<Option<&'static str>> {
        let mut edges = self
            .inner
            .wait_edges
            .lock()
            .map_err(|_| KuError::runtime("async wait graph is poisoned", Span::default()))?;
        edges.insert(current, target);
        let mut cursor = target;
        for _ in 0..MAX_AWAIT_DEPTH {
            if cursor == current {
                edges.remove(&current);
                return Ok(Some("await_cycle"));
            }
            let Some(next) = edges.get(&cursor).copied() else {
                return Ok(None);
            };
            cursor = next;
        }
        edges.remove(&current);
        Ok(Some("await_depth"))
    }

    fn clear_wait(&self, current: i64) {
        if let Ok(mut edges) = self.inner.wait_edges.lock() {
            edges.remove(&current);
        }
    }
}

impl Default for TaskRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TaskRuntimeInner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl TaskHandle {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn await_result(&self) -> KuResult<Value> {
        self.runtime.await_task(self)
    }

    pub fn await_timeout(&self, timeout: Duration) -> KuResult<Value> {
        self.runtime.await_task_timeout(self, timeout)
    }

    pub fn cancel(&self) -> bool {
        self.state.request_cancel()
    }

    pub(crate) fn request_cancel_with(&self, context: CancellationContext) -> bool {
        self.state.request_cancel_with(context)
    }

    pub(crate) fn release_scope_owner(&self, context: CancellationContext) {
        // Readonly captures can retain an internal handle proxy after the real
        // owning scope exits. That proxy may observe identity, not retain the
        // unawaited result or regain ownership of a late completion.
        self.state.release_owner(context);
    }

    pub fn status(&self) -> &'static str {
        self.state.status_name()
    }
}

impl Clone for TaskHandle {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            state: Arc::clone(&self.state),
            runtime: self.runtime.clone(),
            owner: Arc::clone(&self.owner),
        }
    }
}

impl std::fmt::Debug for TaskHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskHandle").field("id", &self.id).finish()
    }
}

impl PartialEq for TaskHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.state, &other.state)
    }
}

impl TaskState {
    fn remember_cleanup_deadline(&self) {
        let Some(cleanup) = current_cleanup_context() else {
            return;
        };
        // Match completion/cancellation's result -> cancellation lock order.
        // Only metadata is touched here; no user payload drops under a lock.
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut cancellation = self
            .cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = cancellation.as_mut() {
            // Actual TLS may have an earlier cause. Persist only its tighter D
            // before the caller's cleanup guard restores its outer TLS window.
            existing.cleanup_deadline = existing.cleanup_deadline.min(cleanup.cleanup_deadline);
            if let Some(Err(error)) = slot.as_mut() {
                if error.runtime_termination().is_some() {
                    // A terminal error may already be published while losing
                    // payloads are being released. Tighten its existing D too;
                    // keep the first cause and all public diagnostic fields.
                    error.remember_scope_cleanup(*existing);
                }
            }
        }
    }

    // Finish the remaining ordered cleanup stages before propagating the first
    // native destructor panic. The caller retains its original cleanup window.
    fn catch_cleanup_panic(&self, first: &mut Option<CleanupPanic>, cleanup: impl FnOnce()) {
        let outcome = catch_unwind(AssertUnwindSafe(cleanup));
        // Nested native Drop guards have restored the caller's live window,
        // carrying their smaller D. Persist it before continuing or unwinding.
        self.remember_cleanup_deadline();
        if let Err(panic) = outcome {
            if first.is_none() {
                *first = Some(panic);
            } else {
                if let Some(runtime) = self.runtime.upgrade() {
                    runtime
                        .suppressed_cleanup_outcomes
                        .fetch_add(1, Ordering::Relaxed);
                }
                drop(panic);
            }
        }
    }

    fn install_run(&self, run: impl Into<TaskStart>) {
        let mut uninstalled = Some(run.into());
        let context = {
            let slot = self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let context = self.cancellation();
            if context.is_none() && slot.is_none() && !self.is_terminal() {
                *self
                    .pending_run
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = uninstalled.take();
            }
            context
        };
        if uninstalled.is_some() {
            // Cancellation may arrive while the deferred frame is being built.
            // The builder, not a worker, owns cleanup of that not-yet-queued frame.
            let _cleanup = context.map(CleanupGuard::enter);
            let mut first_panic = None;
            self.catch_cleanup_panic(&mut first_panic, || {
                drop_owner_synchronously(uninstalled.take());
            });
            self.catch_cleanup_panic(&mut first_panic, flush_owner_results);
            self.remember_cleanup_deadline();
            self.catch_cleanup_panic(&mut first_panic, || {
                self.complete(self.id, Ok(Value::Null));
            });
            self.remember_cleanup_deadline();
            if let Some(panic) = first_panic {
                std::panic::resume_unwind(panic);
            }
        }
    }

    fn claim_run(&self) -> Option<TaskStart> {
        let _slot = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_cancelled() || self.is_terminal() {
            return None;
        }
        let run = self
            .pending_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if run.is_some() {
            self.status.store(TASK_RUNNING, Ordering::Release);
        }
        run
    }

    fn reject_queued(&self, id: i64, result: KuResult<Value>) {
        let (run, cancellation_owns_completion) = {
            let _slot = self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let run = self
                .pending_run
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let cancellation_owns_completion = run.is_none() && self.is_cancelled();
            (run, cancellation_owns_completion)
        };
        let _cleanup = self.cancellation().map(CleanupGuard::enter);
        let mut first_panic = None;
        self.catch_cleanup_panic(&mut first_panic, || {
            drop_owner_synchronously(run);
        });
        self.catch_cleanup_panic(&mut first_panic, flush_owner_results);
        self.remember_cleanup_deadline();
        if !cancellation_owns_completion {
            self.catch_cleanup_panic(&mut first_panic, || self.complete(id, result));
        }
        self.remember_cleanup_deadline();
        if let Some(panic) = first_panic {
            std::panic::resume_unwind(panic);
        }
    }

    fn release_owner(self: &Arc<Self>, context: CancellationContext) {
        let (first_release, abandoned) = {
            let mut slot = self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // Scope release and the final logical lease can race. The loser
            // must not acknowledge the winner's detached, still-live payload.
            let first_release = !self.owner_released.swap(true, Ordering::AcqRel);
            (first_release, slot.take())
        };
        let _cleanup = CleanupGuard::enter(context);
        let mut first_panic = None;
        let mut deferred_payload = false;
        match abandoned {
            Some(result) if result.as_ref().is_ok_and(has_single_task_leaf) => {
                assert!(
                    first_release,
                    "only the first owner release detaches a result"
                );
                deferred_payload = true;
                OWNER_RESULT_RELEASE_HEAD.with(|head| {
                    let mut head = head.borrow_mut();
                    *self
                        .owner_result_release
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(OwnerResultRelease::Pending {
                            result,
                            context,
                            next: head.take(),
                        });
                    *head = Some(Arc::clone(self));
                });
                self.catch_cleanup_panic(&mut first_panic, drain_owner_results);
            }
            Some(result) => {
                assert!(
                    first_release,
                    "only the first owner release detaches a result"
                );
                self.catch_cleanup_panic(&mut first_panic, || drop_owner_synchronously(result));
            }
            None => {}
        }
        // Keep queued-frame cancellation synchronous: its terminal ACK must
        // still follow capture destruction, including a concurrent queue take.
        // A native payload panic must not skip that remaining obligation.
        // Duplicate calls retain cancellation/deadline tightening, but do not
        // publish either obligation on behalf of the first release call.
        self.catch_cleanup_panic(&mut first_panic, || {
            let mut cancellation = context;
            if let Some(cleanup) = current_cleanup_context() {
                cancellation.cleanup_deadline =
                    cancellation.cleanup_deadline.min(cleanup.cleanup_deadline);
            }
            self.request_cancel_with(cancellation);
        });
        self.remember_cleanup_deadline();
        if first_release {
            // During an active deferred drain, a nested release may return
            // after merely enqueuing its descendants. Its payload publication
            // belongs exclusively to the outermost real drain above.
            if !OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get) {
                self.catch_cleanup_panic(&mut first_panic, flush_owner_results);
            }
            if !deferred_payload {
                self.owner_payload_done.store(true, Ordering::SeqCst);
            }
            self.owner_call_done.store(true, Ordering::SeqCst);
            self.finish_owner_cleanup();
        }
        if let Some(panic) = first_panic {
            std::panic::resume_unwind(panic);
        }
    }

    fn finish_owner_cleanup(&self) {
        // Sequentially consistent obligation publications prevent concurrent
        // release/drain finishers from both missing the other completed stage.
        // This acknowledges owner cleanup only, never execution retirement,
        // result delivery, scope teardown or registry accounting.
        if self.owner_call_done.load(Ordering::SeqCst)
            && self.owner_payload_done.load(Ordering::SeqCst)
            && !self.owner_cleanup_done.swap(true, Ordering::AcqRel)
        {
            self.ready.notify_all();
        }
    }

    fn complete(&self, id: i64, result: KuResult<Value>) {
        self.complete_with_status(id, result, None);
    }

    fn complete_with_status(&self, id: i64, result: KuResult<Value>, status: Option<u8>) {
        // Cancellation and completion use the same lock. The worker's earlier
        // cancellation check is only advisory: a request may win before commit.
        // Keep losing payloads here so their destructors run after unlocking.
        let mut uncommitted = Some(result);
        let mut abandoned = None;
        let mut discard_context = None;
        let mut published = false;
        if let Ok(mut slot) = self.result.lock() {
            if slot.is_none() && !self.is_terminal() {
                let incoming = uncommitted
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .and_then(KuError::runtime_termination);
                let context = {
                    let mut cancellation = self
                        .cancellation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if let Some(incoming) = incoming {
                        match cancellation.as_mut() {
                            Some(existing) => {
                                existing.cleanup_deadline =
                                    existing.cleanup_deadline.min(incoming.cleanup_deadline)
                            }
                            None => *cancellation = Some(incoming),
                        }
                    }
                    *cancellation
                };
                discard_context = context;
                let (result, status) = if let Some(context) = context {
                    self.cancelled.store(true, Ordering::Release);
                    let mut error = KuError::termination(context, Span::default());
                    error.message = format!(
                        "task {id} was {}",
                        match context.reason {
                            TerminationReason::Cancelled => "cancelled",
                            TerminationReason::TimedOut => "timed out",
                        }
                    );
                    (
                        Err(error),
                        match context.reason {
                            TerminationReason::Cancelled => TASK_CANCELLED,
                            TerminationReason::TimedOut => TASK_TIMED_OUT,
                        },
                    )
                } else {
                    let result = uncommitted.take().expect("completion owns its result");
                    let status = status.unwrap_or_else(|| {
                        if result.is_err() {
                            TASK_FAILED
                        } else {
                            TASK_COMPLETED
                        }
                    });
                    (result, status)
                };
                if self.owner_released.load(Ordering::Acquire) {
                    abandoned = Some(result);
                } else {
                    *slot = Some(result);
                }
                self.status.store(status, Ordering::Release);
                published = true;
                self.ready.notify_all();
            }
        }
        let _cleanup = discard_context.map(CleanupGuard::enter);
        let mut first_panic = None;
        if let Some(result) = uncommitted.take() {
            self.catch_cleanup_panic(&mut first_panic, || {
                drop_owner_result_synchronously(result);
            });
        }
        if let Some(result) = abandoned.take() {
            self.catch_cleanup_panic(&mut first_panic, || {
                drop_owner_result_synchronously(result);
            });
        }
        self.catch_cleanup_panic(&mut first_panic, flush_owner_results);
        if published {
            // Losing and abandoned payloads, including their real native drop
            // glue, have finished before accounting may acknowledge this task.
            // A duplicate completion must not acknowledge the winning cleanup.
            // Sequentially consistent obligation publications prevent the two
            // finishing threads from both missing the other completed stage.
            self.completion_cleanup_done.store(true, Ordering::SeqCst);
        }
        if let Some(runtime) = self.runtime.upgrade() {
            finish_task_accounting(&runtime, self);
        }
        if let Some(panic) = first_panic {
            std::panic::resume_unwind(panic);
        }
    }

    fn request_cancel(&self) -> bool {
        self.request_cancel_with(CancellationContext::new(TerminationReason::Cancelled))
    }

    fn request_cancel_with(&self, context: CancellationContext) -> bool {
        let Ok(slot) = self.result.lock() else {
            return false;
        };
        if slot.is_some() || self.is_terminal() {
            return false;
        }
        {
            let mut cancellation = self
                .cancellation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(existing) = &mut *cancellation {
                existing.cleanup_deadline = existing.cleanup_deadline.min(context.cleanup_deadline);
                return false;
            }
            *cancellation = Some(context);
        }
        self.cancelled.store(true, Ordering::Release);
        self.status.store(TASK_CANCELLING, Ordering::Release);
        let queued = self
            .pending_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop(slot);
        self.ready.notify_all();
        if let Some(queued) = queued {
            let _cleanup = CleanupGuard::enter(context);
            let mut first_panic = None;
            // Real capture drop glue exits before terminal cancellation is
            // published, even when a native destructor starts unwinding.
            self.catch_cleanup_panic(&mut first_panic, || {
                drop_owner_synchronously(queued);
            });
            self.catch_cleanup_panic(&mut first_panic, flush_owner_results);
            self.catch_cleanup_panic(&mut first_panic, || {
                self.complete(self.id, Ok(Value::Null));
            });
            if let Some(panic) = first_panic {
                std::panic::resume_unwind(panic);
            }
        }
        true
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancellation(&self) -> Option<CancellationContext> {
        if !self.is_cancelled() {
            return None;
        }
        self.cancellation
            .lock()
            .map(|context| *context)
            .unwrap_or_else(|error| *error.into_inner())
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self.status.load(Ordering::Acquire),
            TASK_COMPLETED | TASK_FAILED | TASK_CANCELLED | TASK_PANICKED | TASK_TIMED_OUT
        )
    }

    fn claim_await(&self) -> bool {
        self.awaited
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn set_status(&self, status: u8) {
        // Do not overwrite a cancellation or terminal state between a separate
        // cancelled load and the status store.
        if let Ok(slot) = self.result.lock() {
            if slot.is_none() && !self.is_cancelled() && !self.is_terminal() {
                self.status.store(status, Ordering::Release);
            }
        }
    }

    fn status_name(&self) -> &'static str {
        match self.status.load(Ordering::Acquire) {
            TASK_PENDING => "pending",
            TASK_RUNNING => "running",
            TASK_WAITING => "waiting",
            TASK_COMPLETED => "completed",
            TASK_FAILED => "failed",
            TASK_CANCELLED => "cancelled",
            TASK_CANCELLING => "cancelling",
            TASK_PANICKED => "panicked",
            TASK_TIMED_OUT => "timed_out",
            _ => "unknown",
        }
    }

    fn observe_result(&self, consume: bool) -> KuResult<(Option<KuResult<Value>>, bool)> {
        let mut slot = self
            .result
            .lock()
            .map_err(|_| KuError::runtime("async task state is poisoned", Span::default()))?;
        // A pending empty slot and a consumed terminal slot must be distinguished
        // under the completion lock. A later terminal load could otherwise see a
        // newly installed payload and incorrectly report the first await as used.
        let terminal = self.is_terminal();
        let result = if consume { slot.take() } else { slot.clone() };
        Ok((result, terminal))
    }

    #[cfg(test)]
    fn result(&self) -> KuResult<Option<KuResult<Value>>> {
        self.observe_result(false).map(|(result, _)| result)
    }

    fn wait(&self, timeout: Duration) -> KuResult<()> {
        let slot = self
            .result
            .lock()
            .map_err(|_| KuError::runtime("async task state is poisoned", Span::default()))?;
        let _ = self
            .ready
            .wait_timeout(slot, timeout)
            .map_err(|_| KuError::runtime("async task state is poisoned", Span::default()))?;
        Ok(())
    }
}

fn spawn_task_workers(inner: &Arc<TaskRuntimeInner>, count: usize) {
    for index in 0..count {
        let weak = Arc::downgrade(inner);
        let receiver = Arc::clone(&inner.task_rx);
        if thread::Builder::new()
            .name(format!("ku-task-{index}"))
            .spawn(move || task_worker_loop(weak, receiver))
            .is_ok()
        {
            inner.task_workers.fetch_add(1, Ordering::Release);
        }
    }
}

fn task_worker_loop(weak: Weak<TaskRuntimeInner>, receiver: Arc<Mutex<TaskQueueReceiver>>) {
    loop {
        let Some(shutdown) = weak
            .upgrade()
            .map(|inner| inner.shutdown.load(Ordering::Acquire))
        else {
            return;
        };
        if shutdown {
            return;
        }
        let job = {
            let Ok(mut receiver) = receiver.lock() else {
                return;
            };
            receiver.recv()
        };
        match job {
            Ok(job) => {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
                if let Err(panic) = catch_unwind(AssertUnwindSafe(|| execute_task_job(&inner, job)))
                {
                    // Ordinary callback panics keep their existing task
                    // classification. An escaping cleanup panic has already
                    // retired its queue node; keep the winning terminal result
                    // and the original panic-hook diagnostic, then serve again.
                    inner
                        .suppressed_cleanup_outcomes
                        .fetch_add(1, Ordering::Relaxed);
                    drop(panic);
                }
            }
            Err(_) => return,
        }
    }
}

struct QueueRetirement<'a> {
    inner: &'a TaskRuntimeInner,
    state: &'a TaskState,
}

impl Drop for QueueRetirement<'_> {
    fn drop(&mut self) {
        // Primitive retirement only: no callback, payload cleanup, completion
        // or wait is started by this destructor while another panic unwinds.
        finish_task_job(self.inner, self.state);
    }
}

fn execute_task_job(inner: &TaskRuntimeInner, job: TaskJob) {
    local_driver::execute(inner, job);
}

fn finish_task_job(inner: &TaskRuntimeInner, state: &TaskState) {
    state.queue_consumed.store(true, Ordering::Release);
    // Call only after the claimant frame/queue node has actually been dropped.
    // This says nothing about retained result ownership or scope teardown.
    state.execution_queue_retired.store(true, Ordering::SeqCst);
    finish_task_accounting(inner, state);
}

fn finish_task_accounting(inner: &TaskRuntimeInner, state: &TaskState) {
    // Terminal publication can precede discarded payload cleanup. A cancelled
    // queue consumer and the completion thread may finish in either order;
    // neither can acknowledge work still owned by the other.
    if !state.execution_queue_retired.load(Ordering::SeqCst)
        || !state.completion_cleanup_done.load(Ordering::SeqCst)
        || !state.is_terminal()
        || state.accounting_finished.swap(true, Ordering::AcqRel)
    {
        return;
    }
    // Poison rejects new registration elsewhere, but existing retirement
    // must actually remove its Weak entry before publishing accounting_done.
    inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&state.id);
    inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
    inner.finished_tasks.fetch_add(1, Ordering::Relaxed);
    // accounting_finished above is a once-claim, not proof of these updates.
    state.accounting_done.store(true, Ordering::Release);
    state.ready.notify_all();
}

fn spawn_blocking_workers(
    inner: &Arc<TaskRuntimeInner>,
    count: usize,
    receiver: Receiver<BlockingJob>,
) {
    let receiver = Arc::new(Mutex::new(receiver));
    for index in 0..count {
        let weak = Arc::downgrade(inner);
        let receiver = Arc::clone(&receiver);
        if thread::Builder::new()
            .name(format!("ku-blocking-{index}"))
            .spawn(move || blocking_worker_loop(weak, receiver))
            .is_ok()
        {
            inner.blocking_workers.fetch_add(1, Ordering::Release);
        }
    }
}

fn blocking_worker_loop(weak: Weak<TaskRuntimeInner>, receiver: Arc<Mutex<Receiver<BlockingJob>>>) {
    loop {
        let Some(shutdown) = weak
            .upgrade()
            .map(|inner| inner.shutdown.load(Ordering::Acquire))
        else {
            return;
        };
        if shutdown {
            return;
        }
        let job = {
            let Ok(receiver) = receiver.lock() else {
                return;
            };
            receiver.recv()
        };
        match job {
            Ok(job) => {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let _running = RunningBlockingJob::claim(&inner);
                inner.queued_blocking_jobs.fetch_sub(1, Ordering::AcqRel);
                let cancelled = job
                    .cancelled
                    .as_ref()
                    .and_then(|state| state.cancellation());
                let result = if let Some(context) = cancelled {
                    let _cleanup = CleanupGuard::enter(context);
                    drop(job.run);
                    Err(KuError::termination(context, Span::default()))
                } else {
                    match catch_unwind(AssertUnwindSafe(job.run)).map_err(|_| {
                        KuError::structured(
                            crate::error::KuErrorKind::Runtime,
                            "task",
                            "blocking_panic",
                            "blocking pool job panicked",
                            Span::default(),
                        )
                    }) {
                        Ok(result) => result,
                        Err(error) => Err(error),
                    }
                };
                let result = if let Some(context) = job
                    .cancelled
                    .as_ref()
                    .and_then(|state| state.cancellation())
                {
                    let _cleanup = CleanupGuard::enter(context);
                    drop_owner_result_synchronously(result);
                    Err(KuError::termination(context, Span::default()))
                } else {
                    result
                };
                send_blocking_result(&job.response, job.cancelled.as_deref(), result);
            }
            Err(_) => return,
        }
    }
}

fn send_blocking_result(
    response: &SyncSender<KuResult<Value>>,
    parent: Option<&TaskState>,
    result: KuResult<Value>,
) {
    if let Err(unsent) = response.send(result) {
        // The parent can terminate after the worker's post-operation check and
        // drop its receiver before send. Re-read here so nested Task owners in
        // the undeliverable payload keep that first cause and absolute deadline.
        let _cleanup = parent
            .and_then(TaskState::cancellation)
            .map(CleanupGuard::enter);
        drop_owner_result_synchronously(unsent.0);
    }
}

fn runtime_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(4, 32)
}

pub fn blocking_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(4, 32)
}

pub fn current_task_id() -> i64 {
    CURRENT_TASK_ID.with(Cell::get)
}

pub fn current_task_cancelled() -> bool {
    CURRENT_TASK_STATE.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|state| state.is_cancelled())
    })
}

pub(crate) fn current_task_cancellation() -> Option<CancellationContext> {
    CURRENT_TASK_STATE.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .and_then(|state| state.cancellation())
    })
}

pub(crate) fn request_current_task_cancel_with(context: CancellationContext) -> bool {
    CURRENT_TASK_STATE.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|state| state.request_cancel_with(context))
    })
}

fn await_help_depth() -> usize {
    AWAIT_HELP_DEPTH.with(Cell::get)
}

fn task_error(code: &str, message: impl Into<String>) -> Value {
    errors::err("task", code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry_server::native_test_harness as owner_release_bounded_process;

    fn completion_state() -> TaskState {
        TaskState {
            id: 41,
            runtime: Weak::new(),
            queue_consumed: AtomicBool::new(false),
            execution_queue_retired: AtomicBool::new(false),
            completion_cleanup_done: AtomicBool::new(false),
            accounting_finished: AtomicBool::new(false),
            accounting_done: AtomicBool::new(false),
            result: Mutex::new(None),
            pending_run: Mutex::new(None),
            owner_released: AtomicBool::new(false),
            owner_call_done: AtomicBool::new(false),
            owner_payload_done: AtomicBool::new(false),
            owner_cleanup_done: AtomicBool::new(false),
            owner_result_release: Mutex::new(None),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cancellation: Mutex::new(None),
            awaited: AtomicBool::new(false),
            status: AtomicU8::new(TASK_RUNNING),
        }
    }

    fn spawn_task<F>(runtime: &TaskRuntime, run: F) -> TaskHandle
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        runtime.spawn(run).expect("ordinary test task admission")
    }

    fn assert_cancelled(error: KuError) {
        assert_eq!(
            error.runtime_termination().map(|context| context.reason),
            Some(TerminationReason::Cancelled),
            "{error:?}"
        );
    }

    #[test]
    fn task_submission_fast_path_stays_set_after_completed_owner_is_dropped() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        assert!(!runtime.has_task_submissions());
        // Admission rejection still creates a completed Task owning its error.
        let task = runtime.spawn(|| Ok(Value::Null)).unwrap();
        assert!(runtime.has_task_submissions());
        assert!(task.state.is_terminal());
        assert_eq!(runtime.inner.active_tasks.load(Ordering::Acquire), 0);
        drop(task);
        assert!(runtime.has_task_submissions());
    }

    // Track destruction using actual move-only payload ownership, without a
    // running job, an OS worker, a real socket, or a timing-dependent allocator.
    fn tracked_owned_result() -> (KuResult<Value>, Weak<TaskState>, Weak<TaskRuntimeInner>) {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let weak_state = Arc::downgrade(&state);
        let weak_runtime = Arc::downgrade(&runtime.inner);
        let result = Ok(Value::Result {
            ok: true,
            value: Box::new(Value::Array(vec![
                Value::String("owned completion".repeat(128)),
                Value::Task(TaskHandle {
                    id: 99,
                    owner: Arc::new(TaskOwnerLease {
                        state: Arc::downgrade(&state),
                    }),
                    state,
                    runtime,
                }),
            ])),
        });
        (result, weak_state, weak_runtime)
    }

    fn handle_for_state(runtime: &TaskRuntime, state: &Arc<TaskState>) -> TaskHandle {
        TaskHandle {
            id: state.id,
            state: Arc::clone(state),
            runtime: runtime.clone(),
            owner: Arc::new(TaskOwnerLease {
                state: Arc::downgrade(state),
            }),
        }
    }

    struct OrderedOwnerDrop {
        label: &'static str,
        events: Arc<Mutex<Vec<&'static str>>>,
        pending: Option<Weak<TaskState>>,
        listener_address: Option<std::net::SocketAddr>,
    }

    impl Drop for OrderedOwnerDrop {
        fn drop(&mut self) {
            if let Some(parent) = self.pending.as_ref().and_then(Weak::upgrade) {
                assert_eq!(parent.status_name(), "cancelling");
                assert!(!parent.accounting_finished.load(Ordering::Acquire));
                assert!(parent.result().unwrap().is_none());
                let runtime = parent.runtime.upgrade().unwrap();
                assert_eq!(runtime.active_tasks.load(Ordering::Acquire), 1);
                assert_eq!(runtime.finished_tasks.load(Ordering::Acquire), 0);
            }
            if let Some(address) = self.listener_address {
                assert!(
                    std::net::TcpListener::bind(address).is_err(),
                    "the following listener must remain open until this Task payload is dropped"
                );
            }
            self.events.lock().unwrap().push(self.label);
        }
    }

    fn completed_with_value(runtime: &TaskRuntime, value: Value) -> TaskHandle {
        let state = Arc::new(completion_state());
        state.complete(state.id, Ok(value));
        handle_for_state(runtime, &state)
    }

    fn single_task_leaf_wrapper(value: Value, kind: usize) -> Value {
        match kind % 5 {
            0 => Value::Result {
                ok: kind < 5,
                value: Box::new(value),
            },
            1 => Value::Array(vec![value]),
            2 => Value::Object(HashMap::from([("payload".into(), value)])),
            3 => Value::Struct {
                name: "Holder".into(),
                fields: HashMap::from([("payload".into(), value)]),
            },
            _ => Value::Enum {
                name: "Choice".into(),
                variant: "Only".into(),
                fields: vec![value],
            },
        }
    }

    fn single_task_leaf_wrappers(mut value: Value, depth: usize) -> Value {
        for kind in 0..depth {
            value = single_task_leaf_wrapper(value, kind);
        }
        value
    }

    #[test]
    fn owner_single_task_leaf_classifier_is_readonly_and_rejects_other_shapes() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let new_task = || Value::Task(handle_for_state(&runtime, &Arc::new(completion_state())));
        for kind in 0..10 {
            let leaf = Arc::new(completion_state());
            let value =
                single_task_leaf_wrapper(Value::Task(handle_for_state(&runtime, &leaf)), kind);
            let strong = Arc::strong_count(&leaf);
            assert!(has_single_task_leaf(&value));
            assert_eq!(Arc::strong_count(&leaf), strong);
            assert!(!leaf.owner_released.load(Ordering::Acquire));
            drop_owner_result_synchronously(Ok(value));
            assert!(leaf.owner_released.load(Ordering::Acquire));
        }
        let nested = single_task_leaf_wrappers(new_task(), 31);
        assert!(has_single_task_leaf(&nested));
        drop_owner_result_synchronously(Ok(nested));
        let mut sparse = HashMap::with_capacity(4_096);
        sparse.insert("only".into(), new_task());
        let sparse = Value::Object(sparse);
        assert!(has_single_task_leaf(&sparse));
        drop_owner_result_synchronously(Ok(sparse));

        let mut captures = crate::env::Env::new();
        captures
            .define_owned("captured".into(), new_task(), false, Span::default())
            .unwrap();
        let function = Value::Function {
            params: Vec::new(),
            param_modes: Vec::new(),
            body: Vec::new(),
            captures,
            self_name: None,
            is_async: false,
        };
        let borrowed_root = Arc::new(new_task());
        let borrowed = Value::Borrowed(crate::value::BorrowedValue::new(&borrowed_root));
        for value in [
            Value::Null,
            Value::Int(1),
            Value::Result {
                ok: false,
                value: Box::new(Value::String("plain error".into())),
            },
            Value::Array(Vec::new()),
            Value::Array(vec![new_task(), Value::String("sibling".into())]),
            Value::Object(HashMap::new()),
            Value::Object(HashMap::from([
                ("first".into(), new_task()),
                ("second".into(), new_task()),
            ])),
            Value::Struct {
                name: "Empty".into(),
                fields: HashMap::new(),
            },
            Value::Struct {
                name: "Pair".into(),
                fields: HashMap::from([
                    ("first".into(), new_task()),
                    ("second".into(), Value::Int(0)),
                ]),
            },
            Value::Enum {
                name: "Choice".into(),
                variant: "None".into(),
                fields: Vec::new(),
            },
            Value::Enum {
                name: "Choice".into(),
                variant: "Pair".into(),
                fields: vec![new_task(), new_task()],
            },
            single_task_leaf_wrappers(function, 7),
            single_task_leaf_wrappers(borrowed, 7),
            Value::HttpListenerLease(crate::runtime::value::HttpListenerLease::new(-1)),
        ] {
            assert!(!has_single_task_leaf(&value));
            drop_owner_result_synchronously(Ok(value));
        }
        drop(borrowed_root);
        assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
        assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
    }

    fn ordered_completed_owner(
        runtime: &TaskRuntime,
        label: &'static str,
        events: &Arc<Mutex<Vec<&'static str>>>,
        pending: Option<Weak<TaskState>>,
        listener_address: Option<std::net::SocketAddr>,
    ) -> TaskHandle {
        let leaf = Arc::new(completion_state());
        let probe = OrderedOwnerDrop {
            label,
            events: Arc::clone(events),
            pending,
            listener_address,
        };
        leaf.install_run(Box::new(move || {
            drop(probe);
            Ok(Value::Null)
        }));
        completed_with_value(runtime, Value::Task(handle_for_state(runtime, &leaf)))
    }

    fn ordered_completed_fork(
        runtime: &TaskRuntime,
        events: &Arc<Mutex<Vec<&'static str>>>,
        pending: Option<Weak<TaskState>>,
    ) -> Value {
        let first = ordered_completed_owner(runtime, "A1", events, pending.clone(), None);
        let second = ordered_completed_owner(runtime, "A2", events, pending.clone(), None);
        let a = completed_with_value(
            runtime,
            Value::Array(vec![Value::Task(first), Value::Task(second)]),
        );
        let b = ordered_completed_owner(runtime, "B1", events, pending, None);
        Value::Array(vec![Value::Task(a), Value::Task(b)])
    }

    #[test]
    fn owner_result_fork_preserves_nested_vec_depth_first_drop_order() {
        for wrapped in [false, true] {
            for explicit in [false, true] {
                let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
                let events = Arc::new(Mutex::new(Vec::new()));
                let payload = ordered_completed_fork(&runtime, &events, None);
                let payload = if wrapped {
                    Value::Result {
                        ok: true,
                        value: Box::new(payload),
                    }
                } else {
                    payload
                };
                let owner = completed_with_value(&runtime, payload);
                let context = CancellationContext::new(TerminationReason::Cancelled);
                let _cleanup = CleanupGuard::enter(context);
                if explicit {
                    owner.release_scope_owner(context);
                }
                drop(owner);
                assert_eq!(*events.lock().unwrap(), ["A1", "A2", "B1"]);
                assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
                assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
            }
        }
    }

    #[test]
    fn owner_result_fork_queued_capture_finishes_before_parent_ack() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
        let mut initial = completion_state();
        initial.runtime = Arc::downgrade(&runtime.inner);
        initial.queue_consumed.store(true, Ordering::Release);
        // This kernel fixture represents an already-retired claimant node.
        initial
            .execution_queue_retired
            .store(true, Ordering::SeqCst);
        let pending = Arc::new(initial);
        runtime.inner.active_tasks.store(1, Ordering::Release);
        runtime
            .inner
            .states
            .lock()
            .unwrap()
            .insert(pending.id, Arc::downgrade(&pending));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captures = ordered_completed_fork(&runtime, &events, Some(Arc::downgrade(&pending)));
        let ran = Arc::new(AtomicBool::new(false));
        let child_ran = Arc::clone(&ran);
        pending.install_run(Box::new(move || {
            child_ran.store(true, Ordering::Release);
            drop(captures);
            Ok(Value::Null)
        }));
        let outer =
            completed_with_value(&runtime, Value::Task(handle_for_state(&runtime, &pending)));
        let context = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now(),
        };
        let _cleanup = CleanupGuard::enter(context);
        drop(outer);
        assert_eq!(*events.lock().unwrap(), ["A1", "A2", "B1"]);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(pending.cancellation(), Some(context));
        assert_eq!(pending.status_name(), "timed_out");
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.finished_tasks, 1);
        assert_eq!(snapshot.registered_tasks, 0);
    }

    #[test]
    fn owner_result_fork_preserves_task_before_listener_release() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let listener_id = crate::runtime::http_listener_registry::insert(listener).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let task = ordered_completed_owner(&runtime, "task", &events, None, Some(address));
        let owner = completed_with_value(
            &runtime,
            Value::Array(vec![
                Value::Task(task),
                Value::HttpListenerLease(crate::runtime::value::HttpListenerLease::new(
                    listener_id,
                )),
            ]),
        );
        drop(owner);
        assert_eq!(*events.lock().unwrap(), ["task"]);
        std::net::TcpListener::bind(address).expect("listener closes after the preceding Task");
        assert!(
            crate::runtime::http_listener_registry::take(listener_id, Span::default()).is_err()
        );
    }

    #[test]
    fn owner_result_fork_panic_restores_mode_and_allows_next_independent_release() {
        struct PanickingCapture {
            events: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Drop for PanickingCapture {
            fn drop(&mut self) {
                assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
                self.events.lock().unwrap().push("A");
                panic!("intentional queued capture destructor panic");
            }
        }

        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let events = Arc::new(Mutex::new(Vec::new()));
        let leaf = Arc::new(completion_state());
        let capture = PanickingCapture {
            events: Arc::clone(&events),
        };
        leaf.install_run(Box::new(move || {
            drop(capture);
            Ok(Value::Null)
        }));
        let a = completed_with_value(&runtime, Value::Task(handle_for_state(&runtime, &leaf)));
        let b = ordered_completed_owner(&runtime, "B", &events, None, None);
        let outer =
            completed_with_value(&runtime, Value::Array(vec![Value::Task(a), Value::Task(b)]));
        let context = CancellationContext::new(TerminationReason::Cancelled);
        let _cleanup = CleanupGuard::enter(context);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(outer))).is_err());
        assert_eq!(*events.lock().unwrap(), ["A", "B"]);
        assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
        assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
        drop(ordered_completed_owner(&runtime, "C", &events, None, None));
        assert_eq!(*events.lock().unwrap(), ["A", "B", "C"]);
        assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
        assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
    }

    #[test]
    fn completed_owner_result_chain_drains_on_real_worker_without_recursive_drop() {
        const CHILD: &str = "KU_TASK_OWNER_RESULT_CHAIN_CHILD";
        const COMPLETED: &str = "completed owner result chain regression completed";
        if std::env::var_os(CHILD).is_none() {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "runtime::task::tests::completed_owner_result_chain_drains_on_real_worker_without_recursive_drop",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            let output = owner_release_bounded_process::run_bounded(
                &mut command,
                Duration::from_secs(20),
                owner_release_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
            )
            .expect("completed owner cleanup has a bounded subprocess");
            assert!(
                output.status.success(),
                "completed owner cleanup subprocess failed: {:?}\n{}{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .any(|line| line == COMPLETED),
                "completed owner cleanup subprocess did not finish its regression\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        struct CaptureAckProbe {
            pending: Arc<Mutex<Option<Weak<TaskState>>>>,
            observed: Arc<AtomicBool>,
        }
        impl Drop for CaptureAckProbe {
            fn drop(&mut self) {
                let pending = self
                    .pending
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .unwrap();
                assert_eq!(pending.status_name(), "cancelling");
                assert!(!pending.accounting_finished.load(Ordering::Acquire));
                assert!(pending.result().unwrap().is_none());
                self.observed.store(true, Ordering::Release);
            }
        }

        // Both direct and wrapped variants leave two cancelled queue nodes
        // each for the sole worker to retire after this parent returns.
        let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 5);
        let child_runtime = runtime.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let job = spawn_task(&runtime, move || {
            for wrapped in [false, true] {
                for release_path in 0..4 {
                    let inert = TaskRuntime::with_limits(0, 0, 0, 0, 0);
                    let leaf = Arc::new(completion_state());
                    let capture_pending = Arc::new(Mutex::new(None));
                    let capture_observed = Arc::new(AtomicBool::new(false));
                    if release_path == 3 {
                        let probe = CaptureAckProbe {
                            pending: Arc::clone(&capture_pending),
                            observed: Arc::clone(&capture_observed),
                        };
                        leaf.install_run(Box::new(move || {
                            drop(probe);
                            Ok(Value::Null)
                        }));
                    }
                    let mut owner = handle_for_state(&inert, &leaf);
                    let mut weak_states = Vec::with_capacity(16_384);
                    for _ in 0..16_384 {
                        let state = Arc::new(completion_state());
                        weak_states.push(Arc::downgrade(&state));
                        let payload = if wrapped {
                            single_task_leaf_wrapper(Value::Task(owner), weak_states.len())
                        } else {
                            Value::Task(owner)
                        };
                        state.complete(state.id, Ok(payload));
                        owner = handle_for_state(&inert, &state);
                    }
                    let context = CancellationContext {
                        reason: TerminationReason::TimedOut,
                        cleanup_deadline: Instant::now(),
                    };
                    if release_path >= 2 {
                        // The only worker is this parent. Cancellation must drop
                        // the queued capture before publishing its terminal ACK,
                        // without running the child's continuation.
                        let ran = Arc::new(AtomicBool::new(false));
                        let child_ran = Arc::clone(&ran);
                        let child = spawn_task(&child_runtime, move || {
                            child_ran.store(true, Ordering::Release);
                            Ok(Value::Task(owner))
                        });
                        let _cleanup = CleanupGuard::enter(context);
                        let child_state = Arc::clone(&child.state);
                        if release_path == 3 {
                            *capture_pending.lock().unwrap() = Some(Arc::downgrade(&child_state));
                            let outer = Arc::new(completion_state());
                            let payload = if wrapped {
                                single_task_leaf_wrappers(Value::Task(child), 5)
                            } else {
                                Value::Task(child)
                            };
                            outer.complete(outer.id, Ok(payload));
                            drop(handle_for_state(&inert, &outer));
                            assert!(capture_observed.load(Ordering::Acquire));
                            assert!(outer.result().unwrap().is_none());
                        } else {
                            child.release_scope_owner(context);
                            drop(child);
                        }
                        assert_eq!(child_state.status_name(), "timed_out");
                        assert!(!ran.load(Ordering::Acquire));
                        assert!(weak_states.iter().all(|state| state.upgrade().is_none()));
                    } else if release_path == 1 {
                        let _cleanup = CleanupGuard::enter(context);
                        let observer = owner.clone();
                        owner.release_scope_owner(context);
                        assert_eq!(observer.status(), "completed");
                        assert!(observer.state.result().unwrap().is_none());
                        drop(observer);
                        drop(owner);
                    } else {
                        let _cleanup = CleanupGuard::enter(context);
                        drop(owner);
                    }
                    assert!(weak_states.iter().all(|state| state.upgrade().is_none()));
                    assert!(leaf.owner_released.load(Ordering::Acquire));
                    assert_eq!(leaf.cancellation(), Some(context));
                    assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
                    assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
                    assert_eq!(inert.snapshot().unwrap().active_tasks, 0);
                }
            }
            done_tx.send(current_task_id()).unwrap();
            Ok(Value::Null)
        });
        // No await/help runs on the parent while the deep cleanup executes.
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            job.id()
        );
        assert_eq!(job.await_result().unwrap(), Value::Null);
        runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.finished_tasks, 5);
        assert_eq!(snapshot.wait_edges, 0);
        eprintln!("{COMPLETED}");
    }

    #[test]
    fn concurrent_scope_and_final_lease_release_detach_completed_result_once() {
        for _ in 0..32 {
            let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
            let state = Arc::new(completion_state());
            let owner = handle_for_state(&runtime, &state);
            let release_state = Arc::clone(&state);
            let (payload, payload_state, payload_runtime) = tracked_owned_result();
            state.complete(state.id, payload);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let release_barrier = Arc::clone(&barrier);
            let context = CancellationContext::new(TerminationReason::Cancelled);
            let release = thread::spawn(move || {
                release_barrier.wait();
                release_state.release_owner(context);
            });
            barrier.wait();
            drop(owner);
            release.join().unwrap();
            assert!(payload_state.upgrade().is_none());
            assert!(payload_runtime.upgrade().is_none());
            assert!(state.result().unwrap().is_none());
            assert!(state.owner_result_release.lock().unwrap().is_none());
            assert_eq!(state.status_name(), "completed");
            assert!(!state.request_cancel());
        }
    }

    #[test]
    fn deep_single_task_leaf_wrappers_and_losing_results_drop_on_real_worker() {
        const CHILD: &str = "KU_TASK_SINGLE_LEAF_WRAPPER_CHILD";
        const COMPLETED: &str = "single Task leaf wrapper regression completed";
        if std::env::var_os(CHILD).is_none() {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "runtime::task::tests::deep_single_task_leaf_wrappers_and_losing_results_drop_on_real_worker",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            let output = owner_release_bounded_process::run_bounded(
                &mut command,
                Duration::from_secs(20),
                owner_release_bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
            )
            .expect("single leaf wrapper cleanup has a bounded subprocess");
            assert!(
                output.status.success(),
                "single leaf wrapper subprocess failed: {:?}\n{}{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .any(|line| line == COMPLETED),
                "single leaf wrapper regression never reached its completion marker"
            );
            return;
        }

        struct DiscardProbe {
            observed: Arc<Mutex<Vec<Option<CancellationContext>>>>,
            parent: Option<Weak<TaskState>>,
        }
        impl Drop for DiscardProbe {
            fn drop(&mut self) {
                if let Some(parent) = self.parent.as_ref().and_then(Weak::upgrade) {
                    assert!(!parent.accounting_finished.load(Ordering::Acquire));
                    let runtime = parent.runtime.upgrade().unwrap();
                    assert_eq!(runtime.active_tasks.load(Ordering::Acquire), 1);
                    assert_eq!(runtime.finished_tasks.load(Ordering::Acquire), 0);
                }
                self.observed
                    .lock()
                    .unwrap()
                    .push(current_cleanup_context());
            }
        }

        let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let job = spawn_task(&runtime, move || {
            assert!(thread::current().name().unwrap().starts_with("ku-task-"));
            for discard_path in 0..7 {
                let inert = TaskRuntime::with_limits(0, 0, 0, 0, 0);
                let tracks_accounting = matches!(discard_path, 2 | 4 | 6);
                let mut parent = completion_state();
                if tracks_accounting {
                    parent.runtime = Arc::downgrade(&inert.inner);
                    parent.queue_consumed.store(true, Ordering::Release);
                    // The fixture has no live runner or physical queue node.
                    parent.execution_queue_retired.store(true, Ordering::SeqCst);
                    inert.inner.active_tasks.store(1, Ordering::Release);
                }
                let parent = Arc::new(parent);
                if tracks_accounting {
                    inert
                        .inner
                        .states
                        .lock()
                        .unwrap()
                        .insert(parent.id, Arc::downgrade(&parent));
                }
                let leaf = Arc::new(completion_state());
                let weak_leaf = Arc::downgrade(&leaf);
                let observed = Arc::new(Mutex::new(Vec::with_capacity(1)));
                let probe = DiscardProbe {
                    observed: Arc::clone(&observed),
                    parent: tracks_accounting.then(|| Arc::downgrade(&parent)),
                };
                let ran = Arc::new(AtomicBool::new(false));
                let child_ran = Arc::clone(&ran);
                leaf.install_run(Box::new(move || {
                    child_ran.store(true, Ordering::Release);
                    drop(probe);
                    Ok(Value::Null)
                }));
                let payload =
                    single_task_leaf_wrappers(Value::Task(handle_for_state(&inert, &leaf)), 16_384);
                let context = CancellationContext {
                    reason: TerminationReason::TimedOut,
                    cleanup_deadline: Instant::now(),
                };
                let _cleanup = CleanupGuard::enter(context);
                match discard_path {
                    0 => {
                        parent.complete(parent.id, Ok(payload));
                        drop(handle_for_state(&inert, &parent));
                        assert_eq!(parent.status_name(), "completed");
                        assert!(parent.result().unwrap().is_none());
                    }
                    1 => {
                        parent.complete(parent.id, Ok(payload));
                        let owner = handle_for_state(&inert, &parent);
                        let observer = owner.clone();
                        owner.release_scope_owner(context);
                        drop(owner);
                        assert_eq!(observer.status(), "completed");
                        assert!(parent.result().unwrap().is_none());
                        drop(observer);
                    }
                    2 => {
                        assert!(parent.request_cancel_with(context));
                        parent.complete(parent.id, Ok(payload));
                        assert_eq!(parent.status_name(), "timed_out");
                        assert_eq!(parent.cancellation(), Some(context));
                    }
                    3 => {
                        parent.complete(parent.id, Ok(Value::Int(7)));
                        parent.complete_with_status(parent.id, Ok(payload), Some(TASK_PANICKED));
                        assert_eq!(parent.status_name(), "completed");
                        assert!(matches!(parent.result().unwrap(), Some(Ok(Value::Int(7)))));
                    }
                    4 => {
                        let owner = handle_for_state(&inert, &parent);
                        let observer = owner.clone();
                        owner.release_scope_owner(context);
                        parent.complete(parent.id, Ok(payload));
                        assert_eq!(observer.status(), "timed_out");
                        assert_eq!(parent.cancellation(), Some(context));
                        assert!(parent.result().unwrap().is_none());
                        drop(owner);
                        drop(observer);
                    }
                    5 => {
                        assert!(parent.request_cancel_with(context));
                        let (send, receive) = mpsc::sync_channel(1);
                        drop(receive);
                        send_blocking_result(&send, Some(&parent), Ok(payload));
                        assert_eq!(parent.cancellation(), Some(context));
                    }
                    _ => {
                        // Reproduce release_owner's unlocked interval between
                        // marking the owner released and requesting cancellation:
                        // completion may win, but must discard the owned result.
                        parent.owner_released.store(true, Ordering::Release);
                        parent.complete(parent.id, Ok(payload));
                        assert_eq!(parent.status_name(), "completed");
                        assert!(!parent.request_cancel_with(context));
                        assert!(parent.result().unwrap().is_none());
                    }
                }
                assert_eq!(*observed.lock().unwrap(), [Some(context)]);
                assert!(!ran.load(Ordering::Acquire));
                assert!(leaf.owner_released.load(Ordering::Acquire));
                assert_eq!(leaf.cancellation(), Some(context));
                assert_eq!(leaf.status_name(), "timed_out");
                assert!(parent.result.try_lock().is_ok());
                if tracks_accounting {
                    assert!(parent.accounting_finished.load(Ordering::Acquire));
                    let snapshot = inert.snapshot().unwrap();
                    assert_eq!(snapshot.active_tasks, 0);
                    assert_eq!(snapshot.finished_tasks, 1);
                    assert_eq!(snapshot.registered_tasks, 0);
                }
                drop(leaf);
                assert!(weak_leaf.upgrade().is_none());
                assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
                assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
            }
            done_tx.send(current_task_id()).unwrap();
            Ok(Value::Null)
        });
        // The parent cannot execute cleanup through await/help before success.
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            job.id()
        );
        assert_eq!(job.await_result().unwrap(), Value::Null);
        runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
        assert_eq!(runtime.snapshot().unwrap().active_tasks, 0);
        eprintln!("{COMPLETED}");
    }

    #[test]
    fn concurrent_single_task_leaf_scope_and_final_lease_release_detach_once() {
        for _ in 0..32 {
            let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
            let state = Arc::new(completion_state());
            let owner = handle_for_state(&runtime, &state);
            let events = Arc::new(Mutex::new(Vec::with_capacity(1)));
            let leaf = ordered_completed_owner(&runtime, "leaf", &events, None, None);
            state.complete(
                state.id,
                Ok(single_task_leaf_wrappers(Value::Task(leaf), 64)),
            );
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let release_barrier = Arc::clone(&barrier);
            let release_state = Arc::clone(&state);
            let context = CancellationContext {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: Instant::now(),
            };
            let release = thread::spawn(move || {
                release_barrier.wait();
                release_state.release_owner(context);
                assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
                assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
            });
            let _cleanup = CleanupGuard::enter(context);
            barrier.wait();
            drop(owner);
            release.join().unwrap();
            assert_eq!(*events.lock().unwrap(), ["leaf"]);
            assert!(state.result().unwrap().is_none());
            assert!(state.owner_result_release.lock().unwrap().is_none());
            assert_eq!(state.status_name(), "completed");
            assert!(!state.request_cancel());
            assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
            assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
        }
    }

    #[test]
    fn linear_discard_panic_restores_outer_drain_and_next_owner_cleanup() {
        struct PanickingProbe {
            events: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Drop for PanickingProbe {
            fn drop(&mut self) {
                assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
                self.events.lock().unwrap().push("panic");
                panic!("intentional single-leaf discard destructor panic");
            }
        }
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let events = Arc::new(Mutex::new(Vec::with_capacity(2)));
        let leaf = Arc::new(completion_state());
        let probe = PanickingProbe {
            events: Arc::clone(&events),
        };
        leaf.install_run(Box::new(move || {
            drop(probe);
            Ok(Value::Null)
        }));
        let payload = single_task_leaf_wrappers(Value::Task(handle_for_state(&runtime, &leaf)), 31);
        let previous = OWNER_RESULT_RELEASE_ACTIVE.with(|active| active.replace(true));
        let panic = catch_unwind(AssertUnwindSafe(|| {
            drop_owner_result_synchronously(Ok(payload));
        }));
        let restored_outer = OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get);
        OWNER_RESULT_RELEASE_ACTIVE.with(|active| active.set(previous));
        assert!(panic.is_err());
        assert!(restored_outer);
        assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
        drop(ordered_completed_owner(
            &runtime, "next", &events, None, None,
        ));
        assert_eq!(*events.lock().unwrap(), ["panic", "next"]);
        assert!(OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none()));
        assert!(!OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get));
    }

    #[test]
    fn last_owner_releases_completed_payload_while_scheduler_state_survives() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        let transferred = owner.clone();
        let (payload, payload_state, payload_runtime) = tracked_owned_result();
        state.complete(state.id, payload);
        drop(owner);
        assert!(payload_state.upgrade().is_some());
        assert!(!state.owner_released.load(Ordering::Acquire));
        drop(transferred);
        assert!(payload_state.upgrade().is_none());
        assert!(payload_runtime.upgrade().is_none());
        assert!(state.result().unwrap().is_none());
        assert_eq!(state.status_name(), "completed");
        assert!(!state.request_cancel());
    }

    #[test]
    fn explicit_scope_release_drops_success_and_failure_payloads_with_observer_alive() {
        for ok in [true, false] {
            let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
            let state = Arc::new(completion_state());
            let owner = handle_for_state(&runtime, &state);
            let observer = owner.clone();
            let (payload, payload_state, payload_runtime) = tracked_owned_result();
            let mut payload = payload.unwrap();
            let Value::Result { ok: result_ok, .. } = &mut payload else {
                unreachable!();
            };
            *result_ok = ok;
            state.complete(state.id, Ok(payload));
            let context = CancellationContext::new(TerminationReason::Cancelled);
            owner.release_scope_owner(context);
            assert!(payload_state.upgrade().is_none());
            assert!(payload_runtime.upgrade().is_none());
            assert!(state.result().unwrap().is_none());
            assert_eq!(observer.id(), state.id);
            assert_eq!(observer.status(), "completed");

            // Only a non-owning internal alias remains after scope exit. It
            // cannot recover the released payload through ordinary await.
            let Value::Result { ok: false, value } = observer.await_result().unwrap() else {
                panic!("released scope owner cannot be awaited through an observer");
            };
            let Value::Object(fields) = *value else {
                panic!("expected already_awaited task error");
            };
            assert_eq!(
                fields.get("code"),
                Some(&Value::String("already_awaited".into()))
            );
            owner.release_scope_owner(context);
            drop(owner);
            drop(observer);
            assert!(state.result().unwrap().is_none());
            assert_eq!(state.status_name(), "completed");
        }
    }

    #[test]
    fn explicit_scope_release_discards_late_completion_with_observer_alive() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        let observer = owner.clone();
        let context = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now(),
        };
        owner.release_scope_owner(context);
        assert_eq!(state.cancellation(), Some(context));
        for _ in 0..2 {
            let (payload, payload_state, payload_runtime) = tracked_owned_result();
            state.complete(state.id, payload);
            assert!(payload_state.upgrade().is_none());
            assert!(payload_runtime.upgrade().is_none());
            assert!(state.result().unwrap().is_none());
            assert_eq!(state.status_name(), "timed_out");
        }
        observer.release_scope_owner(context);
        drop(owner);
        assert_eq!(observer.id(), state.id);
        drop(observer);
        assert_eq!(state.cancellation(), Some(context));
        assert!(state.result().unwrap().is_none());
    }

    #[test]
    fn ordinary_await_moves_payload_and_taken_terminal_cannot_be_cancelled() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        let (payload, payload_state, payload_runtime) = tracked_owned_result();
        state.complete(state.id, payload);
        let taken = owner.await_result().expect("completed owned payload");
        assert!(state.result().unwrap().is_none());
        assert!(!state.request_cancel());
        assert_eq!(state.status_name(), "completed");
        drop(owner);
        assert!(payload_state.upgrade().is_some());
        drop(taken);
        assert!(payload_state.upgrade().is_none());
        assert!(payload_runtime.upgrade().is_none());
    }

    #[test]
    fn await_pending_observation_then_completion_delivers_owned_payload_once() {
        let timeout = Duration::from_secs(2);
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        let payload = "owned result".repeat(1024);
        let allocation = payload.as_ptr() as usize;
        let (complete_tx, complete_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker_state = Arc::clone(&state);
        let worker = thread::spawn(move || {
            complete_rx.recv_timeout(timeout).unwrap();
            worker_state.complete(worker_state.id, Ok(Value::String(payload)));
            done_tx.send(()).unwrap();
        });
        // This is the first await's claim and its first pending observation.
        // Force completion before the awaiting side examines that observation.
        assert!(state.claim_await());
        let pending = state.observe_result(true).unwrap();
        complete_tx.send(()).unwrap();
        done_rx.recv_timeout(timeout).unwrap();
        worker.join().unwrap();
        assert!(pending.0.is_none());
        assert!(
            !pending.1,
            "completion cannot rewrite the earlier pending observation"
        );
        assert!(state.is_terminal());
        let delivered = runtime.await_task_until(&owner, None, true).unwrap();
        let Value::String(delivered) = delivered else {
            panic!("the first await must deliver the actual owned result");
        };
        assert_eq!(
            delivered.as_ptr() as usize,
            allocation,
            "await must move, not clone"
        );
        assert!(state.result().unwrap().is_none());
        let Value::Result { ok: false, value } = owner.await_result().unwrap() else {
            panic!("only the second await is already_awaited");
        };
        let Value::Object(fields) = *value else {
            panic!("expected structured task error");
        };
        assert_eq!(
            fields.get("code"),
            Some(&Value::String("already_awaited".into()))
        );
    }

    #[test]
    fn diagnostic_peek_after_pending_observation_does_not_consume_completion() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        let pending = state.observe_result(false).unwrap();
        let payload = "peek remains nonconsuming".repeat(128);
        let allocation = payload.as_ptr();
        state.complete(state.id, Ok(Value::String(payload)));
        assert!(pending.0.is_none());
        assert!(!pending.1);
        let Value::String(peeked) = owner.await_timeout(Duration::ZERO).unwrap() else {
            panic!("diagnostic peek must see the completed payload");
        };
        assert_ne!(
            peeked.as_ptr(),
            allocation,
            "peek retains its internal clone contract"
        );
        assert!(!state.awaited.load(Ordering::Acquire));
        let Value::String(delivered) = owner.await_result().unwrap() else {
            panic!("ordinary await must still own the original payload");
        };
        assert_eq!(delivered.as_ptr(), allocation);
        assert!(state.result().unwrap().is_none());
    }

    #[test]
    fn cancellation_reason_is_first_winner_and_deadline_only_shortens() {
        let state = completion_state();
        let now = Instant::now();
        let first = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now + Duration::from_secs(1),
        };
        assert!(state.request_cancel_with(first));
        assert!(!state.request_cancel_with(CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now,
        }));
        assert!(!state.request_cancel_with(CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now + Duration::from_secs(5),
        }));
        state.complete(state.id, Ok(Value::String("late".into())));
        assert_eq!(state.status_name(), "timed_out");
        assert_eq!(
            state
                .result()
                .unwrap()
                .unwrap()
                .unwrap_err()
                .runtime_termination(),
            Some(CancellationContext {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: now,
            })
        );
    }

    #[test]
    fn nested_cleanup_preserves_deadline_and_rejects_new_task_operations() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        let now = Instant::now();
        let outer = CleanupGuard::enter(CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now + Duration::from_secs(1),
        });
        {
            let _inner = CleanupGuard::enter(CancellationContext {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now,
            });
            assert_eq!(current_cleanup_context().unwrap().cleanup_deadline, now);
        }
        let effective = current_cleanup_context().unwrap();
        assert_eq!(effective.reason, TerminationReason::TimedOut);
        assert_eq!(effective.cleanup_deadline, now);
        assert_eq!(
            CancellationContext::new(TerminationReason::Cancelled),
            effective
        );
        let built = AtomicBool::new(false);
        let error = runtime
            .spawn_deferred(|| {
                built.store(true, Ordering::Release);
                || Ok(Value::Null)
            })
            .expect_err("cleanup cannot construct a task");
        assert_eq!(error.runtime_termination(), Some(effective));
        assert!(!built.load(Ordering::Acquire));
        assert_eq!(
            owner.await_result().unwrap_err().runtime_termination(),
            Some(effective)
        );
        assert_eq!(
            runtime
                .run_blocking(
                    || panic!("cleanup submitted blocking work"),
                    Span::default()
                )
                .unwrap_err()
                .runtime_termination(),
            Some(effective)
        );
        assert_eq!(runtime.snapshot().unwrap().total_submissions, 0);
        assert!(!state.awaited.load(Ordering::Acquire));
        drop(outer);
        assert!(current_cleanup_context().is_none());
    }

    #[test]
    fn expired_cleanup_broadcasts_all_children_before_counting_one_timeout() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let first = Arc::new(completion_state());
        let second = Arc::new(completion_state());
        let handles = [
            handle_for_state(&runtime, &first),
            handle_for_state(&runtime, &second),
        ];
        let context = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now(),
        };
        let error = runtime
            .cancel_handles_and_wait(&handles, context)
            .unwrap_err();
        assert_eq!(error.runtime_termination(), Some(context));
        assert_eq!(first.cancellation(), Some(context));
        assert_eq!(second.cancellation(), Some(context));
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.cleanup_timeouts, 1);
        assert_eq!(snapshot.cleanup_unfinished_tasks, 2);
    }

    #[test]
    fn single_worker_parent_cancels_queued_child_without_executing_child() {
        let timeout = Duration::from_secs(2);
        let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
        let child_runtime = runtime.clone();
        let (cleaned_tx, cleaned_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let executed = Arc::new(AtomicBool::new(false));
        let child_executed = Arc::clone(&executed);
        let (payload, payload_state, payload_runtime) = tracked_owned_result();
        let parent = spawn_task(&runtime, move || {
            let child = child_runtime.spawn(move || {
                child_executed.store(true, Ordering::Release);
                payload
            })?;
            let context = CancellationContext::new(TerminationReason::Cancelled);
            child_runtime.cancel_handles_and_wait(std::slice::from_ref(&child), context)?;
            assert_eq!(child.status(), "cancelled");
            cleaned_tx.send(child_runtime.snapshot()?).unwrap();
            release_rx.recv_timeout(timeout).expect("release parent");
            Ok(Value::Null)
        });
        let snapshot = cleaned_rx
            .recv_timeout(timeout)
            .expect("queued child cleanup");
        assert!(!executed.load(Ordering::Acquire));
        assert!(payload_state.upgrade().is_none());
        assert!(payload_runtime.upgrade().is_none());
        assert_eq!(snapshot.active_tasks, 1);
        assert_eq!(snapshot.queued_tasks, 0);
        assert_eq!(snapshot.finished_tasks, 1);
        assert_eq!(snapshot.cleanup_timeouts, 0);
        release_tx.send(()).unwrap();
        assert_eq!(parent.await_result().unwrap(), Value::Null);
        runtime
            .cancel_all_and_wait(timeout)
            .expect("drain queue tombstone");
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.accepted_submissions, 2);
        assert_eq!(snapshot.finished_tasks, 2);
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.queued_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
    }

    #[test]
    fn dropping_queued_owner_releases_frame_before_worker_is_available() {
        let timeout = Duration::from_secs(2);
        let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let blocker = spawn_task(&runtime, move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(timeout).expect("release blocker");
            Ok(Value::Null)
        });
        started_rx.recv_timeout(timeout).expect("worker blocked");
        let (payload, payload_state, payload_runtime) = tracked_owned_result();
        let child = spawn_task(&runtime, move || payload);
        let scheduler_observer = Arc::clone(&child.state);
        drop(child);
        assert!(payload_state.upgrade().is_none());
        assert!(payload_runtime.upgrade().is_none());
        assert_eq!(scheduler_observer.status_name(), "cancelled");
        assert!(scheduler_observer.result().unwrap().is_none());
        assert_eq!(runtime.snapshot().unwrap().finished_tasks, 0);
        release_tx.send(()).unwrap();
        blocker.await_result().unwrap();
        runtime.cancel_all_and_wait(timeout).unwrap();
        assert_eq!(runtime.snapshot().unwrap().finished_tasks, 2);
    }

    #[test]
    fn queued_claim_cancel_race_drops_capture_once() {
        struct DropProbe {
            tx: mpsc::SyncSender<()>,
        }
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.tx.send(()).unwrap();
            }
        }
        let timeout = Duration::from_secs(2);
        for _ in 0..32 {
            let state = Arc::new(completion_state());
            state.status.store(TASK_PENDING, Ordering::Release);
            let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
            let probe = DropProbe { tx: dropped_tx };
            state.install_run(Box::new(move || {
                drop(probe);
                Ok(Value::Int(7))
            }));
            let (worker_start_tx, worker_start_rx) = mpsc::sync_channel(1);
            let (cancel_start_tx, cancel_start_rx) = mpsc::sync_channel(1);
            let (worker_done_tx, worker_done_rx) = mpsc::sync_channel(1);
            let (cancel_done_tx, cancel_done_rx) = mpsc::sync_channel(1);
            let worker_state = Arc::clone(&state);
            let worker = thread::spawn(move || {
                worker_start_rx.recv_timeout(timeout).unwrap();
                if let Some(run) = worker_state.claim_run() {
                    let TaskStart::Callback(run) = run else {
                        panic!("this callback race fixture must claim its callback start");
                    };
                    worker_state.complete(worker_state.id, run());
                }
                worker_done_tx.send(()).unwrap();
            });
            let cancel_state = Arc::clone(&state);
            let canceller = thread::spawn(move || {
                cancel_start_rx.recv_timeout(timeout).unwrap();
                cancel_done_tx.send(cancel_state.request_cancel()).unwrap();
            });
            worker_start_tx.send(()).unwrap();
            cancel_start_tx.send(()).unwrap();
            worker_done_rx.recv_timeout(timeout).unwrap();
            let cancelled = cancel_done_rx.recv_timeout(timeout).unwrap();
            worker.join().unwrap();
            canceller.join().unwrap();
            dropped_rx.recv_timeout(timeout).unwrap();
            assert!(matches!(
                dropped_rx.try_recv(),
                Err(TryRecvError::Disconnected)
            ));
            let result = state.result().unwrap().unwrap();
            if cancelled {
                assert_cancelled(result.unwrap_err());
                assert_eq!(state.status_name(), "cancelled");
            } else {
                assert_eq!(result.unwrap(), Value::Int(7));
                assert_eq!(state.status_name(), "completed");
            }
        }
    }

    #[test]
    fn consumed_cancelled_queue_node_stays_active_until_frame_drop_finishes() {
        struct PausedDrop {
            state: Weak<TaskState>,
            entered: mpsc::SyncSender<bool>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for PausedDrop {
            fn drop(&mut self) {
                let unlocked = self.state.upgrade().unwrap().result.try_lock().is_ok();
                self.entered.send(unlocked).unwrap();
                self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
        }
        let timeout = Duration::from_secs(2);
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
        let mut initial = completion_state();
        initial.runtime = Arc::downgrade(&runtime.inner);
        initial.status.store(TASK_PENDING, Ordering::Release);
        let state = Arc::new(initial);
        runtime.inner.active_tasks.store(1, Ordering::Release);
        runtime
            .inner
            .states
            .lock()
            .unwrap()
            .insert(state.id, Arc::downgrade(&state));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let probe = PausedDrop {
            state: Arc::downgrade(&state),
            entered: entered_tx,
            release: release_rx,
        };
        state.install_run(Box::new(move || {
            drop(probe);
            Ok(Value::Null)
        }));
        let cancel_state = Arc::clone(&state);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let canceller = thread::spawn(move || {
            done_tx.send(cancel_state.request_cancel()).unwrap();
        });
        assert!(
            entered_rx.recv_timeout(timeout).unwrap(),
            "drop held result lock"
        );
        // Reproduce a worker consuming the now-empty queue node while its
        // canceller is still in frame destruction; no user job is run here.
        assert!(state.claim_run().is_none());
        finish_task_job(&runtime.inner, &state);
        assert_eq!(runtime.snapshot().unwrap().active_tasks, 1);
        assert_eq!(runtime.snapshot().unwrap().finished_tasks, 0);
        assert_eq!(state.status_name(), "cancelling");
        assert!(state.execution_queue_retired.load(Ordering::Acquire));
        assert!(!state.completion_cleanup_done.load(Ordering::Acquire));
        assert!(!state.accounting_done.load(Ordering::Acquire));
        release_tx.send(()).unwrap();
        assert!(done_rx.recv_timeout(timeout).unwrap());
        canceller.join().unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.finished_tasks, 1);
        assert_eq!(snapshot.registered_tasks, 0);
        assert!(state.completion_cleanup_done.load(Ordering::Acquire));
        assert!(state.accounting_done.load(Ordering::Acquire));
        finish_task_job(&runtime.inner, &state);
        state.complete(state.id, Ok(Value::Null));
        assert_eq!(runtime.snapshot().unwrap().finished_tasks, 1);
    }

    #[test]
    fn last_owner_release_discards_late_owned_completion_with_observer_alive() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let owner = handle_for_state(&runtime, &state);
        drop(owner);
        let (payload, payload_state, payload_runtime) = tracked_owned_result();
        state.complete(state.id, payload);
        assert!(payload_state.upgrade().is_none());
        assert!(payload_runtime.upgrade().is_none());
        assert!(state.result().unwrap().is_none());
        assert_eq!(state.status_name(), "cancelled");
    }

    #[test]
    fn http_style_temporary_owner_drop_inherits_latched_timeout_without_cleanup_guard() {
        let _execution = ExecutionTerminationGuard::enter();
        assert!(current_task_cancellation().is_none());
        assert!(current_cleanup_context().is_none());
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let state = Arc::new(completion_state());
        let temporary = handle_for_state(&runtime, &state);
        let context = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now(),
        };
        // Model an earlier call argument owning a Task while a later HTTP
        // argument detects timeout, before interpreter finally cleanup begins.
        set_execution_termination(context);
        drop(temporary);
        assert_eq!(state.cancellation(), Some(context));
        assert_eq!(current_execution_termination(), Some(context));
        assert!(current_cleanup_context().is_none());
        state.complete(state.id, Ok(Value::Null));
        assert_eq!(state.status_name(), "timed_out");
        assert!(state.result().unwrap().is_none());
    }

    #[test]
    fn executing_helped_task_isolates_and_restores_callers_termination_context() {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
        let mut initial = completion_state();
        initial.runtime = Arc::downgrade(&runtime.inner);
        let state = Arc::new(initial);
        state.install_run(Box::new(|| {
            assert!(current_execution_termination().is_none());
            ensure_task_operations_allowed(Span::default())?;
            Ok(Value::Int(7))
        }));
        runtime.inner.active_tasks.store(1, Ordering::Release);
        runtime
            .inner
            .states
            .lock()
            .unwrap()
            .insert(state.id, Arc::downgrade(&state));
        let parent = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now(),
        };
        set_execution_termination(parent);
        execute_task_job(
            &runtime.inner,
            TaskJob {
                id: state.id,
                state: Arc::clone(&state),
            },
        );
        assert_eq!(current_execution_termination(), Some(parent));
        assert_eq!(state.result().unwrap().unwrap().unwrap(), Value::Int(7));
        assert_eq!(runtime.snapshot().unwrap().finished_tasks, 1);
    }

    #[test]
    fn blocking_queue_handoff_never_exposes_empty_shutdown_accounting() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        runtime
            .inner
            .queued_blocking_jobs
            .store(1, Ordering::Release);
        let queued = runtime.snapshot().unwrap();
        assert_eq!(queued.queued_blocking_jobs, 1);
        assert_eq!(queued.running_blocking_jobs, 0);

        // Deterministically stop at the actual handoff phase used by the worker:
        // running is already owned, but queued has not been removed yet.
        let running = RunningBlockingJob::claim(&runtime.inner);
        let transferring = runtime.snapshot().unwrap();
        assert_eq!(transferring.queued_blocking_jobs, 1);
        assert_eq!(transferring.running_blocking_jobs, 1);
        assert_eq!(
            runtime
                .cancel_all_and_wait(Duration::ZERO)
                .unwrap_err()
                .code
                .as_deref(),
            Some("shutdown_timeout")
        );
        runtime
            .inner
            .queued_blocking_jobs
            .fetch_sub(1, Ordering::AcqRel);
        let claimed = runtime.snapshot().unwrap();
        assert_eq!(claimed.queued_blocking_jobs, 0);
        assert_eq!(claimed.running_blocking_jobs, 1);
        assert_eq!(
            runtime
                .cancel_all_and_wait(Duration::ZERO)
                .unwrap_err()
                .code
                .as_deref(),
            Some("shutdown_timeout")
        );
        drop(running);
        runtime.cancel_all_and_wait(Duration::ZERO).unwrap();
        let finished = runtime.snapshot().unwrap();
        assert_eq!(finished.queued_blocking_jobs, 0);
        assert_eq!(finished.running_blocking_jobs, 0);
    }

    #[test]
    fn failed_blocking_send_inherits_cancellation_after_workers_last_check() {
        let timeout = Duration::from_secs(2);
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let parent = Arc::new(completion_state());
        let nested = Arc::new(completion_state());
        let nested_owner = handle_for_state(&runtime, &nested);
        let result = Ok(Value::Result {
            ok: true,
            value: Box::new(Value::Array(vec![
                Value::String("late owned result".repeat(128)),
                Value::Task(nested_owner),
            ])),
        });
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let (checked_tx, checked_rx) = mpsc::sync_channel(1);
        let (send_tx, send_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker_parent = Arc::clone(&parent);
        let worker = thread::spawn(move || {
            assert!(worker_parent.cancellation().is_none());
            checked_tx.send(()).unwrap();
            send_rx.recv_timeout(timeout).unwrap();
            send_blocking_result(&response_tx, Some(&worker_parent), result);
            done_tx.send(()).unwrap();
        });
        checked_rx.recv_timeout(timeout).unwrap();
        let context = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now(),
        };
        assert!(parent.request_cancel_with(context));
        assert!(!parent.request_cancel_with(CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: context.cleanup_deadline + Duration::from_secs(1),
        }));
        drop(response_rx);
        send_tx.send(()).unwrap();
        done_rx.recv_timeout(timeout).unwrap();
        worker.join().unwrap();
        assert!(nested.owner_released.load(Ordering::Acquire));
        assert_eq!(nested.cancellation(), Some(context));
        assert_eq!(parent.cancellation(), Some(context));
    }

    #[test]
    fn cancel_between_worker_check_and_completion_commits_cancelled_result() {
        let state = completion_state();
        // Reproduce the exact reachable ordering in execute_task_job: its
        // cancellation check has passed, but the result lock is not held yet.
        assert!(!state.is_cancelled());
        assert!(state.request_cancel());
        state.complete_with_status(41, Ok(Value::String("late success".into())), None);
        assert_eq!(state.status_name(), "cancelled");
        let error = state.result().unwrap().unwrap().unwrap_err();
        assert_eq!(error.message, "task 41 was cancelled");
        assert_cancelled(error);
    }

    #[test]
    fn task_completion_first_rejects_cancel_and_releases_duplicate_owned_result() {
        let state = completion_state();
        let (first, first_state, first_runtime) = tracked_owned_result();
        state.complete(41, first);
        assert_eq!(state.status_name(), "completed");
        assert!(!state.request_cancel());
        assert!(!state.request_cancel());
        let (late, late_state, late_runtime) = tracked_owned_result();
        state.complete_with_status(41, late, Some(TASK_PANICKED));
        assert_eq!(state.status_name(), "completed");
        assert!(late_state.upgrade().is_none());
        assert!(late_runtime.upgrade().is_none());
        assert!(first_state.upgrade().is_some());
        assert!(first_runtime.upgrade().is_some());
        drop(state);
        assert!(first_state.upgrade().is_none());
        assert!(first_runtime.upgrade().is_none());
    }

    #[test]
    fn task_cancel_first_releases_late_owned_results_and_stays_terminal() {
        let state = completion_state();
        assert!(state.request_cancel());
        assert!(!state.request_cancel());
        for supplied_status in [None, Some(TASK_PANICKED)] {
            let (late, late_state, late_runtime) = tracked_owned_result();
            state.complete_with_status(41, late, supplied_status);
            assert_eq!(state.status_name(), "cancelled");
            assert!(late_state.upgrade().is_none());
            assert!(late_runtime.upgrade().is_none());
            assert!(!state.request_cancel());
        }
        assert!(state.result.try_lock().is_ok());
    }

    #[test]
    fn task_status_updates_cannot_overwrite_cancellation_or_terminal_states() {
        let cancelling = completion_state();
        assert!(cancelling.request_cancel());
        for update in [TASK_RUNNING, TASK_WAITING] {
            cancelling.set_status(update);
            assert_eq!(cancelling.status_name(), "cancelling");
        }
        for (result, supplied_status, expected) in [
            (Ok(Value::Null), None, "completed"),
            (
                Ok(task_error("application", "recoverable Result")),
                None,
                "completed",
            ),
            (
                Err(KuError::runtime("runtime failure", Span::default())),
                None,
                "failed",
            ),
            (
                Ok(task_error("panic", "async task panicked")),
                Some(TASK_PANICKED),
                "panicked",
            ),
        ] {
            let state = completion_state();
            state.complete_with_status(41, result, supplied_status);
            assert_eq!(state.status_name(), expected);
            for update in [TASK_RUNNING, TASK_WAITING] {
                state.set_status(update);
                assert_eq!(state.status_name(), expected);
            }
            assert!(!state.request_cancel());
        }
    }

    #[test]
    fn task_cancel_before_panicked_completion_keeps_cancellation_result() {
        let state = completion_state();
        assert!(state.request_cancel());
        state.complete_with_status(
            41,
            Ok(task_error("panic", "async task panicked")),
            Some(TASK_PANICKED),
        );
        assert_eq!(state.status_name(), "cancelled");
        assert_cancelled(state.result().unwrap().unwrap().unwrap_err());
    }

    #[test]
    fn task_cancel_and_complete_two_thread_race_keeps_outcome_consistent() {
        enum Finished {
            Cancel(bool),
            Complete,
        }
        let timeout = Duration::from_secs(2);
        for round in 0..32 {
            let id = 100 + round;
            let state = Arc::new(completion_state());
            let (cancel_start, cancel_ready) = mpsc::channel();
            let (complete_start, complete_ready) = mpsc::channel();
            let (finished, results) = mpsc::channel();
            let cancel_state = Arc::clone(&state);
            let cancel_finished = finished.clone();
            let canceller = thread::spawn(move || {
                cancel_ready.recv_timeout(timeout).expect("cancel start");
                let accepted = cancel_state.request_cancel();
                cancel_finished
                    .send(Finished::Cancel(accepted))
                    .expect("cancel result");
            });
            let complete_state = Arc::clone(&state);
            let completer = thread::spawn(move || {
                complete_ready
                    .recv_timeout(timeout)
                    .expect("complete start");
                complete_state.complete(id, Ok(Value::Int(round)));
                finished
                    .send(Finished::Complete)
                    .expect("completion result");
            });
            // Alternate the launch order, without requiring either race winner.
            if round % 2 == 0 {
                cancel_start.send(()).unwrap();
                complete_start.send(()).unwrap();
            } else {
                complete_start.send(()).unwrap();
                cancel_start.send(()).unwrap();
            }
            let mut cancelled = None;
            let mut completed = false;
            for _ in 0..2 {
                match results
                    .recv_timeout(timeout)
                    .expect("bounded completion race")
                {
                    Finished::Cancel(accepted) => cancelled = Some(accepted),
                    Finished::Complete => completed = true,
                }
            }
            // Join only after both closures have reported completion; no lock or
            // unbounded barrier is used to wait for the competing operations.
            canceller.join().expect("canceller exited");
            completer.join().expect("completer exited");
            assert!(completed);
            let actual = state.result().unwrap().unwrap();
            if cancelled.expect("cancel outcome") {
                assert_eq!(state.status_name(), "cancelled");
                let error = actual.unwrap_err();
                assert_eq!(error.message, format!("task {id} was cancelled"));
                assert_cancelled(error);
            } else {
                assert_eq!(state.status_name(), "completed");
                assert_eq!(actual.unwrap(), Value::Int(round));
            }
            assert!(!state.request_cancel());
        }
    }

    #[test]
    fn task_worker_panic_and_accepted_cancellation_preserve_classification() {
        let runtime = TaskRuntime::with_limits(1, 2, 1, 1, 2);
        let timeout = Duration::from_secs(2);
        for cancel_before_exit in [false, true] {
            let (started, start_result) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let task = spawn_task(&runtime, move || -> KuResult<Value> {
                started
                    .send((
                        current_task_id(),
                        thread::current().name().unwrap_or("").to_string(),
                    ))
                    .expect("worker start signal");
                released
                    .recv_timeout(timeout)
                    .expect("bounded worker release");
                panic!("intentional task worker panic");
            });
            // No await/helping is entered until the real worker is running.
            let (running_id, worker_name) =
                start_result.recv_timeout(timeout).expect("worker start");
            assert_eq!(running_id, task.id());
            assert!(worker_name.starts_with("ku-task-"), "{worker_name}");
            if cancel_before_exit {
                assert!(task.cancel());
                assert_eq!(task.status(), "cancelling");
                assert!(!task.cancel());
            }
            release.send(()).unwrap();
            let result = task.await_timeout(timeout);
            if cancel_before_exit {
                assert_eq!(task.status(), "cancelled");
                assert_cancelled(result.unwrap_err());
            } else {
                assert_eq!(task.status(), "panicked");
                assert_eq!(result.unwrap(), task_error("panic", "async task panicked"));
            }
            assert!(!task.cancel());
        }
        runtime
            .cancel_all_and_wait(timeout)
            .expect("worker cleanup");
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
    }

    fn wait_until(timeout: Duration, message: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !ready() {
            assert!(Instant::now() < deadline, "{message}");
            thread::yield_now();
        }
    }

    #[test]
    fn runtime_workers_do_not_keep_inner_alive() {
        let runtime = TaskRuntime::with_limits(1, 1, 1, 1, 1);
        let inner = Arc::downgrade(&runtime.inner);
        drop(runtime);

        let deadline = Instant::now() + Duration::from_secs(1);
        while inner.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            inner.upgrade().is_none(),
            "worker threads must not retain TaskRuntimeInner"
        );
    }

    #[test]
    fn timeout_does_not_cancel_target_task() {
        let runtime = TaskRuntime::with_limits(1, 2, 1, 1, 2);
        let task = spawn_task(&runtime, || {
            thread::sleep(Duration::from_millis(20));
            Ok(Value::Int(7))
        });

        let timed = task
            .await_timeout(Duration::ZERO)
            .expect("timeout should be a task Result value");
        assert!(matches!(timed, Value::Result { ok: false, .. }));
        assert_eq!(
            task.await_result().expect("task should finish"),
            Value::Int(7)
        );
        assert_eq!(task.status(), "completed");
    }

    #[test]
    fn await_result_consumes_task_once() {
        let runtime = TaskRuntime::with_limits(1, 2, 1, 1, 2);
        let task = spawn_task(&runtime, || Ok(Value::Int(7)));

        assert_eq!(
            task.await_result().expect("first await should finish"),
            Value::Int(7)
        );
        let second = task
            .await_result()
            .expect("second await should return a task Result value");
        let Value::Result { ok, value } = second else {
            panic!("second await should be a structured task error");
        };
        assert!(!ok);
        let Value::Object(fields) = *value else {
            panic!("task error payload should be an object");
        };
        assert_eq!(
            fields.get("code"),
            Some(&Value::String("already_awaited".to_string()))
        );
    }

    #[test]
    fn one_worker_nested_await_executes_the_queued_child() {
        let runtime = TaskRuntime::with_limits(1, 4, 1, 1, 4);
        let nested_runtime = runtime.clone();
        let parent = spawn_task(&runtime, move || {
            let child = spawn_task(&nested_runtime, || Ok(Value::Int(7)));
            child.await_timeout(Duration::from_secs(1))
        });

        assert_eq!(
            parent
                .await_timeout(Duration::from_secs(1))
                .expect("one-worker nested await should not starve"),
            Value::Int(7)
        );
    }

    #[test]
    fn cancellation_is_idempotent_and_wakes_waiters() {
        let runtime = TaskRuntime::with_limits(1, 2, 1, 1, 2);
        let task = spawn_task(&runtime, || {
            while !current_task_cancelled() {
                thread::yield_now();
            }
            Ok(Value::Int(1))
        });

        assert!(task.cancel());
        assert!(!task.cancel());
        assert_eq!(
            runtime
                .cancel_all_and_wait(Duration::from_secs(1))
                .expect("bounded shutdown should drain cancelled tasks"),
            0
        );
        let error = task
            .await_timeout(Duration::from_secs(1))
            .expect_err("cancelled task should wake waiters with internal termination");
        assert_cancelled(error);
        assert_eq!(task.status(), "cancelled");
    }

    #[test]
    fn snapshot_tracks_timeout_cancel_queue_and_reclamation() {
        let runtime = TaskRuntime::with_limits(1, 4, 1, 1, 4);
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let first_started = Arc::clone(&started);
        let first_release = Arc::clone(&release);
        let first = spawn_task(&runtime, move || {
            first_started.store(true, Ordering::Release);
            while !first_release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            Ok(Value::Int(1))
        });
        wait_until(Duration::from_secs(1), "first task did not start", || {
            started.load(Ordering::Acquire)
        });

        let second = spawn_task(&runtime, || Ok(Value::Int(2)));
        let third = spawn_task(&runtime, || Ok(Value::Int(3)));
        let snapshot = runtime.snapshot().expect("snapshot queued tasks");
        assert_eq!(snapshot.active_tasks, 3);
        assert_eq!(snapshot.registered_tasks, 3);
        assert_eq!(snapshot.queued_tasks, 2);
        assert_eq!(snapshot.running_tasks, 1);
        assert_eq!(snapshot.pending_tasks, 2);
        assert_eq!(snapshot.wait_edges, 0);

        assert!(second.cancel());
        let timed = third
            .await_timeout(Duration::ZERO)
            .expect("zero timeout should return a task error value");
        assert!(matches!(timed, Value::Result { ok: false, .. }));
        let snapshot = runtime.snapshot().expect("snapshot cancelled task");
        assert_eq!(snapshot.queued_tasks, 2);
        assert_eq!(snapshot.cancelled_tasks, 1);
        assert_eq!(snapshot.cancelling_tasks, 0);

        release.store(true, Ordering::Release);
        assert_eq!(
            first
                .await_timeout(Duration::from_secs(1))
                .expect("first task"),
            Value::Int(1)
        );
        assert_cancelled(
            second
                .await_timeout(Duration::from_secs(1))
                .expect_err("cancelled second task"),
        );
        assert_eq!(
            third
                .await_timeout(Duration::from_secs(1))
                .expect("third task"),
            Value::Int(3)
        );
        assert_eq!(
            runtime
                .cancel_all_and_wait(Duration::from_secs(1))
                .expect("shutdown should observe an empty runtime"),
            0
        );

        let snapshot = runtime.snapshot().expect("snapshot reclaimed runtime");
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.queued_tasks, 0);
        assert_eq!(snapshot.wait_edges, 0);
        assert_eq!(snapshot.queued_blocking_jobs, 0);
        assert_eq!(snapshot.running_blocking_jobs, 0);
    }

    #[test]
    fn shutdown_timeout_is_bounded_for_non_cooperative_task() {
        let runtime = TaskRuntime::with_limits(1, 1, 1, 1, 1);
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&started);
        let task_release = Arc::clone(&release);
        let task = spawn_task(&runtime, move || {
            task_started.store(true, Ordering::Release);
            while !task_release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            Ok(Value::Null)
        });
        wait_until(
            Duration::from_secs(1),
            "non-cooperative task did not start",
            || started.load(Ordering::Acquire),
        );

        let began = Instant::now();
        let error = runtime
            .cancel_all_and_wait(Duration::from_millis(20))
            .expect_err("non-cooperative task must hit bounded shutdown timeout");
        assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
        assert!(began.elapsed() < Duration::from_secs(1));
        let snapshot = runtime.snapshot().expect("snapshot timed out shutdown");
        assert_eq!(snapshot.active_tasks, 1);
        assert_eq!(snapshot.cancelling_tasks, 1);

        release.store(true, Ordering::Release);
        assert_cancelled(
            task.await_timeout(Duration::from_secs(1))
                .expect_err("cancelled task should finish after release"),
        );
        runtime
            .cancel_all_and_wait(Duration::from_secs(1))
            .expect("runtime should drain after release");
        assert_eq!(runtime.snapshot().expect("final snapshot").active_tasks, 0);
    }

    #[test]
    fn blocking_response_delivery_does_not_acknowledge_running_guard_retirement() {
        let timeout = Duration::from_secs(1);
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
        let inner = Arc::clone(&runtime.inner);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            // Exercise the actual accounting guard and publication helper in
            // their worker order. This channel only holds the guard's lifetime;
            // receiving a result must not be treated as cleanup acknowledgement.
            let running = RunningBlockingJob::claim(&inner);
            send_blocking_result(&response_tx, None, Ok(Value::Int(7)));
            release_rx
                .recv_timeout(timeout)
                .expect("bounded blocking retirement release");
            drop(running);
        });
        assert_eq!(
            response_rx
                .recv_timeout(timeout)
                .expect("blocking response published")
                .expect("blocking result"),
            Value::Int(7)
        );
        let delivered = runtime.snapshot().expect("delivered result snapshot");
        assert_eq!(delivered.queued_blocking_jobs, 0);
        assert_eq!(delivered.running_blocking_jobs, 1);
        assert_eq!(
            runtime
                .cancel_all_and_wait(Duration::ZERO)
                .expect_err("live blocking guard still owns cleanup")
                .code
                .as_deref(),
            Some("shutdown_timeout")
        );
        release_tx.send(()).expect("release blocking retirement");
        worker.join().expect("blocking guard retired");
        runtime
            .cancel_all_and_wait(Duration::ZERO)
            .expect("retired blocking guard permits shutdown");
        let reclaimed = runtime.snapshot().expect("retired guard snapshot");
        assert_eq!(reclaimed.queued_blocking_jobs, 0);
        assert_eq!(reclaimed.running_blocking_jobs, 0);
    }

    #[test]
    fn snapshot_tracks_blocking_queue_reclamation() {
        let runtime = TaskRuntime::with_limits(1, 1, 1, 2, 1);
        let timeout = Duration::from_secs(1);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);

        let first_runtime = runtime.clone();
        let first = thread::spawn(move || {
            first_runtime.run_blocking(
                move || {
                    started_tx.send(()).expect("blocking start notification");
                    release_rx
                        .recv_timeout(timeout)
                        .expect("bounded blocking job release");
                    Ok(Value::Int(1))
                },
                Span::default(),
            )
        });
        started_rx
            .recv_timeout(timeout)
            .expect("blocking job did not start");

        let second_runtime = runtime.clone();
        let second = thread::spawn(move || {
            second_runtime.run_blocking(|| Ok(Value::Int(2)), Span::default())
        });
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = runtime.snapshot().expect("snapshot blocking queue");
            if snapshot.running_blocking_jobs == 1 && snapshot.queued_blocking_jobs == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "blocking queue did not reach the expected bounded state: {snapshot:?}"
            );
            thread::yield_now();
        }

        release_tx.send(()).expect("release first blocking job");
        assert_eq!(
            first
                .join()
                .expect("first blocking caller")
                .expect("first job"),
            Value::Int(1)
        );
        assert_eq!(
            second
                .join()
                .expect("second blocking caller")
                .expect("second job"),
            Value::Int(2)
        );
        // Caller joins prove response delivery, not worker-side cleanup ACK:
        // send_blocking_result runs before RunningBlockingJob's final drop.
        runtime
            .cancel_all_and_wait(timeout)
            .expect("blocking worker accounting should drain after replies");
        let snapshot = runtime.snapshot().expect("reclaimed blocking queue");
        assert_eq!(snapshot.queued_blocking_jobs, 0);
        assert_eq!(snapshot.running_blocking_jobs, 0);
    }

    #[test]
    fn shutdown_waits_for_already_running_blocking_jobs() {
        let runtime = TaskRuntime::with_limits(1, 1, 1, 1, 1);
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let task_runtime = runtime.clone();
        let job_started = Arc::clone(&started);
        let job_release = Arc::clone(&release);
        let task = spawn_task(&runtime, move || {
            task_runtime.run_blocking(
                move || {
                    job_started.store(true, Ordering::Release);
                    while !job_release.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    Ok(Value::Null)
                },
                Span::default(),
            )
        });
        wait_until(
            Duration::from_secs(1),
            "blocking shutdown test job did not start",
            || started.load(Ordering::Acquire),
        );

        let error = runtime
            .cancel_all_and_wait(Duration::from_millis(20))
            .expect_err("shutdown must report a running blocking job");
        assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
        assert_eq!(
            runtime
                .snapshot()
                .expect("blocking shutdown snapshot")
                .running_blocking_jobs,
            1
        );

        release.store(true, Ordering::Release);
        assert_cancelled(
            task.await_timeout(Duration::from_secs(1))
                .expect_err("cancelled task should finish"),
        );
        runtime
            .cancel_all_and_wait(Duration::from_secs(1))
            .expect("runtime should drain after blocking job release");
    }

    #[test]
    #[ignore = "manual bounded million-demand concurrency stress benchmark"]
    fn million_concurrent_demand_stress_report() {
        let runtime = TaskRuntime::with_limits(4, 1024, 4, 1024, 1024);
        let report = runtime
            .stress_concurrent_demand(1_000_000, 15, Duration::from_millis(250))
            .expect("million-demand stress must drain within the bounded deadline");
        assert_eq!(report.demand, 1_000_000);
        assert_eq!(report.peak_active, 1024);
        assert_eq!(
            report.accepted
                + report.rejected_limit
                + report.rejected_queue
                + report.rejected_internal,
            report.demand
        );
        let snapshot = runtime.snapshot().expect("stress snapshot");
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.queued_tasks, 0);
        println!(
            "KU_ASYNC_STRESS demand={} producers={} peak_active={} accepted={} rejected_limit={} rejected_queue={} rejected_internal={} finished={} submit_ms={} total_ms={} demand_per_sec={:.0} accepted_per_sec={:.0}",
            report.demand,
            report.producers,
            report.peak_active,
            report.accepted,
            report.rejected_limit,
            report.rejected_queue,
            report.rejected_internal,
            report.finished,
            report.submit_ms,
            report.total_ms,
            report.demand as f64 / (report.submit_ms.max(1) as f64 / 1000.0),
            report.accepted as f64 / (report.total_ms.max(1) as f64 / 1000.0)
        );
    }

    #[test]
    fn terminal_completion_cleanup_does_not_acknowledge_execution_queue_retirement() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
        let mut initial = completion_state();
        initial.runtime = Arc::downgrade(&runtime.inner);
        let state = Arc::new(initial);
        runtime.inner.active_tasks.store(1, Ordering::Release);
        runtime
            .inner
            .states
            .lock()
            .unwrap()
            .insert(state.id, Arc::downgrade(&state));

        state.complete(state.id, Ok(Value::Int(17)));
        assert!(state.is_terminal());
        assert!(state.completion_cleanup_done.load(Ordering::Acquire));
        assert!(!state.execution_queue_retired.load(Ordering::Acquire));
        finish_task_accounting(&runtime.inner, &state);
        assert!(!state.accounting_finished.load(Ordering::Acquire));
        assert!(!state.accounting_done.load(Ordering::Acquire));
        assert_eq!(runtime.snapshot().unwrap().active_tasks, 1);

        // Kernel reproduction of the real claimant's post-frame-drop callback.
        finish_task_job(&runtime.inner, &state);
        assert!(state.execution_queue_retired.load(Ordering::Acquire));
        assert!(state.accounting_done.load(Ordering::Acquire));
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.finished_tasks, 1);
        finish_task_job(&runtime.inner, &state);
        state.complete(state.id, Ok(Value::Null));
        assert_eq!(runtime.snapshot().unwrap().finished_tasks, 1);
        // A successful retained result remains owned by the caller; this is
        // execution/accounting evidence, never a full owner-resource ACK.
        assert!(!state.owner_released.load(Ordering::Acquire));
        assert_eq!(state.result().unwrap(), Some(Ok(Value::Int(17))));
    }

    #[test]
    fn accounting_claim_is_not_the_post_registry_and_counter_witness() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
        let mut initial = completion_state();
        initial.runtime = Arc::downgrade(&runtime.inner);
        let state = Arc::new(initial);
        runtime.inner.active_tasks.store(1, Ordering::Release);
        runtime
            .inner
            .states
            .lock()
            .unwrap()
            .insert(state.id, Arc::downgrade(&state));
        state.complete(state.id, Ok(Value::Null));

        let registry = runtime.inner.states.lock().unwrap();
        let inner = Arc::clone(&runtime.inner);
        let retiring = Arc::clone(&state);
        let worker = thread::spawn(move || finish_task_job(&inner, &retiring));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.accounting_finished.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        let claimed = state.accounting_finished.load(Ordering::Acquire);
        let done_before_unlock = state.accounting_done.load(Ordering::Acquire);
        let active_before_unlock = runtime.inner.active_tasks.load(Ordering::Acquire);
        let finished_before_unlock = runtime.inner.finished_tasks.load(Ordering::Acquire);
        let registered_before_unlock = registry.contains_key(&state.id);
        // Unlock before assertions and join even on a failed observation.
        drop(registry);
        worker
            .join()
            .expect("retirement thread exits after registry unlock");

        assert!(claimed);
        assert!(!done_before_unlock);
        assert_eq!(active_before_unlock, 1);
        assert_eq!(finished_before_unlock, 0);
        assert!(registered_before_unlock);
        assert!(state.accounting_done.load(Ordering::Acquire));
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.finished_tasks, 1);
    }

    #[test]
    fn poisoned_registry_retirement_removes_the_real_weak_entry_before_done() {
        let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
        let mut initial = completion_state();
        initial.runtime = Arc::downgrade(&runtime.inner);
        let state = Arc::new(initial);
        runtime.inner.active_tasks.store(1, Ordering::Release);
        runtime
            .inner
            .states
            .lock()
            .unwrap()
            .insert(state.id, Arc::downgrade(&state));
        let poison = catch_unwind(AssertUnwindSafe(|| {
            let _registry = runtime.inner.states.lock().unwrap();
            panic!("formal poisoned registry retirement probe");
        }));
        assert!(poison.is_err());
        assert!(runtime.inner.states.is_poisoned());
        state.complete(state.id, Ok(Value::Null));
        finish_task_job(&runtime.inner, &state);

        assert!(state.accounting_done.load(Ordering::Acquire));
        assert_eq!(runtime.inner.active_tasks.load(Ordering::Acquire), 0);
        assert_eq!(runtime.inner.finished_tasks.load(Ordering::Acquire), 1);
        assert!(runtime
            .inner
            .states
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty());
        // Cleanup recovery does not unpoison the registration API.
        assert!(runtime.inner.states.is_poisoned());
        finish_task_job(&runtime.inner, &state);
        assert_eq!(runtime.inner.finished_tasks.load(Ordering::Acquire), 1);
    }

    #[test]
    fn concurrent_completion_and_execution_retirement_never_lose_the_accounting_trigger() {
        for _ in 0..64 {
            let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 1);
            let mut initial = completion_state();
            initial.runtime = Arc::downgrade(&runtime.inner);
            let state = Arc::new(initial);
            runtime.inner.active_tasks.store(1, Ordering::Release);
            runtime
                .inner
                .states
                .lock()
                .unwrap()
                .insert(state.id, Arc::downgrade(&state));
            let (complete_tx, complete_rx) = mpsc::sync_channel(1);
            let (retire_tx, retire_rx) = mpsc::sync_channel(1);
            let completing = Arc::clone(&state);
            let completion = thread::spawn(move || {
                complete_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("bounded completion start");
                completing.complete(completing.id, Ok(Value::Int(17)));
            });
            let retiring = Arc::clone(&state);
            let inner = Arc::clone(&runtime.inner);
            let execution = thread::spawn(move || {
                retire_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("bounded retirement start");
                finish_task_job(&inner, &retiring);
            });
            complete_tx.send(()).expect("release completion start");
            retire_tx.send(()).expect("release retirement start");
            completion.join().expect("completion thread exits");
            execution.join().expect("retirement thread exits");
            assert!(state.completion_cleanup_done.load(Ordering::Acquire));
            assert!(state.execution_queue_retired.load(Ordering::Acquire));
            assert!(state.accounting_done.load(Ordering::Acquire));
            let snapshot = runtime.snapshot().unwrap();
            assert_eq!(snapshot.active_tasks, 0);
            assert_eq!(snapshot.registered_tasks, 0);
            assert_eq!(snapshot.finished_tasks, 1);
        }
    }
}
