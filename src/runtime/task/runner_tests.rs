use super::*;
use std::{cell::Cell, num::NonZeroUsize, ops::ControlFlow, rc::Rc};

const WAIT: Duration = Duration::from_secs(3);
const RUNNER_PANIC: &str = "local runner actual fixture panic";

struct ReleaseGate(Option<SyncSender<()>>);

impl ReleaseGate {
    fn pair() -> (Self, Receiver<()>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (Self(Some(tx)), rx)
    }

    fn release(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.try_send(());
        }
    }
}

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.release();
    }
}

fn until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::yield_now();
    }
    true
}

fn listener_payload() -> (Value, Weak<crate::value::HttpListenerLease>) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let id = crate::runtime::http_listener_registry::insert(socket).unwrap();
    let lease = crate::value::HttpListenerLease::new(id);
    let observer = Arc::downgrade(&lease);
    (Value::HttpListenerLease(lease), observer)
}

#[derive(Debug)]
struct ResourceDrop {
    context: Option<CancellationContext>,
    thread: thread::ThreadId,
    task_id: i64,
    socket_released: bool,
}

struct Resource {
    listener: Option<Value>,
    observer: Weak<crate::value::HttpListenerLease>,
    count: Arc<AtomicUsize>,
    receipt: SyncSender<ResourceDrop>,
}

impl Drop for Resource {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        drop(self.listener.take());
        let _ = self.receipt.try_send(ResourceDrop {
            context: current_cleanup_context(),
            thread: thread::current().id(),
            task_id: current_task_id(),
            socket_released: self.observer.upgrade().is_none(),
        });
    }
}

fn resource() -> (
    Resource,
    Weak<crate::value::HttpListenerLease>,
    Arc<AtomicUsize>,
    Receiver<ResourceDrop>,
) {
    let (listener, observer) = listener_payload();
    let count = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::sync_channel(1);
    (
        Resource {
            listener: Some(listener),
            observer: observer.clone(),
            count: Arc::clone(&count),
            receipt: tx,
        },
        observer,
        count,
        rx,
    )
}

#[derive(Debug)]
struct RunnerObservation {
    phase: &'static str,
    step: usize,
    budget: usize,
    rc_address: usize,
    rc_owners: usize,
    task_owners: Option<usize>,
    thread: thread::ThreadId,
    task_id: i64,
    depth: usize,
    status: &'static str,
    terminal: bool,
    queue_consumed: bool,
    accounting_finished: bool,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanicAt {
    None,
    Builder,
    Constructor,
    Dispatch,
    Drop,
}

// Rc is functional state changed across More, not a marker added to a Send
// wrapper. It is created only by the actual claimant's factory invocation.
struct CounterRunner {
    counter: Rc<Cell<usize>>,
    owner: Arc<Mutex<Option<Weak<TaskOwnerLease>>>>,
    observations: SyncSender<RunnerObservation>,
    output: Option<Value>,
    resource: Option<Resource>,
    panic_at: PanicAt,
}

impl CounterRunner {
    fn new(
        owner: Arc<Mutex<Option<Weak<TaskOwnerLease>>>>,
        observations: SyncSender<RunnerObservation>,
        output: Value,
        resource: Resource,
        panic_at: PanicAt,
    ) -> Self {
        let runner = Self {
            counter: Rc::new(Cell::new(0)),
            owner,
            observations,
            output: Some(output),
            resource: Some(resource),
            panic_at,
        };
        runner.observe("construct", 0);
        runner
    }

    fn observe(&self, phase: &'static str, budget: usize) {
        let state = CURRENT_TASK_STATE.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade));
        let _ = self.observations.try_send(RunnerObservation {
            phase,
            step: self.counter.get(),
            budget,
            rc_address: Rc::as_ptr(&self.counter) as usize,
            rc_owners: Rc::strong_count(&self.counter),
            task_owners: self.owner.lock().unwrap().as_ref().map(Weak::strong_count),
            thread: thread::current().id(),
            task_id: current_task_id(),
            depth: await_help_depth(),
            status: state.as_ref().map_or("none", |state| state.status_name()),
            terminal: state.as_ref().is_some_and(|state| state.is_terminal()),
            queue_consumed: state
                .as_ref()
                .is_some_and(|state| state.queue_consumed.load(Ordering::Acquire)),
            accounting_finished: state
                .as_ref()
                .is_some_and(|state| state.accounting_finished.load(Ordering::Acquire)),
            cleanup: current_cleanup_context(),
            execution: current_execution_termination(),
        });
    }
}

