use super::*;

const TEST_WAIT: Duration = Duration::from_secs(3);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(50);
const PANIC_MESSAGE: &str = "submission frame destructor fixture panic";

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

fn held_worker(runtime: &TaskRuntime) -> (TaskHandle, ReleaseGate, thread::ThreadId) {
    let (release, release_rx) = ReleaseGate::pair();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let handle = runtime
        .spawn(move || {
            started_tx.send(thread::current().id()).unwrap();
            release_rx
                .recv_timeout(TEST_WAIT)
                .expect("release submission fixture worker");
            Ok(Value::Null)
        })
        .unwrap();
    let worker = started_rx
        .recv_timeout(TEST_WAIT)
        .expect("actual single worker entered");
    (handle, release, worker)
}

// Find a state that the real admission path has already registered. This only
// retains its control block; no TaskHandle or logical result owner is cloned.
fn building_state(runtime: &TaskRuntime, previous_ids: &[i64]) -> Arc<TaskState> {
    runtime
        .inner
        .states
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .find(|state| !previous_ids.contains(&state.id))
        .expect("real spawn_deferred registered its new state before build")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureObservation {
    name: &'static str,
    cleanup: Option<CancellationContext>,
    status: &'static str,
    terminal: bool,
    queue_consumed: bool,
    accounting_finished: bool,
}

struct CapturedProbe {
    name: &'static str,
    panics: bool,
    state: Weak<TaskState>,
    observed: SyncSender<CaptureObservation>,
}

impl Drop for CapturedProbe {
    fn drop(&mut self) {
        let state = self.state.upgrade().unwrap();
        self.observed
            .try_send(CaptureObservation {
                name: self.name,
                cleanup: current_cleanup_context(),
                status: state.status_name(),
                terminal: state.is_terminal(),
                queue_consumed: state.queue_consumed.load(Ordering::Acquire),
                accounting_finished: state.accounting_finished.load(Ordering::Acquire),
            })
            .unwrap();
        assert!(!self.panics, "{PANIC_MESSAGE}");
    }
}

fn captures(
    state: &Arc<TaskState>,
    observed: SyncSender<CaptureObservation>,
) -> (CapturedProbe, CapturedProbe) {
    (
        CapturedProbe {
            name: "panic",
            panics: true,
            state: Arc::downgrade(state),
            observed: observed.clone(),
        },
        CapturedProbe {
            name: "tail",
            panics: false,
            state: Arc::downgrade(state),
            observed,
        },
    )
}

#[derive(Debug)]
struct StateReceipt {
    status: &'static str,
    terminal: bool,
    queue_consumed: bool,
    accounting_finished: bool,
    owner_released: bool,
    cancellation: Option<CancellationContext>,
    result_present: bool,
}

impl StateReceipt {
    fn observe(state: &TaskState) -> Self {
        Self {
            status: state.status_name(),
            terminal: state.is_terminal(),
            queue_consumed: state.queue_consumed.load(Ordering::Acquire),
            accounting_finished: state.accounting_finished.load(Ordering::Acquire),
            owner_released: state.owner_released.load(Ordering::Acquire),
            cancellation: state.cancellation(),
            result_present: state.result.lock().unwrap().is_some(),
        }
    }
}

#[derive(Debug)]
struct WorkerReceipt {
    thread: thread::ThreadId,
    task_id: i64,
    state: Option<StateReceipt>,
    cleanup_tls: Option<CancellationContext>,
    execution_tls: Option<CancellationContext>,
}

#[derive(Debug)]
struct MarkerObservation {
    task_id: Option<i64>,
    admission_error: Option<KuError>,
    receipt: Result<WorkerReceipt, mpsc::RecvTimeoutError>,
    retired: bool,
    value: Option<i64>,
}

fn marker(
    runtime: &TaskRuntime,
    state: Option<Arc<TaskState>>,
    finished: usize,
) -> MarkerObservation {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = runtime.spawn(move || {
        tx.send(WorkerReceipt {
            thread: thread::current().id(),
            task_id: current_task_id(),
            state: state
                .as_ref()
                .map(|state| StateReceipt::observe(state.as_ref())),
            cleanup_tls: current_cleanup_context(),
            execution_tls: current_execution_termination(),
        })
        .unwrap();
        Ok(Value::Int(99))
    });
    let receipt = rx.recv_timeout(TEST_WAIT);
    if receipt.is_ok() {
        let deadline = Instant::now() + TEST_WAIT;
        if let Ok(handle) = &handle {
            while (!handle.state.accounting_finished.load(Ordering::Acquire)
                || runtime.inner.finished_tasks.load(Ordering::Acquire) < finished)
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
        }
    }
    let retired = handle.as_ref().ok().is_some_and(|handle| {
        handle.state.queue_consumed.load(Ordering::Acquire)
            && handle.state.accounting_finished.load(Ordering::Acquire)
    });
    let value = handle.as_ref().ok().and_then(|handle| {
        let result = handle.state.result.lock().unwrap();
        match result.as_ref() {
            Some(Ok(Value::Int(value))) => Some(*value),
            _ => None,
        }
    });
    let task_id = handle.as_ref().ok().map(TaskHandle::id);
    // The receipt contains no live owner. Release the real marker handle before
    // the caller records shutdown and makes semantic assertions.
    let admission_error = handle.err();
    MarkerObservation {
        task_id,
        admission_error,
        receipt,
        retired,
        value,
    }
}

fn panic_text(panic: &(dyn std::any::Any + Send)) -> Option<String> {
    panic.downcast_ref::<String>().cloned().or_else(|| {
        panic
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
    })
}

fn assert_marker(marker: MarkerObservation, worker: thread::ThreadId) -> StateReceipt {
    let receipt = marker
        .receipt
        .expect("subsequent callback ran on the actual single worker");
    assert_eq!(receipt.thread, worker);
    assert_ne!(receipt.thread, thread::current().id());
    assert!(marker.admission_error.is_none());
    assert_eq!(Some(receipt.task_id), marker.task_id);
    assert_ne!(receipt.task_id, 0);
    assert_eq!(receipt.cleanup_tls, None);
    assert_eq!(receipt.execution_tls, None);
    assert!(marker.retired);
    assert_eq!(marker.value, Some(99));
    receipt
        .state
        .expect("actual admission state remains observable without a logical owner")
}

fn assert_never_enqueued(state: &StateReceipt) {
    assert!(state.terminal);
    assert!(
        !state.queue_consumed,
        "an unsubmitted frame has no worker queue ACK"
    );
    assert!(
        !state.accounting_finished,
        "reservation rollback must not invent a finished task"
    );
    assert!(state.owner_released);
    assert!(
        !state.result_present,
        "the unwound producer's final owner must release its result"
    );
}

#[test]
fn cancelled_during_build_capture_panic_publishes_terminal_state_and_releases_real_admission() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let (held, mut release, worker) = held_worker(&runtime);
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    };
    let (tx, rx) = mpsc::sync_channel(2);
    let ran = Arc::new(AtomicBool::new(false));
    let frame_ran = Arc::clone(&ran);
    let mut observer = None;
    let mut registered = None;
    let mut cancelled_count = None;
    let attempt = catch_unwind(AssertUnwindSafe(|| {
        runtime.spawn_deferred(|| {
            let state = building_state(&runtime, &[held.id()]);
            observer = Some(Arc::clone(&state));
            registered = Some(runtime.snapshot().unwrap());
            // Actual cancellation occurs while admission exists and before there
            // is a pending runnable. The builder still returns its complete frame.
            cancelled_count = Some(runtime.cancel_all_with(context));
            let captures = captures(&state, tx);
            move || {
                frame_ran.store(true, Ordering::Release);
                drop(captures);
                Ok(Value::Null)
            }
        })
    }));
    let propagated = attempt.is_err();
    let original_panic = attempt
        .as_ref()
        .err()
        .and_then(|panic| panic_text(panic.as_ref()));
    let first_drop = rx.recv_timeout(TEST_WAIT);
    let tail_drop = rx.recv_timeout(TEST_WAIT);
    let after_panic = observer
        .as_ref()
        .map(|state| StateReceipt::observe(state.as_ref()));
    let cleanup_after_panic = current_cleanup_context();
    release.release();
    let marker = marker(&runtime, observer.as_ref().map(Arc::clone), 2);
    let before_shutdown = runtime.snapshot();
    let shutdown = runtime.cancel_all_and_wait(SHUTDOWN_WAIT);
    let final_snapshot = runtime.snapshot();
    eprintln!(
        "submission_install_panic registered={registered:?} cancelled_count={cancelled_count:?} \
         propagated={propagated}/{original_panic:?} drops={first_drop:?},{tail_drop:?} \
         after_panic={after_panic:?} marker={marker:?} \
         before_shutdown={before_shutdown:?} shutdown={shutdown:?} final_snapshot={final_snapshot:?}"
    );
    drop(attempt);
    drop(observer);
    drop(held);
    drop(runtime);

    assert!(propagated);
    assert_eq!(original_panic.as_deref(), Some(PANIC_MESSAGE));
    let registered = registered.expect("real build admission was recorded");
    assert_eq!(registered.active_tasks, 2);
    assert_eq!(registered.registered_tasks, 2);
    assert_eq!(cancelled_count, Some(2));
    for (event, name) in [(first_drop, "panic"), (tail_drop, "tail")] {
        assert_eq!(
            event.unwrap(),
            CaptureObservation {
                name,
                cleanup: Some(context),
                status: "cancelling",
                terminal: false,
                queue_consumed: false,
                accounting_finished: false,
            }
        );
    }
    assert_eq!(cleanup_after_panic, None);
    assert!(!ran.load(Ordering::Acquire));
    let state = after_panic.expect("real producer state survived as a control-block observer");
    assert_never_enqueued(&state);
    assert_eq!(state.status, "timed_out");
    assert_eq!(state.cancellation, Some(context));
    let state = assert_marker(marker, worker);
    assert_never_enqueued(&state);
    assert_eq!(state.status, "timed_out");
    assert_eq!(state.cancellation, Some(context));
    shutdown.expect("uninstalled-frame panic must roll back its real admission reservation");
    let snapshot = final_snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, 2);
    assert_eq!(snapshot.accepted_submissions, 2);
    assert_eq!(snapshot.cleanup_timeouts, 0);
}

