use super::await_ticket::AwaitPoll;
use super::*;

const WAIT: Duration = Duration::from_secs(3);
const OBSERVER_PANIC: &str = "await observer actual native panic";

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

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "real task did not reach its finite barrier"
        );
        thread::yield_now();
    }
}

fn panic_message(payload: &CleanupPanic) -> Option<&str> {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
}

fn held_worker(runtime: &TaskRuntime) -> (TaskHandle, ReleaseGate) {
    let (release, rx) = ReleaseGate::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            entered_tx.send(thread::current().id()).unwrap();
            rx.recv_timeout(WAIT).expect("release real worker");
            Ok(Value::Null)
        })
        .unwrap();
    assert_ne!(
        entered_rx.recv_timeout(WAIT).unwrap(),
        thread::current().id()
    );
    (task, release)
}

fn drain(runtime: &TaskRuntime, finished: usize) {
    runtime.cancel_all_and_wait(Duration::from_secs(1)).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, finished);
}

fn listener_payload() -> (Value, Weak<crate::value::HttpListenerLease>) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let id = crate::runtime::http_listener_registry::insert(socket).unwrap();
    let lease = crate::value::HttpListenerLease::new(id);
    let observer = Arc::downgrade(&lease);
    (Value::HttpListenerLease(lease), observer)
}

#[test]
fn real_pending_ticket_claims_once_and_moves_ready_owned_allocation_once() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let (mut release, rx) = ReleaseGate::pair();
    let (allocation_tx, allocation_rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            let payload = "actual owned await payload".repeat(1024);
            allocation_tx.send(payload.as_ptr() as usize).unwrap();
            rx.recv_timeout(WAIT).expect("release pending producer");
            Ok(Value::String(payload))
        })
        .unwrap();
    let allocation = allocation_rx.recv_timeout(WAIT).unwrap();
    let state = Arc::clone(&task.state); // Diagnostic state observer, no owner lease.
    let owner = Arc::downgrade(&task.owner);
    let mut ticket = AwaitTicket::begin(task);
    eprintln!(
        "owned await inline header={} wait_header={} logical_owners={} (excludes runtime/state/payload backing)",
        std::mem::size_of::<AwaitTicket>(),
        std::mem::size_of::<TaskWaitGuard>(),
        owner.strong_count()
    );
    assert!(state.awaited.load(Ordering::Acquire));
    assert_eq!(owner.strong_count(), 1);
    assert_eq!(ticket.poll(), AwaitPoll::Pending);
    assert_eq!(ticket.poll(), AwaitPoll::Pending);
    release.release();
    ticket.drive_sync();
    assert_eq!(ticket.poll(), AwaitPoll::Ready);
    assert_eq!(ticket.poll(), AwaitPoll::Ready);
    assert!(state.result.lock().unwrap().is_none());
    let mut observed = false;
    let delivered = ticket
        .finish_with(|result| {
            observed = true;
            let Ok(Value::String(payload)) = result else {
                panic!("actual owned result");
            };
            assert_eq!(payload.as_ptr() as usize, allocation);
            assert_eq!(
                owner.strong_count(),
                1,
                "observer precedes unique owner release"
            );
        })
        .unwrap();
    let Value::String(delivered) = delivered else {
        panic!("actual owned result");
    };
    assert!(observed);
    assert_eq!(delivered.as_ptr() as usize, allocation);
    assert_eq!(owner.strong_count(), 0);
    assert!(state.result.lock().unwrap().is_none());
    assert!(!state.request_cancel());
    drop(delivered);
    drain(&runtime, 1);
}

