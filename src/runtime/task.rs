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
    error::{KuError, KuResult},
    span::Span,
    stdlib::errors,
    value::Value,
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

thread_local! {
    static CURRENT_TASK_ID: Cell<i64> = const { Cell::new(0) };
    static CURRENT_TASK_STATE: RefCell<Option<Weak<TaskState>>> = const { RefCell::new(None) };
    static AWAIT_HELP_DEPTH: Cell<usize> = const { Cell::new(0) };
}

type TaskFn = Box<dyn FnOnce() -> KuResult<Value> + Send + 'static>;
type BlockingFn = Box<dyn FnOnce() -> KuResult<Value> + Send + 'static>;

struct TaskJob {
    id: i64,
    run: TaskFn,
    state: Arc<TaskState>,
}

struct BlockingJob {
    run: BlockingFn,
    response: SyncSender<KuResult<Value>>,
    cancelled: Option<Weak<TaskState>>,
}

#[derive(Clone)]
pub struct TaskRuntime {
    inner: Arc<TaskRuntimeInner>,
}

struct TaskRuntimeInner {
    task_tx: SyncSender<TaskJob>,
    task_rx: Arc<Mutex<Receiver<TaskJob>>>,
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
    next_task_id: AtomicI64,
    shutdown: AtomicBool,
    max_tasks: usize,
    task_queue_limit: usize,
    blocking_queue_limit: usize,
    task_workers: AtomicUsize,
    blocking_workers: AtomicUsize,
}

pub struct TaskHandle {
    id: i64,
    state: Arc<TaskState>,
    runtime: TaskRuntime,
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
    result: Mutex<Option<KuResult<Value>>>,
    ready: Condvar,
    cancelled: AtomicBool,
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

    fn with_limits(
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
            task_rx: Arc::new(Mutex::new(task_rx)),
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
            next_task_id: AtomicI64::new(1),
            shutdown: AtomicBool::new(false),
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

    pub(crate) fn spawn<F>(&self, run: F) -> TaskHandle
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        self.spawn_deferred(|| run)
    }