#[test]
fn full_queue_capture_panic_preserves_completed_rejection_without_synthetic_accounting() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 4);
    let (held, mut release, worker) = held_worker(&runtime);
    let (queued_tx, queued_rx) = mpsc::sync_channel(1);
    let queued = runtime
        .spawn(move || {
            queued_tx.send(thread::current().id()).unwrap();
            Ok(Value::Int(13))
        })
        .unwrap();
    let (tx, rx) = mpsc::sync_channel(2);
    let ran = Arc::new(AtomicBool::new(false));
    let frame_ran = Arc::clone(&ran);
    let mut observer = None;
    let mut registered = None;
    let attempt = catch_unwind(AssertUnwindSafe(|| {
        runtime.spawn_deferred(|| {
            let state = building_state(&runtime, &[held.id(), queued.id()]);
            observer = Some(Arc::clone(&state));
            registered = Some(runtime.snapshot().unwrap());
            let captures = captures(&state, tx);
            move || {
                frame_ran.store(true, Ordering::Release);
                drop(captures);
                Ok(Value::Null)
            }
        })
    }));
    let propagated = attempt.is_err();
    let original_panic = attempt
        .as_ref()
        .err()
        .and_then(|panic| panic_text(panic.as_ref()));
    let first_drop = rx.recv_timeout(TEST_WAIT);
    let tail_drop = rx.recv_timeout(TEST_WAIT);
    let after_panic = observer
        .as_ref()
        .map(|state| StateReceipt::observe(state.as_ref()));
    let after_rejection = runtime.snapshot();
    let cleanup_after_panic = current_cleanup_context();
    release.release();
    let queued_receipt = queued_rx.recv_timeout(TEST_WAIT);
    if queued_receipt.is_ok() {
        let deadline = Instant::now() + TEST_WAIT;
        while !queued.state.accounting_finished.load(Ordering::Acquire) && Instant::now() < deadline
        {
            thread::yield_now();
        }
    }
    let queued_retired = queued.state.queue_consumed.load(Ordering::Acquire)
        && queued.state.accounting_finished.load(Ordering::Acquire);
    let marker = marker(&runtime, observer.as_ref().map(Arc::clone), 3);
    let shutdown = runtime.cancel_all_and_wait(SHUTDOWN_WAIT);
    let final_snapshot = runtime.snapshot();
    eprintln!(
        "submission_full_panic registered={registered:?} propagated={propagated}/{original_panic:?} \
         drops={first_drop:?},{tail_drop:?} after_panic={after_panic:?} \
         after_rejection={after_rejection:?} queued={queued_receipt:?}/{queued_retired} \
         marker={marker:?} shutdown={shutdown:?} final_snapshot={final_snapshot:?}"
    );
    drop(attempt);
    drop(observer);
    drop(queued);
    drop(held);
    drop(runtime);

    assert!(propagated);
    assert_eq!(original_panic.as_deref(), Some(PANIC_MESSAGE));
    let registered = registered.expect("Full attempt was admitted before trying the channel");
    assert_eq!(registered.active_tasks, 3);
    assert_eq!(registered.registered_tasks, 3);
    assert_eq!(registered.queued_tasks, 1);
    for (event, name) in [(first_drop, "panic"), (tail_drop, "tail")] {
        assert_eq!(
            event.unwrap(),
            CaptureObservation {
                name,
                cleanup: None,
                status: "pending",
                terminal: false,
                queue_consumed: false,
                accounting_finished: false,
            }
        );
    }
    // Full has already rolled back active/registry admission in the original
    // path. Its missing terminal outcome must not be misreported as a leak.
    let rejected = after_rejection.unwrap();
    assert_eq!(rejected.active_tasks, 2);
    assert_eq!(rejected.registered_tasks, 2);
    assert_eq!(rejected.queued_tasks, 1);
    assert_eq!(rejected.rejected_task_queue, 1);
    assert_eq!(cleanup_after_panic, None);
    assert!(!ran.load(Ordering::Acquire));
    let state = after_panic.expect("actual rejected state's control-block observer");
    assert_never_enqueued(&state);
    // queue_full is the existing Ok(Value::Result{ok:false,..}) Task payload,
    // therefore its status is completed. The unwound final handle clears the
    // result slot; it must not turn this published outcome into cancellation.
    assert_eq!(state.status, "completed");
    assert_eq!(state.cancellation, None);
    assert_eq!(queued_receipt.unwrap(), worker);
    assert!(queued_retired);
    let state = assert_marker(marker, worker);
    assert_never_enqueued(&state);
    assert_eq!(state.status, "completed");
    assert_eq!(state.cancellation, None);
    shutdown.expect("real queued and held callbacks must retire normally");
    let snapshot = final_snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, 3);
    assert_eq!(snapshot.accepted_submissions, 3);
    assert_eq!(snapshot.cleanup_timeouts, 0);
}