#[test]
fn borrowed_diagnostic_timeout_keeps_unclaimed_original_then_public_await_moves_it() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let (mut release, rx) = ReleaseGate::pair();
    let (allocation_tx, allocation_rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            let payload = "diagnostic original".repeat(1024);
            allocation_tx.send(payload.as_ptr() as usize).unwrap();
            rx.recv_timeout(WAIT).expect("release diagnostic producer");
            Ok(Value::String(payload))
        })
        .unwrap();
    let allocation = allocation_rx.recv_timeout(WAIT).unwrap();
    let timed = task.await_timeout(Duration::ZERO).unwrap();
    assert!(matches!(timed, Value::Result { ok: false, .. }));
    assert!(!task.state.awaited.load(Ordering::Acquire));
    assert!(!task.state.is_cancelled());
    assert_eq!(Arc::strong_count(&task.owner), 1);
    release.release();
    until(|| task.status() == "completed");
    let Value::String(peek) = task.await_timeout(Duration::ZERO).unwrap() else {
        panic!("real diagnostic peek");
    };
    assert_ne!(peek.as_ptr() as usize, allocation);
    assert!(!task.state.awaited.load(Ordering::Acquire));
    let Value::String(result) = task.await_result().unwrap() else {
        panic!("real public ordinary await");
    };
    assert_eq!(result.as_ptr() as usize, allocation);
    assert!(task.state.awaited.load(Ordering::Acquire));
    assert!(task.state.result.lock().unwrap().is_none());
    assert!(matches!(
        task.await_result().unwrap(),
        Value::Result { ok: false, .. }
    ));
    drop(task);
    drain(&runtime, 1);
}

#[test]
fn borrowed_admission_rejection_preserves_ambient_deadline_and_unclaimed_target() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let (held, mut release) = held_worker(&runtime);
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let witness = Arc::clone(&ran);
    let (payload, weak) = listener_payload();
    let task = runtime
        .spawn(move || {
            witness.store(true, Ordering::Release);
            drop(payload);
            Ok(Value::Null)
        })
        .unwrap();
    let now = Instant::now();
    let target_context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now + Duration::from_secs(2),
    };
    assert!(task.request_cancel_with(target_context));
    assert!(weak.upgrade().is_none());
    let ambient_context = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now + Duration::from_secs(4),
    };
    let ambient = CleanupGuard::enter(ambient_context);
    let before = current_cleanup_context();
    let ordinary = task.await_result();
    let after_ordinary = current_cleanup_context();
    let diagnostic = task.await_timeout(Duration::MAX);
    let after_diagnostic = current_cleanup_context();
    let claimed = task.state.awaited.load(Ordering::Acquire);
    let target_after = task.state.cancellation();
    let owner_count = Arc::strong_count(&task.owner);
    let wait_edges = runtime.snapshot().unwrap().wait_edges;
    drop(ambient);
    drop(task);
    release.release();
    assert_eq!(held.await_result().unwrap(), Value::Null);
    drop(held);
    drain(&runtime, 2);
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(
        ordinary.unwrap_err().runtime_termination(),
        Some(ambient_context)
    );
    assert_eq!(
        diagnostic.unwrap_err().runtime_termination(),
        Some(ambient_context)
    );
    assert_eq!(before, Some(ambient_context));
    assert_eq!(
        after_ordinary, before,
        "borrowed rejection cannot tighten ambient D from an unobserved target"
    );
    assert_eq!(after_diagnostic, before);
    assert!(!claimed);
    assert_eq!(target_after, Some(target_context));
    assert_eq!(owner_count, 1);
    assert_eq!(wait_edges, 0);
    assert_eq!(current_cleanup_context(), None);
}

