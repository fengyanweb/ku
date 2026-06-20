use std::{
    cell::Cell,
    collections::HashMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        Arc, Condvar, Mutex, Weak,
    },
    thread,
    time::Duration,
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

thread_local! {
    static CURRENT_TASK_ID: Cell<i64> = const { Cell::new(0) };
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
    active_tasks: AtomicUsize,
    next_task_id: AtomicI64,
    shutdown: AtomicBool,
    max_tasks: usize,
    task_queue_limit: usize,
    blocking_queue_limit: usize,
}

pub struct TaskHandle {
    id: i64,
    state: Arc<TaskState>,
    runtime: TaskRuntime,
}

struct TaskState {
    result: Mutex<Option<KuResult<Value>>>,
    ready: Condvar,
    waiting_on: AtomicI64,
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
            active_tasks: AtomicUsize::new(0),
            next_task_id: AtomicI64::new(1),
            shutdown: AtomicBool::new(false),
            max_tasks,
            task_queue_limit,
            blocking_queue_limit,
        });
        spawn_task_workers(&inner, task_workers);
        spawn_blocking_workers(&inner, blocking_workers, blocking_rx);
        Self { inner }
    }

    pub fn spawn<F>(&self, run: F) -> TaskHandle
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        let id = self.inner.next_task_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(TaskState {
            result: Mutex::new(None),
            ready: Condvar::new(),
            waiting_on: AtomicI64::new(0),
        });
        let handle = TaskHandle {
            id,
            state: Arc::clone(&state),
            runtime: self.clone(),
        };
        if self
            .inner
            .active_tasks
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.inner.max_tasks).then_some(current + 1)
            })
            .is_err()
        {
            state.complete(Ok(task_error(
                "too_many_tasks",
                format!("async task limit {} reached", self.inner.max_tasks),
            )));
            return handle;
        }
        if let Ok(mut states) = self.inner.states.lock() {
            states.insert(id, Arc::downgrade(&state));
        } else {
            self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
            state.complete(Err(KuError::runtime(
                "async task registry is poisoned",
                Span::default(),
            )));
            return handle;
        }
        match self.inner.task_tx.try_send(TaskJob {
            id,
            run: Box::new(run),
            state: Arc::clone(&state),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
                self.remove_state(id);
                state.complete(Ok(task_error(
                    "queue_full",
                    format!(
                        "async task queue limit {} reached",
                        self.inner.task_queue_limit
                    ),
                )));
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
                self.remove_state(id);
                state.complete(Ok(task_error(
                    "runtime_stopped",
                    "async task runtime is stopped",
                )));
            }
        }
        handle
    }

    pub fn await_task(&self, handle: &TaskHandle) -> KuResult<Value> {
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
        if let Some(state) = &current_state {
            state.waiting_on.store(handle.id, Ordering::Release);
        }
        let result = (|| loop {
            if let Some(result) = handle.state.result()? {
                return result;
            }
            if current != 0 {
                if let Some(code) = self.wait_cycle_error(current, handle.id)? {
                    return Ok(task_error(
                        code,
                        format!("task {current} cannot await task {}", handle.id),
                    ));
                }
            }
            if self.help_one_bounded()? {
                continue;
            }
            handle.state.wait(Duration::from_millis(2))?;
        })();
        if let Some(state) = current_state {
            state.waiting_on.store(0, Ordering::Release);
        }
        result
    }

    pub fn run_blocking<F>(&self, run: F, _span: Span) -> KuResult<Value>
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        match self.inner.blocking_tx.try_send(BlockingJob {
            run: Box::new(run),
            response: response_tx,
        }) {
            Ok(()) => loop {
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
            Err(TrySendError::Full(_)) => Ok(task_error(
                "queue_full",
                format!(
                    "blocking queue limit {} reached",
                    self.inner.blocking_queue_limit
                ),
            )),
            Err(TrySendError::Disconnected(_)) => Ok(task_error(
                "blocking_pool_stopped",
                "blocking pool is stopped",
            )),
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
            let receiver =
                self.inner.task_rx.lock().map_err(|_| {
                    KuError::runtime("async task queue is poisoned", Span::default())
                })?;
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

    fn wait_cycle_error(&self, current: i64, target: i64) -> KuResult<Option<&'static str>> {
        let mut cursor = target;
        for _ in 0..MAX_AWAIT_DEPTH {
            if cursor == current {
                return Ok(Some("await_cycle"));
            }
            let Some(state) = self.state(cursor)? else {
                return Ok(None);
            };
            let next = state.waiting_on.load(Ordering::Acquire);
            if next == 0 {
                return Ok(None);
            }
            cursor = next;
        }
        Ok(Some("await_depth"))
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
    fn complete(&self, result: KuResult<Value>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(result);
            self.ready.notify_all();
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
        let _ = thread::Builder::new()
            .name(format!("ku-task-{index}"))
            .spawn(move || task_worker_loop(weak));
    }
}

fn task_worker_loop(weak: Weak<TaskRuntimeInner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        if inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let job = {
            let Ok(receiver) = inner.task_rx.lock() else {
                return;
            };
            receiver.recv_timeout(Duration::from_millis(50))
        };
        match job {
            Ok(job) => execute_task_job(&inner, job),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn execute_task_job(inner: &TaskRuntimeInner, job: TaskJob) {
    let previous = CURRENT_TASK_ID.with(|current| {
        let previous = current.get();
        current.set(job.id);
        previous
    });
    let result = catch_unwind(AssertUnwindSafe(job.run))
        .map_err(|_| task_error("panic", "async task panicked"));
    let result = match result {
        Ok(result) => result,
        Err(error) => Ok(error),
    };
    CURRENT_TASK_ID.with(|current| current.set(previous));
    job.state.complete(result);
    if let Ok(mut states) = inner.states.lock() {
        states.remove(&job.id);
    }
    inner.active_tasks.fetch_sub(1, Ordering::AcqRel);
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
        let _ = thread::Builder::new()
            .name(format!("ku-blocking-{index}"))
            .spawn(move || blocking_worker_loop(weak, receiver));
    }
}

fn blocking_worker_loop(weak: Weak<TaskRuntimeInner>, receiver: Arc<Mutex<Receiver<BlockingJob>>>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        if inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let job = {
            let Ok(receiver) = receiver.lock() else {
                return;
            };
            receiver.recv_timeout(Duration::from_millis(50))
        };
        match job {
            Ok(job) => {
                let result = catch_unwind(AssertUnwindSafe(job.run)).map_err(|_| {
                    KuError::structured(
                        crate::error::KuErrorKind::Runtime,
                        "task",
                        "blocking_panic",
                        "blocking pool job panicked",
                        Span::default(),
                    )
                });
                let result = match result {
                    Ok(result) => result,
                    Err(error) => Err(error),
                };
                let _ = job.response.send(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
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

fn task_error(code: &str, message: impl Into<String>) -> Value {
    errors::err("task", code, message)
}