    pub(crate) fn spawn_deferred<B, F>(&self, build: B) -> TaskHandle
    where
        B: FnOnce() -> F,
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        self.inner.total_submissions.fetch_add(1, Ordering::Relaxed);
        let id = self.inner.next_task_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(TaskState {
            result: Mutex::new(None),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            awaited: AtomicBool::new(false),
            status: AtomicU8::new(TASK_PENDING),
        });
        let handle = TaskHandle {
            id,
            state: Arc::clone(&state),
            runtime: self.clone(),
        };
        if self.inner.task_workers.load(Ordering::Acquire) == 0 {
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
            return handle;
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
            return handle;
        }
        if let Ok(mut states) = self.inner.states.lock() {
            states.insert(id, Arc::downgrade(&state));
        } else {
            self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
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
            return handle;
        }
        let run = match catch_unwind(AssertUnwindSafe(build)) {
            Ok(run) => run,
            Err(_) => {
                self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
                self.inner
                    .rejected_task_internal
                    .fetch_add(1, Ordering::Relaxed);
                self.remove_state(id);
                state.complete(
                    id,
                    Ok(task_error(
                        "spawn_panic",
                        "async task construction panicked",
                    )),
                );
                return handle;
            }
        };
        self.inner.queued_tasks.fetch_add(1, Ordering::AcqRel);
        match self.inner.task_tx.try_send(TaskJob {
            id,
            run: Box::new(run),
            state: Arc::clone(&state),
        }) {
            Ok(()) => {
                self.inner
                    .accepted_submissions
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                self.inner
                    .rejected_task_queue
                    .fetch_add(1, Ordering::Relaxed);
                self.inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
                self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
                self.remove_state(id);
                state.complete(
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
                self.inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
                self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
                self.remove_state(id);
                state.complete(
                    id,
                    Ok(task_error(
                        "runtime_stopped",
                        "async task runtime is stopped",
                    )),
                );
            }
        }
        handle
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
            shutdown: self.inner.shutdown.load(Ordering::Acquire),
            total_submissions: self.inner.total_submissions.load(Ordering::Relaxed),
            accepted_submissions: self.inner.accepted_submissions.load(Ordering::Relaxed),
            rejected_task_limit: self.inner.rejected_task_limit.load(Ordering::Relaxed),
            rejected_task_queue: self.inner.rejected_task_queue.load(Ordering::Relaxed),
            rejected_task_internal: self.inner.rejected_task_internal.load(Ordering::Relaxed),
            finished_tasks: self.inner.finished_tasks.load(Ordering::Relaxed),
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
        let began = Instant::now();
        let mut producers_threads = Vec::with_capacity(producers);
        for producer in 0..producers {
            let runtime = self.clone();
            let producer_release = Arc::clone(&release);
            let peak_active = Arc::clone(&peak_active);
            let count = demand / producers + usize::from(producer < demand % producers);
            let producer_thread = thread::Builder::new()
                .name(format!("ku-stress-producer-{producer}"))
                .spawn(move || {
                    for _ in 0..count {
                        let release = Arc::clone(&producer_release);
                        drop(runtime.spawn(move || {
                            while !release.load(Ordering::Acquire) && !current_task_cancelled() {
                                thread::yield_now();
                            }
                            Ok(Value::Null)
                        }));
                        peak_active.fetch_max(
                            runtime.inner.active_tasks.load(Ordering::Acquire),
                            Ordering::AcqRel,
                        );
                    }
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
            producer_panicked |= producer.join().is_err();
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
        if !handle.state.claim_await() {
            return Ok(task_error(
                "already_awaited",
                format!("task {} has already been awaited", handle.id),
            ));
        }
        self.await_task_until(handle, None)
    }

    pub fn await_task_timeout(&self, handle: &TaskHandle, timeout: Duration) -> KuResult<Value> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            KuError::runtime(
                "task timeout is too large for this platform",
                Span::default(),
            )
        })?;
        self.await_task_until(handle, Some(deadline))
    }