#[test]
fn real_caller_cancel_beats_ready_result_and_retires_wait_before_observer_owner_release() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(2, 6, 0, 0, 6);
    let (payload, weak) = listener_payload();
    let payload_observer = weak.clone();
    let (mut release, rx) = ReleaseGate::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let worker_runtime = runtime.clone();
    let parent = runtime
        .spawn(move || {
            let child = worker_runtime.spawn(move || Ok(payload)).unwrap();
            until(|| child.status() == "completed");
            let child_state = Arc::clone(&child.state);
            let mut ticket = AwaitTicket::begin(child);
            let parent_state = CURRENT_TASK_STATE
                .with(|current| current.borrow().as_ref().and_then(Weak::upgrade).unwrap());
            entered_tx
                .send((
                    current_task_id(),
                    parent_state.status_name(),
                    worker_runtime.snapshot().unwrap().wait_edges,
                ))
                .unwrap();
            rx.recv_timeout(WAIT)
                .expect("deliver real parent cancellation");
            assert_eq!(ticket.poll(), AwaitPoll::Ready);
            assert!(
                child_state.result.lock().unwrap().is_some(),
                "caller cancellation did not take ready payload"
            );
            let alive_before = payload_observer.upgrade().is_some();
            let mut observed = false;
            let result = ticket.finish_with(|result| {
                observed = result
                    .as_ref()
                    .err()
                    .and_then(KuError::runtime_termination)
                    .is_some();
                assert_eq!(worker_runtime.snapshot().unwrap().wait_edges, 0);
                assert_eq!(parent_state.status_name(), "cancelling");
                assert!(payload_observer.upgrade().is_some());
            });
            receipt_tx
                .send((
                    alive_before,
                    observed,
                    payload_observer.upgrade().is_none(),
                    worker_runtime.snapshot().unwrap().wait_edges,
                ))
                .unwrap();
            result
        })
        .unwrap();
    let (parent_id, status, edges) = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(parent_id, parent.id());
    assert_eq!(status, "waiting");
    assert_eq!(edges, 1);
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now() + WAIT,
    };
    assert!(parent.request_cancel_with(context));
    release.release();
    let receipt = receipt_rx.recv_timeout(WAIT).unwrap();
    let error = parent.await_result().unwrap_err();
    assert_eq!(error.runtime_termination(), Some(context));
    assert_eq!(receipt, (true, true, true, 0));
    assert!(weak.upgrade().is_none());
    drop(parent);
    drain(&runtime, 2);
    queued_poll_native_panic_retires_wait_and_forbids_resume();
}

struct CaptureDrop {
    observed: SyncSender<(bool, Option<CancellationContext>)>,
    observer_ran: Arc<AtomicBool>,
    count: Arc<AtomicUsize>,
}

impl Drop for CaptureDrop {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        let _ = self.observed.try_send((
            self.observer_ran.load(Ordering::Acquire),
            current_cleanup_context(),
        ));
    }
}

struct PanickingCapture {
    witness: CaptureDrop,
    shorter: CancellationContext,
}

impl Drop for PanickingCapture {
    fn drop(&mut self) {
        drop(CleanupGuard::enter(CancellationContext {
            reason: TerminationReason::Cancelled,
            ..self.shorter
        }));
        // Observe the actual field before Rust's unwind drop glue retires it.
        self.witness.observer_ran.store(false, Ordering::Release);
        panic!("queued await capture actual panic");
    }
}