impl LocalTaskRunner for CounterRunner {
    fn dispatch(&mut self, budget: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        self.counter.set(self.counter.get() + 1);
        self.observe("dispatch", budget.get());
        if self.panic_at == PanicAt::Dispatch {
            panic!("{RUNNER_PANIC}");
        }
        if self.counter.get() < 3 {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(Ok(self.output.take().unwrap()))
        }
    }
}

impl Drop for CounterRunner {
    fn drop(&mut self) {
        drop(self.resource.take());
        self.observe("drop", 0);
        if self.panic_at == PanicAt::Drop {
            panic!("{RUNNER_PANIC}");
        }
    }
}

struct NullRcRunner(Rc<Cell<usize>>);

impl LocalTaskRunner for NullRcRunner {
    fn dispatch(&mut self, _: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        self.0.set(self.0.get() + 1);
        ControlFlow::Break(Ok(Value::Null))
    }
}

#[derive(Debug)]
struct MarkerReceipt {
    entered:
        Result<(thread::ThreadId, i64, usize, Option<CancellationContext>), mpsc::RecvTimeoutError>,
    id: Option<i64>,
    retired: bool,
    value: Option<i64>,
}

fn marker(runtime: &TaskRuntime) -> MarkerReceipt {
    let (tx, rx) = mpsc::sync_channel(1);
    let task = runtime.spawn(move || {
        let _ = tx.try_send((
            thread::current().id(),
            current_task_id(),
            await_help_depth(),
            current_cleanup_context(),
        ));
        Ok(Value::Int(99))
    });
    let entered = rx.recv_timeout(WAIT);
    let retired = task.as_ref().ok().is_some_and(|task| {
        until(|| task.state.accounting_finished.load(Ordering::Acquire))
            && task.state.queue_consumed.load(Ordering::Acquire)
    });
    let value = task.as_ref().ok().and_then(|task| {
        let slot = task.state.result.lock().unwrap();
        match slot.as_ref() {
            Some(Ok(Value::Int(value))) => Some(*value),
            _ => None,
        }
    });
    let id = task.as_ref().ok().map(TaskHandle::id);
    drop(task);
    MarkerReceipt {
        entered,
        id,
        retired,
        value,
    }
}

fn assert_marker(receipt: MarkerReceipt, worker: thread::ThreadId) {
    let (actual_thread, id, depth, cleanup) = receipt.entered.unwrap();
    assert_eq!(actual_thread, worker);
    assert_ne!(actual_thread, thread::current().id());
    assert_eq!(Some(id), receipt.id);
    assert_eq!(depth, 0);
    assert_eq!(cleanup, None);
    assert!(receipt.retired);
    assert_eq!(receipt.value, Some(99));
}

fn assert_drained(
    shutdown: KuResult<usize>,
    snapshot: KuResult<TaskRuntimeSnapshot>,
    finished: usize,
) {
    shutdown.unwrap();
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, finished);
    assert_eq!(snapshot.cleanup_timeouts, 0);
}

fn task_error_code(value: &Value) -> Option<&str> {
    let Value::Result { ok: false, value } = value else {
        return None;
    };
    let Value::Object(fields) = value.as_ref() else {
        return None;
    };
    match fields.get("code") {
        Some(Value::String(code)) => Some(code),
        _ => None,
    }
}

fn assert_trace(
    observations: &[RunnerObservation],
    task_id: i64,
    worker: thread::ThreadId,
    depth: usize,
) {
    assert_eq!(observations.len(), 5);
    assert_eq!(
        observations
            .iter()
            .map(|item| item.phase)
            .collect::<Vec<_>>(),
        ["construct", "dispatch", "dispatch", "dispatch", "drop"]
    );
    assert_eq!(
        observations
            .iter()
            .map(|item| item.step)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 3]
    );
    let address = observations[0].rc_address;
    assert_ne!(address, 0);
    for item in observations {
        assert_eq!(item.thread, worker);
        assert_eq!(item.task_id, task_id);
        assert_eq!(item.depth, depth);
        assert_eq!(item.rc_address, address);
        assert_eq!(item.rc_owners, 1);
        assert_eq!(item.task_owners, Some(1));
        assert_eq!(item.status, "running");
        assert!(!item.terminal);
        assert!(!item.queue_consumed);
        assert!(!item.accounting_finished);
        assert_eq!(item.cleanup, None);
        assert_eq!(item.execution, None);
        if item.phase == "dispatch" {
            assert_eq!(item.budget, 256);
        }
    }
}

