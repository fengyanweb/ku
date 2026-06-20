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

struct TaskState {
    result: Mutex<Option<KuResult<Value>>>,
    ready: Condvar,
    cancelled: AtomicBool,
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

    pub fn spawn<F>(&self, run: F) -> TaskHandle
    where
        F: FnOnce() -> KuResult<Value> + Send + 'static,
    {
        let id = self.inner.next_task_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(TaskState {
            result: Mutex::new(None),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            status: AtomicU8::new(TASK_PENDING),
        });
        let handle = TaskHandle {
            id,
            state: Arc::clone(&state),
            runtime: self.clone(),
        };
        if self.inner.task_workers.load(Ordering::Acquire) == 0 {
            state.complete(Ok(task_error(
                "runtime_stopped",
                "async task runtime has no available workers",
            )));
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
        while self.inner.active_tasks.load(Ordering::Acquire) != 0 {
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
        match self.inner.blocking_tx.try_send(BlockingJob {
            run: Box::new(run),
            response: response_tx,
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
    fn complete(&self, result: KuResult<Value>) {
        self.complete_with_status(result, None);
    }

    fn complete_with_status(&self, result: KuResult<Value>, status: Option<u8>) {
        if let Ok(mut slot) = self.result.lock() {
            if slot.is_none() {
                let status = status.unwrap_or_else(|| {
                    if self.cancelled.load(Ordering::Acquire) {
                        TASK_CANCELLED
                    } else if result.is_err() {
                        TASK_FAILED
                    } else {
                        TASK_COMPLETED
                    }
                });
                *slot = Some(result);
                self.status.store(status, Ordering::Release);
                self.ready.notify_all();
            }
        }
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

    fn set_status(&self, status: u8) {
        if !self.is_cancelled() {
            self.status.store(status, Ordering::Release);
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
            receiver.try_recv()
        };
        match job {
            Ok(job) => {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                execute_task_job(&inner, job)
            }
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
            Err(TryRecvError::Disconnected) => return,
        }
    }
}

fn execute_task_job(inner: &TaskRuntimeInner, job: TaskJob) {
    if job.state.is_cancelled() {
        job.state.complete_with_status(
            Ok(task_error(
                "cancelled",
                format!("task {} was cancelled", job.id),
            )),
            Some(TASK_CANCELLED),
        );
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
    if job.state.is_cancelled() {
        job.state.complete_with_status(
            Ok(task_error(
                "cancelled",
                format!("task {} was cancelled", job.id),
            )),
            Some(TASK_CANCELLED),
        );
    } else {
        job.state
            .complete_with_status(result, panicked.then_some(TASK_PANICKED));
    }
    finish_task_job(inner, job.id);
}

fn finish_task_job(inner: &TaskRuntimeInner, id: i64) {
    if let Ok(mut states) = inner.states.lock() {
        states.remove(&id);
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
            receiver.try_recv()
        };
        match job {
            Ok(job) => {
                if weak.upgrade().is_none() {
                    return;
                }
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
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
            Err(TryRecvError::Disconnected) => return,
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
}