fn queued_poll_native_panic_retires_wait_and_forbids_resume() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now() + WAIT,
    };
    let shorter = CancellationContext {
        cleanup_deadline: Instant::now() - Duration::from_millis(1),
        ..context
    };
    let (payload, weak) = listener_payload();
    let observer_ran = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let capture = (
        PanickingCapture {
            witness: CaptureDrop {
                observed: drop_tx,
                observer_ran: Arc::clone(&observer_ran),
                count: Arc::clone(&drops),
            },
            shorter,
        },
        payload,
    );
    let child_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&child_ran);
    let (mut release, rx) = ReleaseGate::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let parent_runtime = runtime.clone();
    let observed = Arc::clone(&observer_ran);
    let parent = runtime
        .spawn(move || {
            let child = parent_runtime
                .spawn(move || {
                    ran.store(true, Ordering::Release);
                    drop(capture);
                    Ok(Value::Null)
                })
                .unwrap();
            let owner = Arc::downgrade(&child.owner);
            let state = Arc::clone(&child.state);
            let mut ticket = AwaitTicket::begin(child);
            entered_tx
                .send(parent_runtime.snapshot().unwrap().wait_edges)
                .unwrap();
            rx.recv_timeout(WAIT)
                .expect("cancel the actual waiting parent");
            let original = catch_unwind(AssertUnwindSafe(|| ticket.poll()))
                .expect_err("actual queued capture panic must propagate");
            assert_eq!(
                panic_message(&original),
                Some("queued await capture actual panic")
            );
            let retired = parent_runtime.snapshot().unwrap().wait_edges == 0;
            let restored = current_cleanup_context().is_none();
            assert!(
                state.is_terminal(),
                "capture drop completed real cancellation"
            );
            assert_eq!(
                owner.strong_count(),
                1,
                "aborted ticket still owns its target"
            );
            assert!(catch_unwind(AssertUnwindSafe(|| ticket.poll())).is_err());
            let finish = catch_unwind(AssertUnwindSafe(|| {
                ticket.finish_with(|_| observed.store(true, Ordering::Release))
            }));
            assert!(
                finish.is_err(),
                "native panic cannot become a normal outcome"
            );
            receipt_tx
                .send((retired, restored, owner.strong_count()))
                .unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    assert_eq!(entered_rx.recv_timeout(WAIT).unwrap(), 1);
    assert!(parent.request_cancel_with(context));
    release.release();
    let receipt = receipt_rx.recv_timeout(WAIT).unwrap();
    let error = parent.await_result().unwrap_err();
    assert_eq!(error.runtime_termination(), Some(context));
    assert_eq!(receipt, (true, true, 0));
    assert_eq!(drop_rx.recv_timeout(WAIT).unwrap(), (false, Some(shorter)));
    assert!(!observer_ran.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(!child_ran.load(Ordering::Acquire));
    assert!(weak.upgrade().is_none());
    drop(parent);
    drain(&runtime, 2);
}

#[test]
fn begin_rejection_retains_queued_owner_until_observation_and_observer_panic_keeps_short_d() {
    for panic_observer in [false, true] {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
        let (held, mut release) = held_worker(&runtime);
        let (payload, weak) = listener_payload();
        let observer_ran = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let capture = (
            CaptureDrop {
                observed: drop_tx,
                observer_ran: Arc::clone(&observer_ran),
                count: Arc::clone(&drops),
            },
            payload,
        );
        let child_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&child_ran);
        let child = runtime
            .spawn(move || {
                ran.store(true, Ordering::Release);
                drop(capture);
                Ok(Value::Null)
            })
            .unwrap();
        let state = Arc::clone(&child.state);
        let context = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now() - Duration::from_millis(1),
        };
        let shorter = CancellationContext {
            cleanup_deadline: context.cleanup_deadline - Duration::from_millis(1),
            ..context
        };
        let ambient = CleanupGuard::enter(context);
        let mut ticket = AwaitTicket::begin(child);
        assert_eq!(ticket.poll(), AwaitPoll::Ready);
        assert!(
            !state.awaited.load(Ordering::Acquire),
            "operation rejection precedes claim"
        );
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(ambient);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            ticket.finish_with(|result| {
                assert_eq!(
                    result.as_ref().err().unwrap().runtime_termination(),
                    Some(context)
                );
                observer_ran.store(true, Ordering::Release);
                drop(CleanupGuard::enter(CancellationContext {
                    reason: TerminationReason::Cancelled,
                    ..shorter
                }));
                if panic_observer {
                    panic!("{OBSERVER_PANIC}");
                }
            })
        }));
        let actual_drop = drop_rx.recv_timeout(WAIT);
        let cleanup_after = current_cleanup_context();
        release.release();
        drain(&runtime, 2);
        drop(held);
        assert_eq!(actual_drop.unwrap(), (true, Some(shorter)));
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!child_ran.load(Ordering::Acquire));
        assert!(weak.upgrade().is_none());
        assert_eq!(cleanup_after, None);
        if panic_observer {
            let panic = outcome.expect_err("original observer panic propagates");
            assert_eq!(panic_message(&panic), Some(OBSERVER_PANIC));
        } else {
            assert_eq!(
                outcome.unwrap().unwrap_err().runtime_termination(),
                Some(shorter)
            );
        }
    }
}