#[test]
fn actual_worker_constructs_non_send_runner_and_resumes_more_with_one_owner() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let host = thread::current().id();
    let owner_slot = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&owner_slot);
    let (log_tx, log_rx) = mpsc::sync_channel(8);
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (mut release, release_rx) = ReleaseGate::pair();
    let (resource, weak, drops, drop_rx) = resource();
    let payload = "runner unique allocation".repeat(1024);
    let allocation = payload.as_ptr() as usize;
    let builder_thread = Arc::new(Mutex::new(None));
    let observed_builder = Arc::clone(&builder_thread);
    let task = runtime
        .spawn_runner_deferred(move || {
            *observed_builder.lock().unwrap() = Some(thread::current().id());
            move || {
                let _ = started_tx.try_send((thread::current().id(), current_task_id()));
                release_rx
                    .recv_timeout(WAIT)
                    .expect("release real claimant factory");
                CounterRunner::new(
                    slot,
                    log_tx,
                    Value::String(payload),
                    resource,
                    PanicAt::None,
                )
            }
        })
        .unwrap();
    let task_id = task.id();
    let owner = Arc::downgrade(&task.owner);
    *owner_slot.lock().unwrap() = Some(owner.clone());
    let started = started_rx.recv_timeout(WAIT);
    let mid = runtime.snapshot();
    let mid_owner = owner.strong_count();
    release.release();
    let retired = until(|| task.state.accounting_finished.load(Ordering::Acquire));
    let outcome = if retired {
        Some(task.await_result())
    } else {
        None
    };
    let actual_drop = drop_rx.recv_timeout(WAIT);
    let observations = log_rx.try_iter().collect::<Vec<_>>();
    let healthy = marker(&runtime);
    drop(task);
    let owner_after = owner.strong_count();
    let shutdown = runtime.cancel_all_and_wait(WAIT);
    let snapshot = runtime.snapshot();

    assert_drained(shutdown, snapshot, 2);
    let (worker, id) = started.unwrap();
    assert_ne!(worker, host);
    assert_eq!(id, task_id);
    assert_eq!(*builder_thread.lock().unwrap(), Some(host));
    assert_eq!(mid_owner, 1);
    assert_eq!(owner_after, 0);
    let mid = mid.unwrap();
    assert_eq!(mid.active_tasks, 1);
    assert_eq!(mid.registered_tasks, 1);
    assert_eq!(mid.queued_tasks, 0);
    assert_eq!(mid.accepted_submissions, 1);
    assert!(retired);
    assert_trace(&observations, task_id, worker, 0);
    let actual_drop = actual_drop.unwrap();
    assert_eq!(actual_drop.thread, worker);
    assert_eq!(actual_drop.task_id, task_id);
    assert_eq!(actual_drop.context, None);
    assert!(actual_drop.socket_released);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(weak.upgrade().is_none());
    let Value::String(delivered) = outcome.unwrap().unwrap() else {
        panic!("actual owned result");
    };
    assert_eq!(delivered.as_ptr() as usize, allocation);
    assert_marker(healthy, worker);
}

#[derive(Debug)]
struct ParentReceipt {
    child_id: i64,
    task_id: i64,
    thread: thread::ThreadId,
    depth: usize,
    status: &'static str,
    wait_edges: usize,
    child_retired: bool,
    child_owner_before: usize,
    child_owner_after: usize,
    allocation: usize,
    delivered: usize,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
}