    fn await_task_until(&self, handle: &TaskHandle, deadline: Option<Instant>) -> KuResult<Value> {
        let current = current_task_id();
        if current == handle.id {
            return Ok(task_error(
                "self_await",
                format!("task {} cannot await itself", handle.id),
            ));
        }
        let current_state = if current == 0 {
            None
        } else {
            self.state(current)?
        };
        if current != 0 {
            if let Some(code) = self.register_wait(current, handle.id)? {
                return Ok(task_error(
                    code,
                    format!("task {current} cannot await task {}", handle.id),
                ));
            }
        }
        if let Some(state) = &current_state {
            state.set_status(TASK_WAITING);
        }
        let result = (|| loop {
            if current_task_cancelled() {
                return Ok(task_error(
                    "cancelled",
                    format!("task {current} was cancelled"),
                ));
            }
            if let Some(result) = handle.state.result()? {
                return result;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(task_error(
                    "timeout",
                    format!("timed out waiting for task {}", handle.id),
                ));
            }
            if await_help_depth() >= MAX_AWAIT_DEPTH {
                return Ok(task_error(
                    "await_depth",
                    format!("task {current} exceeded await depth {MAX_AWAIT_DEPTH}"),
                ));
            }
            if self.help_one_bounded()? {
                continue;
            }
            let wait = deadline
                .map(|deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(2))
                })
                .unwrap_or(Duration::from_millis(2));
            if wait.is_zero() {
                continue;
            }
            handle.state.wait(wait)?;
        })();
        if current != 0 {
            self.clear_wait(current);
        }
        if let Some(state) = current_state {
            if !state.is_cancelled() {
                state.set_status(TASK_RUNNING);
            }
        }
        result
    }

    pub fn cancel_all(&self) -> usize {
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
            .filter(|state| state.request_cancel())
            .count()
    }

    pub fn cancel_all_and_wait(&self, timeout: Duration) -> KuResult<usize> {
        let cancelled = self.cancel_all();
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            KuError::runtime(
                "task shutdown timeout is too large for this platform",
                Span::default(),
            )
        })?;
        while self.inner.active_tasks.load(Ordering::Acquire) != 0
            || self.inner.queued_blocking_jobs.load(Ordering::Acquire) != 0
            || self.inner.running_blocking_jobs.load(Ordering::Acquire) != 0
        {
            if Instant::now() >= deadline {
                return Err(KuError::structured(
                    crate::error::KuErrorKind::Runtime,
                    "task",
                    "shutdown_timeout",
                    "async tasks did not stop before the bounded shutdown timeout",
                    Span::default(),
                ));
            }
            if !self.help_one_bounded()? {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(cancelled)
    }

    pub fn run_blocking<F>(&self, run: F, _span: Span) -> KuResult<Value>
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        if self.inner.blocking_workers.load(Ordering::Acquire) == 0 {
            return Ok(task_error(
                "blocking_pool_stopped",
                "blocking pool has no available workers",
            ));
        }
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.inner
            .queued_blocking_jobs
            .fetch_add(1, Ordering::AcqRel);
        match self.inner.blocking_tx.try_send(BlockingJob {
            run: Box::new(run),
            response: response_tx,
            cancelled: CURRENT_TASK_STATE.with(|current| current.borrow().as_ref().cloned()),
        }) {
            Ok(()) => loop {
                if current_task_cancelled() {
                    break Ok(task_error(
                        "cancelled",
                        format!("task {} was cancelled", current_task_id()),
                    ));
                }
                match response_rx.try_recv() {
                    Ok(result) => break result,
                    Err(TryRecvError::Disconnected) => {
                        break Ok(task_error(
                            "blocking_pool_stopped",
                            "blocking pool stopped before returning a result",
                        ))
                    }
                    Err(TryRecvError::Empty) => {
                        if !self.help_one_bounded()? {
                            match response_rx.recv_timeout(Duration::from_millis(2)) {
                                Ok(result) => break result,
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
        let allowed = AWAIT_HELP_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= MAX_AWAIT_DEPTH {
                false
            } else {
                depth.set(current + 1);
                true
            }
        });
        if !allowed {
            return Ok(false);
        }
        let result = self.help_one();
        AWAIT_HELP_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        result
    }

    fn help_one(&self) -> KuResult<bool> {
        let job = {
            // An idle worker blocks in `recv()` while holding the single-consumer
            // receiver lock. In that case it will wake for the queued job itself,
            // so an awaiting worker must not block behind it. When every worker is
            // busy (notably the one-worker nested-await case), the lock is free and
            // this helper can execute one queued child to avoid starvation.
            let receiver = match self.inner.task_rx.try_lock() {
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
    fn complete(&self, id: i64, result: KuResult<Value>) {
        self.complete_with_status(id, result, None);
    }

    fn complete_with_status(&self, id: i64, result: KuResult<Value>, status: Option<u8>) {
        // Cancellation and completion use the same lock. The worker's earlier
        // cancellation check is only advisory: a request may win before commit.
        // Keep losing payloads here so their destructors run after unlocking.
        let mut uncommitted = Some(result);
        if let Ok(mut slot) = self.result.lock() {
            if slot.is_none() {
                let (result, status) = if self.cancelled.load(Ordering::Acquire) {
                    (
                        Ok(task_error("cancelled", format!("task {id} was cancelled"))),
                        TASK_CANCELLED,
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
                *slot = Some(result);
                self.status.store(status, Ordering::Release);
                self.ready.notify_all();
            }
        }
        drop(uncommitted);
    }

    fn request_cancel(&self) -> bool {
        let Ok(slot) = self.result.lock() else {
            return false;
        };
        if slot.is_some() || self.cancelled.load(Ordering::Acquire) {
            return false;
        }
        self.cancelled.store(true, Ordering::Release);
        self.status.store(TASK_CANCELLING, Ordering::Release);
        drop(slot);
        self.ready.notify_all();
        true
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
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
            if slot.is_none() && !self.is_cancelled() {
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
            _ => "unknown",
        }
    }

    fn result(&self) -> KuResult<Option<KuResult<Value>>> {
        self.result
            .lock()
            .map(|slot| slot.clone())
            .map_err(|_| KuError::runtime("async task state is poisoned", Span::default()))
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

fn task_worker_loop(weak: Weak<TaskRuntimeInner>, receiver: Arc<Mutex<Receiver<TaskJob>>>) {
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
                inner.queued_tasks.fetch_sub(1, Ordering::AcqRel);
                execute_task_job(&inner, job)
            }
            Err(_) => return,
        }
    }
}

fn execute_task_job(inner: &TaskRuntimeInner, job: TaskJob) {
    if job.state.is_cancelled() {
        job.state.complete(job.id, Ok(Value::Null));
        finish_task_job(inner, job.id);
        return;
    }
    job.state.set_status(TASK_RUNNING);
    let previous = CURRENT_TASK_ID.with(|current| {
        let previous = current.get();
        current.set(job.id);
        previous
    });
    let previous_state =
        CURRENT_TASK_STATE.with(|current| current.replace(Some(Arc::downgrade(&job.state))));
    let result = catch_unwind(AssertUnwindSafe(job.run))
        .map_err(|_| task_error("panic", "async task panicked"));
    let panicked = result.is_err();
    let result = match result {
        Ok(result) => result,
        Err(error) => Ok(error),
    };
    CURRENT_TASK_STATE.with(|current| {
        current.replace(previous_state);
    });
    CURRENT_TASK_ID.with(|current| current.set(previous));
    job.state
        .complete_with_status(job.id, result, panicked.then_some(TASK_PANICKED));
    finish_task_job(inner, job.id);
}

fn finish_task_job(inner: &TaskRuntimeInner, id: i64) {
    if let Ok(mut states) = inner.states.lock() {
        states.remove(&id);
    }
    inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
    inner.finished_tasks.fetch_add(1, Ordering::Relaxed);
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
                inner.queued_blocking_jobs.fetch_sub(1, Ordering::AcqRel);
                inner.running_blocking_jobs.fetch_add(1, Ordering::AcqRel);
                let cancelled = job
                    .cancelled
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|state| state.is_cancelled());
                let result = if cancelled {
                    Ok(task_error(
                        "cancelled",
                        "blocking job was cancelled before it started",
                    ))
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
                let _ = job.response.send(result);
                inner.running_blocking_jobs.fetch_sub(1, Ordering::AcqRel);
            }
            Err(_) => return,
        }
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

fn await_help_depth() -> usize {
    AWAIT_HELP_DEPTH.with(Cell::get)
}

fn task_error(code: &str, message: impl Into<String>) -> Value {
    errors::err("task", code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_state() -> TaskState {
        TaskState {
            result: Mutex::new(None),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            awaited: AtomicBool::new(false),
            status: AtomicU8::new(TASK_RUNNING),
        }
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
                    state,
                    runtime,
                }),
            ])),
        });
        (result, weak_state, weak_runtime)
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
        let result = state.result().unwrap().unwrap().unwrap();
        let Value::Result { ok: false, value } = result else {
            panic!("cancelled task published a successful payload: {result:?}");
        };
        let Value::Object(fields) = *value else {
            panic!("task cancellation must retain its structured error");
        };
        assert_eq!(fields.get("domain"), Some(&Value::String("task".into())));
        assert_eq!(fields.get("code"), Some(&Value::String("cancelled".into())));
        assert_eq!(
            fields.get("message"),
            Some(&Value::String("task 41 was cancelled".into()))
        );
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
        assert_eq!(
            state.result().unwrap().unwrap().unwrap(),
            task_error("cancelled", "task 41 was cancelled")
        );
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
            let actual = state.result().unwrap().unwrap().unwrap();
            if cancelled.expect("cancel outcome") {
                assert_eq!(state.status_name(), "cancelled");
                assert_eq!(
                    actual,
                    task_error("cancelled", format!("task {id} was cancelled"))
                );
            } else {
                assert_eq!(state.status_name(), "completed");
                assert_eq!(actual, Value::Int(round));
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
            let task = runtime.spawn(move || -> KuResult<Value> {
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
            let result = task.await_timeout(timeout).expect("worker terminal result");
            if cancel_before_exit {
                assert_eq!(task.status(), "cancelled");
                assert_eq!(
                    result,
                    task_error("cancelled", format!("task {} was cancelled", task.id()))
                );
            } else {
                assert_eq!(task.status(), "panicked");
                assert_eq!(result, task_error("panic", "async task panicked"));
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
        let task = runtime.spawn(|| {
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
        let task = runtime.spawn(|| Ok(Value::Int(7)));

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
        let parent = runtime.spawn(move || {
            let child = nested_runtime.spawn(|| Ok(Value::Int(7)));
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
        let task = runtime.spawn(|| {
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
        let value = task
            .await_timeout(Duration::from_secs(1))
            .expect("cancelled task should wake waiters");
        assert!(matches!(value, Value::Result { ok: false, .. }));
        assert_eq!(task.status(), "cancelled");
    }

    #[test]
    fn snapshot_tracks_timeout_cancel_queue_and_reclamation() {
        let runtime = TaskRuntime::with_limits(1, 4, 1, 1, 4);
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let first_started = Arc::clone(&started);
        let first_release = Arc::clone(&release);
        let first = runtime.spawn(move || {
            first_started.store(true, Ordering::Release);
            while !first_release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            Ok(Value::Int(1))
        });
        wait_until(Duration::from_secs(1), "first task did not start", || {
            started.load(Ordering::Acquire)
        });

        let second = runtime.spawn(|| Ok(Value::Int(2)));
        let third = runtime.spawn(|| Ok(Value::Int(3)));
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
        assert_eq!(snapshot.cancelling_tasks, 1);

        release.store(true, Ordering::Release);
        assert_eq!(
            first
                .await_timeout(Duration::from_secs(1))
                .expect("first task"),
            Value::Int(1)
        );
        assert!(matches!(
            second
                .await_timeout(Duration::from_secs(1))
                .expect("cancelled second task"),
            Value::Result { ok: false, .. }
        ));
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
        let task = runtime.spawn(move || {
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
        assert!(matches!(
            task.await_timeout(Duration::from_secs(1))
                .expect("cancelled task should finish after release"),
            Value::Result { ok: false, .. }
        ));
        runtime
            .cancel_all_and_wait(Duration::from_secs(1))
            .expect("runtime should drain after release");
        assert_eq!(runtime.snapshot().expect("final snapshot").active_tasks, 0);
    }

    #[test]
    fn snapshot_tracks_blocking_queue_reclamation() {
        let runtime = TaskRuntime::with_limits(1, 1, 1, 2, 1);
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));

        let first_runtime = runtime.clone();
        let first_started = Arc::clone(&started);
        let first_release = Arc::clone(&release);
        let first = thread::spawn(move || {
            first_runtime.run_blocking(
                move || {
                    first_started.store(true, Ordering::Release);
                    while !first_release.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    Ok(Value::Int(1))
                },
                Span::default(),
            )
        });
        wait_until(Duration::from_secs(1), "blocking job did not start", || {
            started.load(Ordering::Acquire)
        });

        let second_runtime = runtime.clone();
        let second = thread::spawn(move || {
            second_runtime.run_blocking(|| Ok(Value::Int(2)), Span::default())
        });
        let deadline = Instant::now() + Duration::from_secs(1);
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

        release.store(true, Ordering::Release);
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
        let task = runtime.spawn(move || {
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
        let _ = task
            .await_timeout(Duration::from_secs(1))
            .expect("cancelled task should finish");
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
}