#[test]
fn observer_panic_releases_both_consumed_target_and_unique_owned_ready_payload_at_saved_d() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(2, 6, 0, 0, 6);
    let (held, mut held_release) = held_worker(&runtime);
    let (mut producer_release, producer_rx) = ReleaseGate::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (payload, weak) = listener_payload();
    let observer_ran = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let capture = (
        CaptureDrop {
            observed: drop_tx,
            observer_ran: Arc::clone(&observer_ran),
            count: Arc::clone(&drops),
        },
        payload,
    );
    let child_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&child_ran);
    let producer_runtime = runtime.clone();
    let producer = runtime
        .spawn(move || {
            entered_tx.send(thread::current().id()).unwrap();
            producer_rx
                .recv_timeout(WAIT)
                .expect("release real producer");
            let child = producer_runtime
                .spawn(move || {
                    ran.store(true, Ordering::Release);
                    drop(capture);
                    Ok(Value::Null)
                })
                .unwrap();
            let mut captures = crate::env::Env::new();
            captures
                .define_owned("child".into(), Value::Task(child), false, Span::default())
                .unwrap();
            Ok(Value::Function {
                params: Vec::new(),
                param_modes: Vec::new(),
                body: Vec::new(),
                captures,
                self_name: None,
                is_async: false,
            })
        })
        .unwrap();
    assert_ne!(
        entered_rx.recv_timeout(WAIT).unwrap(),
        thread::current().id()
    );
    // Both actual workers are now running. Hold only the real receiver lock
    // to retain the newly queued child until its owner payload is discarded.
    // No TaskState, result, completion or accounting field is manufactured.
    let queue = runtime.inner.task_rx.lock().unwrap();
    producer_release.release();
    until(|| producer.status() == "completed");
    let producer_state = Arc::clone(&producer.state);
    let mut ticket = AwaitTicket::begin(producer);
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now() + WAIT,
    };
    let shorter = CancellationContext {
        cleanup_deadline: Instant::now() - Duration::from_millis(1),
        ..context
    };
    let ambient = CleanupGuard::enter(context);
    assert_eq!(ticket.poll(), AwaitPoll::Ready);
    assert!(producer_state.result.lock().unwrap().is_none());
    assert!(weak.upgrade().is_some());
    drop(ambient);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        ticket.finish_with(|result| {
            assert!(matches!(result, Ok(Value::Function { .. })));
            observer_ran.store(true, Ordering::Release);
            drop(CleanupGuard::enter(CancellationContext {
                reason: TerminationReason::Cancelled,
                ..shorter
            }));
            panic!("{OBSERVER_PANIC}");
        })
    }));
    let actual_drop = drop_rx.recv_timeout(WAIT);
    let cleanup_after = current_cleanup_context();
    drop(queue);
    held_release.release();
    drain(&runtime, 3);
    drop(held);
    let panic = panic.expect_err("original observer panic propagates");
    assert_eq!(panic_message(&panic), Some(OBSERVER_PANIC));
    assert_eq!(actual_drop.unwrap(), (true, Some(shorter)));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(!child_ran.load(Ordering::Acquire));
    assert!(weak.upgrade().is_none());
    assert!(producer_state.result.lock().unwrap().is_none());
    assert_eq!(cleanup_after, None);
}

const OWNED_HELPER_COMPLETION_PANIC: &str = "owned helper losing payload tighter D native panic";

#[derive(Debug)]
struct OwnedHelperDropReceipt {
    context: Option<CancellationContext>,
    socket_released: bool,
    id: i64,
    thread: thread::ThreadId,
    help_depth: usize,
}

struct OwnedHelperTightPanicProbe {
    shorter: CancellationContext,
    listener: Option<Value>,
    listener_observer: Weak<crate::value::HttpListenerLease>,
    count: Arc<AtomicUsize>,
    receipt: SyncSender<OwnedHelperDropReceipt>,
}

impl Drop for OwnedHelperTightPanicProbe {
    fn drop(&mut self) {
        // Ask for a different cause so actual nested TLS must retain the
        // completion's first cause and tighten only its absolute deadline.
        let _short = CleanupGuard::enter(CancellationContext {
            reason: TerminationReason::Cancelled,
            ..self.shorter
        });
        self.count.fetch_add(1, Ordering::AcqRel);
        drop(self.listener.take());
        let _ = self.receipt.try_send(OwnedHelperDropReceipt {
            context: current_cleanup_context(),
            socket_released: self.listener_observer.upgrade().is_none(),
            id: current_task_id(),
            thread: thread::current().id(),
            help_depth: await_help_depth(),
        });
        panic!("{OWNED_HELPER_COMPLETION_PANIC}");
    }
}