#[test]
fn real_parent_helper_constructs_local_child_and_moves_owned_allocation() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 6, 0, 0, 6);
    let (resource, weak, drops, drop_rx) = resource();
    let (log_tx, log_rx) = mpsc::sync_channel(8);
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let parent_runtime = runtime.clone();
    let parent = runtime
        .spawn(move || {
            let payload = "actual local helper owned allocation".repeat(1024);
            let allocation = payload.as_ptr() as usize;
            let owner_slot = Arc::new(Mutex::new(None));
            let slot = Arc::clone(&owner_slot);
            let child = parent_runtime.spawn_runner_deferred(|| {
                move || {
                    CounterRunner::new(
                        slot,
                        log_tx,
                        Value::String(payload),
                        resource,
                        PanicAt::None,
                    )
                }
            })?;
            let child_id = child.id();
            let owner = Arc::downgrade(&child.owner);
            *owner_slot.lock().unwrap() = Some(owner.clone());
            let outcome = child.await_result()?;
            let child_retired = child.state.queue_consumed.load(Ordering::Acquire)
                && child.state.accounting_finished.load(Ordering::Acquire);
            let child_owner_before = owner.strong_count();
            drop(child);
            let delivered = match &outcome {
                Value::String(value) => value.as_ptr() as usize,
                _ => 0,
            };
            let state = CURRENT_TASK_STATE
                .with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade).unwrap());
            let _ = receipt_tx.try_send(ParentReceipt {
                child_id,
                task_id: current_task_id(),
                thread: thread::current().id(),
                depth: await_help_depth(),
                status: state.status_name(),
                wait_edges: parent_runtime.snapshot()?.wait_edges,
                child_retired,
                child_owner_before,
                child_owner_after: owner.strong_count(),
                allocation,
                delivered,
                cleanup: current_cleanup_context(),
                execution: current_execution_termination(),
            });
            Ok(outcome)
        })
        .unwrap();
    let parent_id = parent.id();
    let receipt = receipt_rx.recv_timeout(WAIT);
    let actual_drop = drop_rx.recv_timeout(WAIT);
    let observations = log_rx.try_iter().collect::<Vec<_>>();
    let healthy = marker(&runtime);
    let retired = until(|| parent.state.accounting_finished.load(Ordering::Acquire));
    let outcome = if retired {
        Some(parent.await_result())
    } else {
        None
    };
    drop(parent);
    let shutdown = runtime.cancel_all_and_wait(WAIT);
    let snapshot = runtime.snapshot();

    assert_drained(shutdown, snapshot, 3);
    let receipt = receipt.unwrap();
    assert_ne!(receipt.thread, thread::current().id());
    assert_eq!(receipt.task_id, parent_id);
    assert_eq!(receipt.depth, 0);
    assert_eq!(receipt.status, "running");
    assert_eq!(receipt.wait_edges, 0);
    assert!(receipt.child_retired);
    assert_eq!(receipt.child_owner_before, 1);
    assert_eq!(receipt.child_owner_after, 0);
    assert_eq!(receipt.cleanup, None);
    assert_eq!(receipt.execution, None);
    assert_trace(&observations, receipt.child_id, receipt.thread, 1);
    let actual_drop = actual_drop.unwrap();
    assert_eq!(actual_drop.thread, receipt.thread);
    assert_eq!(actual_drop.task_id, receipt.child_id);
    assert_eq!(actual_drop.context, None);
    assert!(actual_drop.socket_released);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(weak.upgrade().is_none());
    assert!(retired);
    let Value::String(delivered) = outcome.unwrap().unwrap() else {
        panic!("actual parent owned result");
    };
    assert_eq!(receipt.delivered, receipt.allocation);
    assert_eq!(delivered.as_ptr() as usize, receipt.allocation);
    assert_marker(healthy, receipt.thread);
}

fn held_worker(
    runtime: &TaskRuntime,
) -> (
    TaskHandle,
    ReleaseGate,
    Result<thread::ThreadId, mpsc::RecvTimeoutError>,
) {
    let (release, rx) = ReleaseGate::pair();
    let (tx, started) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            let _ = tx.try_send(thread::current().id());
            rx.recv_timeout(WAIT).expect("release actual held worker");
            Ok(Value::Null)
        })
        .unwrap();
    (task, release, started.recv_timeout(WAIT))
}