struct OwnedHelperReady {
    target: Arc<TaskState>,
    nested: Arc<TaskState>,
    nested_owner: Weak<TaskOwnerLease>,
    id: i64,
    thread: thread::ThreadId,
    help_depth: usize,
}

impl std::fmt::Debug for OwnedHelperReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedHelperReady")
            .field("target_id", &self.target.id)
            .field("nested_id", &self.nested.id)
            .field("nested_owners", &self.nested_owner.strong_count())
            .field("id", &self.id)
            .field("thread", &self.thread)
            .field("help_depth", &self.help_depth)
            .finish()
    }
}

#[derive(Debug)]
struct OwnedHelperAwaitReceipt {
    panic: Option<String>,
    id: i64,
    thread: thread::ThreadId,
    status: &'static str,
    help_depth: usize,
    wait_edges: usize,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
    owner_before: usize,
    owner_after: usize,
    observer_ran: bool,
    repoll_rejected: bool,
    finish_rejected: bool,
    target_context_before_owner_release: Option<CancellationContext>,
    target_error_context_before_owner_release: Option<CancellationContext>,
    target_terminal: bool,
    target_retired: bool,
    target_result_released: bool,
    cleanup_after_owner_release: Option<CancellationContext>,
    execution_after_owner_release: Option<CancellationContext>,
}

#[test]
fn owned_helper_completion_panic_preserves_tighter_d_and_retires_aborted_ticket() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 6, 0, 0, 6);
    let now = Instant::now();
    let original = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now - Duration::from_millis(1),
    };
    let shorter = CancellationContext {
        cleanup_deadline: original.cleanup_deadline - Duration::from_millis(1),
        ..original
    };
    let (listener, listener_observer) = listener_payload();
    let actual_drops = Arc::new(AtomicUsize::new(0));
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let probe = OwnedHelperTightPanicProbe {
        shorter,
        listener: Some(listener),
        listener_observer: listener_observer.clone(),
        count: Arc::clone(&actual_drops),
        receipt: drop_tx,
    };
    let nested_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&nested_ran);
    let (mut release, release_rx) = ReleaseGate::pair();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let parent_runtime = runtime.clone();
    let parent = runtime
        .spawn(move || {
            let target_runtime = parent_runtime.clone();
            let target = parent_runtime
                .spawn(move || {
                    let nested = target_runtime
                        .spawn(move || {
                            ran.store(true, Ordering::Release);
                            drop(probe);
                            Ok(Value::Null)
                        })
                        .unwrap();
                    let target_state = CURRENT_TASK_STATE
                        .with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade).unwrap());
                    let nested_state = Arc::clone(&nested.state);
                    let nested_owner = Arc::downgrade(&nested.owner);
                    let mut captures = crate::env::Env::new();
                    captures
                        .define_owned("nested".into(), Value::Task(nested), false, Span::default())
                        .unwrap();
                    ready_tx
                        .send(OwnedHelperReady {
                            target: target_state,
                            nested: nested_state,
                            nested_owner,
                            id: current_task_id(),
                            thread: thread::current().id(),
                            help_depth: await_help_depth(),
                        })
                        .unwrap();
                    release_rx
                        .recv_timeout(WAIT)
                        .expect("release the actually helped target");
                    // B loses this real Owned Function payload to the host's
                    // cancellation. Its capture's last C owner drops C's
                    // queued callback; no state or completion is manufactured.
                    Ok(Value::Function {
                        params: Vec::new(),
                        param_modes: Vec::new(),
                        body: Vec::new(),
                        captures,
                        self_name: None,
                        is_async: false,
                    })
                })
                .unwrap();
            let target_state = Arc::clone(&target.state);
            let owner = Arc::downgrade(&target.owner);
            let mut ticket = AwaitTicket::begin(target);
            let outcome = catch_unwind(AssertUnwindSafe(|| ticket.drive_sync()));
            let panic = outcome
                .as_ref()
                .err()
                .and_then(panic_message)
                .map(str::to_owned);
            let state = CURRENT_TASK_STATE
                .with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade).unwrap());
            let owner_before = owner.strong_count();
            let context_before = target_state.cancellation();
            let error_context_before = target_state
                .result
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .and_then(KuError::runtime_termination);
            let target_terminal = target_state.is_terminal();
            let target_retired = target_state.queue_consumed.load(Ordering::Acquire)
                && target_state.accounting_finished.load(Ordering::Acquire);
            let id = current_task_id();
            let status = state.status_name();
            let help_depth = await_help_depth();
            let wait_edges = parent_runtime.snapshot().unwrap().wait_edges;
            let cleanup = current_cleanup_context();
            let execution = current_execution_termination();
            let repoll_rejected = catch_unwind(AssertUnwindSafe(|| ticket.poll())).is_err();
            let mut observer_ran = false;
            let finish_rejected = catch_unwind(AssertUnwindSafe(|| {
                ticket.finish_with(|_| observer_ran = true)
            }))
            .is_err();
            // At this point B retains only its terminal KuError. Its losing
            // Function/C resources already dropped in helper completion, so
            // this fixture does not claim a resource witness for final ticket
            // owner-release D by inspecting a private context getter.
            receipt_tx
                .send(OwnedHelperAwaitReceipt {
                    panic,
                    id,
                    thread: thread::current().id(),
                    status,
                    help_depth,
                    wait_edges,
                    cleanup,
                    execution,
                    owner_before,
                    owner_after: owner.strong_count(),
                    observer_ran,
                    repoll_rejected,
                    finish_rejected,
                    target_context_before_owner_release: context_before,
                    target_error_context_before_owner_release: error_context_before,
                    target_terminal,
                    target_retired,
                    target_result_released: target_state.result.lock().unwrap().is_none(),
                    cleanup_after_owner_release: current_cleanup_context(),
                    execution_after_owner_release: current_execution_termination(),
                })
                .unwrap();
            Ok(Value::Int(55))
        })
        .unwrap();
    let parent_id = parent.id();
    let ready = ready_rx.recv_timeout(WAIT);
    let cancelled = ready
        .as_ref()
        .ok()
        .is_some_and(|ready| ready.target.request_cancel_with(original));
    let nested_owner_before = ready
        .as_ref()
        .ok()
        .map(|ready| ready.nested_owner.strong_count());
    // Release before any host assertion: the gate also releases on unwind,
    // and its worker-side wait is bounded by WAIT on every failure path.
    release.release();
    let receipt = receipt_rx.recv_timeout(WAIT);
    let drop_receipt = drop_rx.recv_timeout(WAIT);
    let (marker_tx, marker_rx) = mpsc::sync_channel(1);
    let marker = runtime.spawn(move || {
        marker_tx
            .send((
                thread::current().id(),
                current_task_id(),
                await_help_depth(),
            ))
            .unwrap();
        Ok(Value::Int(99))
    });
    let marker_receipt = marker_rx.recv_timeout(WAIT);
    let retired_by = Instant::now() + WAIT;
    if let Ok(marker) = &marker {
        while !marker.state.accounting_finished.load(Ordering::Acquire)
            && Instant::now() < retired_by
        {
            thread::yield_now();
        }
    }
    let parent_value = parent
        .state
        .result
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|result| {
            if let Ok(Value::Int(value)) = result {
                Some(*value)
            } else {
                None
            }
        });
    let marker_value = marker.as_ref().ok().and_then(|marker| {
        marker
            .state
            .result
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|result| {
                if let Ok(Value::Int(value)) = result {
                    Some(*value)
                } else {
                    None
                }
            })
    });
    let target_context = ready
        .as_ref()
        .ok()
        .and_then(|ready| ready.target.cancellation());
    let nested_context = ready
        .as_ref()
        .ok()
        .and_then(|ready| ready.nested.cancellation());
    let target_retired = ready.as_ref().ok().is_some_and(|ready| {
        ready.target.is_terminal()
            && ready.target.queue_consumed.load(Ordering::Acquire)
            && ready.target.accounting_finished.load(Ordering::Acquire)
    });
    let nested_retired = ready.as_ref().ok().is_some_and(|ready| {
        ready.nested.is_terminal()
            && ready.nested.queue_consumed.load(Ordering::Acquire)
            && ready.nested.accounting_finished.load(Ordering::Acquire)
    });
    let nested_owner_after = ready
        .as_ref()
        .ok()
        .map(|ready| ready.nested_owner.strong_count());
    let socket_released = listener_observer.upgrade().is_none();
    let shutdown = runtime.cancel_all_and_wait(WAIT);
    let snapshot = runtime.snapshot();
    let marker_id = marker.as_ref().ok().map(TaskHandle::id);
    eprintln!("owned_helper_completion ready={ready:?} cancelled={cancelled} drop={drop_receipt:?} receipt={receipt:?} target_context={target_context:?} nested_context={nested_context:?} target_retired={target_retired} nested_retired={nested_retired} marker={marker_receipt:?} parent_value={parent_value:?} marker_value={marker_value:?} shutdown={shutdown:?} snapshot={snapshot:?}");
    drop(marker);
    drop(parent);

    let ready = ready.unwrap();
    let receipt = receipt.unwrap();
    let actual_drop = drop_receipt.unwrap();
    let marker = marker_receipt.unwrap();
    let snapshot = snapshot.unwrap();
    shutdown.unwrap();
    assert!(cancelled);
    assert_eq!(ready.help_depth, 1, "B actually runs through A's helper");
    assert_eq!(ready.id, ready.target.id);
    assert_eq!(nested_owner_before, Some(1));
    assert_eq!(
        receipt.panic.as_deref(),
        Some(OWNED_HELPER_COMPLETION_PANIC)
    );
    assert_eq!(receipt.id, parent_id);
    assert_eq!(receipt.thread, ready.thread);
    assert_eq!(receipt.status, "running");
    assert_eq!(receipt.help_depth, 0);
    assert_eq!(receipt.wait_edges, 0);
    assert_eq!(receipt.cleanup, None);
    assert_eq!(receipt.execution, None);
    assert_eq!(receipt.owner_before, 1);
    assert_eq!(receipt.owner_after, 0);
    assert!(receipt.repoll_rejected);
    assert!(receipt.finish_rejected);
    assert!(!receipt.observer_ran);
    assert!(receipt.target_terminal);
    assert!(receipt.target_retired);
    assert!(receipt.target_result_released);
    assert_eq!(receipt.cleanup_after_owner_release, None);
    assert_eq!(receipt.execution_after_owner_release, None);
    assert_eq!(actual_drop.context, Some(shorter));
    assert!(actual_drop.socket_released);
    assert_eq!(
        actual_drop.id, parent_id,
        "completion restored A's Task identity before actual losing-payload drop"
    );
    assert_eq!(actual_drop.thread, ready.thread);
    assert_eq!(actual_drop.help_depth, 1);
    assert_eq!(actual_drops.load(Ordering::Acquire), 1);
    assert!(!nested_ran.load(Ordering::Acquire));
    assert!(socket_released);
    assert_eq!(nested_owner_after, Some(0));
    assert!(target_retired);
    assert!(nested_retired);
    assert_eq!(marker.0, ready.thread, "same real worker remains healthy");
    assert_eq!(Some(marker.1), marker_id);
    assert_eq!(marker.2, 0);
    assert_eq!(parent_value, Some(55));
    assert_eq!(marker_value, Some(99));
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, 4);
    assert_eq!(snapshot.cleanup_timeouts, 0);
    // The actual Drop witness above must retain the earliest cause, and the
    // real Task states must persist its tighter deadline before helper unwind.
    // Persist both states and the already-published typed error before unwind.
    assert_eq!(receipt.target_context_before_owner_release, Some(shorter));
    assert_eq!(
        receipt.target_error_context_before_owner_release,
        Some(shorter)
    );
    assert_eq!(target_context, Some(shorter));
    assert_eq!(nested_context, Some(shorter));
}