#[test]
fn queued_cancel_and_queue_full_drop_uninvoked_factories_with_original_d_and_accounting() {
    for full in [false, true] {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 4);
        let (held, mut release, worker) = held_worker(&runtime);
        let (resource, weak, drops, drop_rx) = resource();
        let factory_ran = Arc::new(AtomicBool::new(false));
        let invoked = Arc::clone(&factory_ran);
        let original = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now() - Duration::from_millis(1),
        };
        let (task, payload_state, payload_owner, cancelled, after_rejection) = if full {
            // C actually fills the queue. F captures C's sole logical owner.
            // A producer interruption established after admission is inherited
            // by C's real lease Drop when F's Full rejection destroys captures.
            let child = runtime
                .spawn(move || {
                    drop(resource);
                    Ok(Value::Null)
                })
                .unwrap();
            let child_state = Arc::clone(&child.state);
            let child_owner = Arc::downgrade(&child.owner);
            let execution = ExecutionTerminationGuard::enter();
            let task = runtime
                .spawn_runner_deferred(move || {
                    set_execution_termination(original);
                    move || {
                        invoked.store(true, Ordering::Release);
                        drop(child);
                        NullRcRunner(Rc::new(Cell::new(0)))
                    }
                })
                .unwrap();
            let after = runtime.snapshot();
            drop(execution);
            (task, child_state, child_owner, false, after)
        } else {
            let task = runtime
                .spawn_runner_deferred(|| {
                    move || {
                        invoked.store(true, Ordering::Release);
                        drop(resource);
                        NullRcRunner(Rc::new(Cell::new(0)))
                    }
                })
                .unwrap();
            let state = Arc::clone(&task.state);
            let owner = Arc::downgrade(&task.owner);
            let cancelled = task.request_cancel_with(original);
            let after = runtime.snapshot();
            (task, state, owner, cancelled, after)
        };
        let task_state = Arc::clone(&task.state);
        let task_owner = Arc::downgrade(&task.owner);
        let target_owner_before = task_owner.strong_count();
        let payload_context = payload_state.cancellation();
        let terminal_before_worker = payload_state.is_terminal();
        let consumed_before_worker = payload_state.queue_consumed.load(Ordering::Acquire);
        let accounted_before_worker = payload_state.accounting_finished.load(Ordering::Acquire);
        let target_consumed_before = task_state.queue_consumed.load(Ordering::Acquire);
        let target_accounted_before = task_state.accounting_finished.load(Ordering::Acquire);
        let actual_drop = drop_rx.recv_timeout(WAIT);
        release.release();
        // A cancelled node still occupies the real bounded queue until the
        // worker consumes it. Submit the health marker after that retirement,
        // so a legitimate Full result cannot obscure the lifecycle oracle.
        let retired_before_marker = until(|| {
            payload_state.queue_consumed.load(Ordering::Acquire)
                && payload_state.accounting_finished.load(Ordering::Acquire)
        });
        let healthy = marker(&runtime);
        let outcome = task.await_result();
        drop(task);
        drop(held);
        let shutdown = runtime.cancel_all_and_wait(WAIT);
        let snapshot = runtime.snapshot();
        let payload_retired = payload_state.queue_consumed.load(Ordering::Acquire)
            && payload_state.accounting_finished.load(Ordering::Acquire);

        let snapshot_copy = snapshot.as_ref().ok().cloned();
        assert_drained(shutdown, snapshot, 3);
        let worker = worker.unwrap();
        assert_marker(healthy, worker);
        assert!(!factory_ran.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(weak.upgrade().is_none());
        let actual_drop = actual_drop.unwrap();
        assert_eq!(actual_drop.context, Some(original));
        assert!(actual_drop.socket_released);
        assert_eq!(actual_drop.thread, thread::current().id());
        assert_eq!(actual_drop.task_id, 0);
        assert_eq!(payload_context, Some(original));
        assert!(terminal_before_worker);
        assert!(!consumed_before_worker);
        assert!(!accounted_before_worker);
        assert!(!target_consumed_before);
        assert!(!target_accounted_before);
        assert!(retired_before_marker);
        assert!(payload_retired);
        assert_eq!(target_owner_before, 1);
        assert_eq!(task_owner.strong_count(), 0);
        assert_eq!(payload_owner.strong_count(), 0);
        let after = after_rejection.unwrap();
        assert_eq!(after.active_tasks, 2);
        assert_eq!(after.registered_tasks, 2);
        assert_eq!(after.queued_tasks, 1);
        let snapshot = snapshot_copy.unwrap();
        if full {
            assert!(!cancelled);
            assert_eq!(task_error_code(&outcome.unwrap()), Some("queue_full"));
            assert!(!task_state.queue_consumed.load(Ordering::Acquire));
            assert!(!task_state.accounting_finished.load(Ordering::Acquire));
            assert_eq!(snapshot.total_submissions, 4);
            assert_eq!(snapshot.accepted_submissions, 3);
            assert_eq!(snapshot.rejected_task_queue, 1);
        } else {
            assert!(cancelled);
            assert_eq!(outcome.unwrap_err().runtime_termination(), Some(original));
            assert!(task_state.queue_consumed.load(Ordering::Acquire));
            assert!(task_state.accounting_finished.load(Ordering::Acquire));
            assert_eq!(snapshot.total_submissions, 3);
            assert_eq!(snapshot.accepted_submissions, 3);
            assert_eq!(snapshot.rejected_task_queue, 0);
        }
        assert_eq!(current_cleanup_context(), None);
        assert_eq!(current_execution_termination(), None);
    }
}

#[test]
fn builder_panic_stays_spawn_panic_and_claimant_panics_preserve_worker_health() {
    for panic_at in [
        PanicAt::Builder,
        PanicAt::Constructor,
        PanicAt::Dispatch,
        PanicAt::Drop,
    ] {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
        let (resource, weak, drops, drop_rx) = resource();
        let (log_tx, log_rx) = mpsc::sync_channel(8);
        let factory_ran = Arc::new(AtomicBool::new(false));
        let invoked = Arc::clone(&factory_ran);
        let owner = Arc::new(Mutex::new(None));
        let (factory_tx, factory_rx) = mpsc::sync_channel(1);
        let task = runtime
            .spawn_runner_deferred(move || {
                if panic_at == PanicAt::Builder {
                    panic!("{RUNNER_PANIC}");
                }
                move || {
                    invoked.store(true, Ordering::Release);
                    let _ = factory_tx.try_send((thread::current().id(), current_task_id()));
                    if panic_at == PanicAt::Constructor {
                        panic!("{RUNNER_PANIC}");
                    }
                    CounterRunner::new(owner, log_tx, Value::Null, resource, panic_at)
                }
            })
            .unwrap();
        let state = Arc::clone(&task.state);
        let logical_owner = Arc::downgrade(&task.owner);
        let healthy = marker(&runtime);
        let actual_drop = drop_rx.recv_timeout(WAIT);
        let factory = factory_rx.try_recv().ok();
        let observations = log_rx.try_iter().collect::<Vec<_>>();
        let status = task.status();
        let retired = state.queue_consumed.load(Ordering::Acquire)
            && state.accounting_finished.load(Ordering::Acquire);
        let outcome = task.await_result();
        drop(task);
        let shutdown = runtime.cancel_all_and_wait(WAIT);
        let snapshot = runtime.snapshot();
        let snapshot_copy = snapshot.as_ref().ok().cloned();

        assert_drained(
            shutdown,
            snapshot,
            if panic_at == PanicAt::Builder { 1 } else { 2 },
        );
        assert_eq!(logical_owner.strong_count(), 0);
        let code = task_error_code(&outcome.unwrap()).unwrap().to_owned();
        let snapshot = snapshot_copy.unwrap();
        let actual_drop = actual_drop.unwrap();
        assert!(actual_drop.socket_released);
        assert_eq!(actual_drop.context, None);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(weak.upgrade().is_none());
        if panic_at == PanicAt::Builder {
            assert_eq!(code, "spawn_panic");
            assert_eq!(status, "completed");
            assert!(!retired);
            assert!(!factory_ran.load(Ordering::Acquire));
            assert!(factory.is_none());
            assert!(observations.is_empty());
            assert_eq!(actual_drop.task_id, 0);
            assert_eq!(actual_drop.thread, thread::current().id());
            assert_eq!(snapshot.rejected_task_internal, 1);
            let worker = healthy.entered.as_ref().unwrap().0;
            assert_marker(healthy, worker);
        } else {
            assert_eq!(code, "panic");
            assert_eq!(status, "panicked");
            assert!(retired);
            assert!(factory_ran.load(Ordering::Acquire));
            let (worker, id) = factory.unwrap();
            assert_ne!(worker, thread::current().id());
            assert_eq!(id, state.id);
            assert_eq!(actual_drop.task_id, state.id);
            assert_eq!(actual_drop.thread, worker);
            assert_eq!(snapshot.rejected_task_internal, 0);
            assert_marker(healthy, worker);
            if panic_at == PanicAt::Constructor {
                assert!(observations.is_empty());
            } else {
                assert_eq!(observations.first().unwrap().phase, "construct");
                assert_eq!(observations.last().unwrap().phase, "drop");
                assert!(observations
                    .iter()
                    .all(|item| item.thread == worker && item.task_id == state.id));
            }
        }
        assert_eq!(current_cleanup_context(), None);
        assert_eq!(current_execution_termination(), None);
    }
}
