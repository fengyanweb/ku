use super::*;

const TEST_WAIT: Duration = Duration::from_secs(3);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(50);
const PANIC_MESSAGE: &str = "queued capture destructor fixture panic";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureObservation {
    name: &'static str,
    cleanup: Option<CancellationContext>,
    status: &'static str,
    terminal: bool,
    queue_consumed: bool,
    accounting_finished: bool,
}

struct DropProbe {
    name: &'static str,
    panics: bool,
    state: Arc<Mutex<Option<Weak<TaskState>>>>,
    observed: SyncSender<CaptureObservation>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        let state = self
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
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
        if self.panics {
            panic!("{PANIC_MESSAGE}");
        }
    }
}

#[derive(Debug)]
struct WorkerReceipt {
    thread: thread::ThreadId,
    task_id: i64,
    child_status: &'static str,
    child_queue_consumed: bool,
    child_accounting_finished: bool,
    child_cancellation: Option<CancellationContext>,
    child_result_termination: Option<CancellationContext>,
    cleanup_tls: Option<CancellationContext>,
    execution_tls: Option<CancellationContext>,
}

#[test]
fn queued_capture_panic_propagates_after_all_captures_drop_and_still_retires_real_task_accounting()
{
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let (mut release, release_rx) = ReleaseGate::pair();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let held = runtime
        .spawn(move || {
            started_tx.send(thread::current().id()).unwrap();
            release_rx
                .recv_timeout(TEST_WAIT)
                .expect("release held real worker");
            Ok(Value::Null)
        })
        .unwrap();
    let started = started_rx.recv_timeout(TEST_WAIT);
    let state_slot = Arc::new(Mutex::new(None));
    let (drop_tx, drop_rx) = mpsc::sync_channel(2);
    // The closure captures this whole tuple. Rust's actual tuple drop glue must
    // destroy the second capture after the first destructor starts unwinding.
    let captures = (
        DropProbe {
            name: "panic",
            panics: true,
            state: Arc::clone(&state_slot),
            observed: drop_tx.clone(),
        },
        DropProbe {
            name: "after",
            panics: false,
            state: Arc::clone(&state_slot),
            observed: drop_tx,
        },
    );
    let ran = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran);
    let child = runtime
        .spawn(move || {
            child_ran.store(true, Ordering::Release);
            drop(captures);
            Ok(Value::Null)
        })
        .unwrap();
    *state_slot.lock().unwrap() = Some(Arc::downgrade(&child.state));
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        // Expiry must not skip capture destruction or terminal publication.
        // The final independent bounded drain cannot extend this earlier D.
        cleanup_deadline: Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    };
    let cancelled = catch_unwind(AssertUnwindSafe(|| child.request_cancel_with(context)));
    let panic_text = cancelled.as_ref().err().and_then(|payload| {
        payload.downcast_ref::<String>().cloned().or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
        })
    });
    let first_drop = drop_rx.recv_timeout(TEST_WAIT);
    let remaining_drop = drop_rx.recv_timeout(TEST_WAIT);
    let child_status_after_panic = child.status();
    let child_context_after_panic = child.state.cancellation();
    let cleanup_after_panic = current_cleanup_context();
    let drain_mode_after_panic = OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get);
    let release_head_after_panic = OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none());

    release.release();
    let child_state = Arc::clone(&child.state);
    let (marker_tx, marker_rx) = mpsc::sync_channel(1);
    let marker = runtime.spawn(move || {
        let child_result_termination = child_state
            .result
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .and_then(KuError::runtime_termination);
        marker_tx
            .send(WorkerReceipt {
                thread: thread::current().id(),
                task_id: current_task_id(),
                child_status: child_state.status_name(),
                child_queue_consumed: child_state.queue_consumed.load(Ordering::Acquire),
                child_accounting_finished: child_state.accounting_finished.load(Ordering::Acquire),
                child_cancellation: child_state.cancellation(),
                child_result_termination,
                cleanup_tls: current_cleanup_context(),
                execution_tls: current_execution_termination(),
            })
            .unwrap();
        Ok(Value::Int(99))
    });
    let receipt = marker_rx.recv_timeout(TEST_WAIT);
    // The receipt is user-code execution, while this separate finite atomic
    // observation waits for the actual worker's result/accounting retirement.
    let marker_deadline = Instant::now() + TEST_WAIT;
    if let Ok(marker) = &marker {
        while !marker.state.accounting_finished.load(Ordering::Acquire)
            && Instant::now() < marker_deadline
        {
            thread::yield_now();
        }
    }
    let marker_id = marker.as_ref().ok().map(TaskHandle::id);
    let marker_retired = marker.as_ref().ok().is_some_and(|marker| {
        marker.state.queue_consumed.load(Ordering::Acquire)
            && marker.state.accounting_finished.load(Ordering::Acquire)
    });
    let marker_value = marker.as_ref().ok().and_then(|marker| {
        let result = marker.state.result.lock().unwrap();
        match result.as_ref() {
            Some(Ok(Value::Int(value))) => Some(*value),
            _ => None,
        }
    });
    let child_queue_consumed = child.state.queue_consumed.load(Ordering::Acquire);
    let child_accounting_finished = child.state.accounting_finished.load(Ordering::Acquire);
    let before_shutdown = runtime.snapshot();
    let shutdown = runtime.cancel_all_and_wait(SHUTDOWN_WAIT);
    let final_snapshot = runtime.snapshot();
    // Keep RED evidence before any semantic assertion and before dropping the
    // final real handles. No fake completion repairs a stranded cancelling task.
    eprintln!(
        "queued_capture_panic propagated={panic_text:?} drops={first_drop:?},{remaining_drop:?} \
         child_after_panic={child_status_after_panic}/{child_context_after_panic:?} \
         marker={receipt:?} marker_retired={marker_retired} marker_value={marker_value:?} \
         child_queue_consumed={child_queue_consumed} child_accounting_finished={child_accounting_finished} \
         before_shutdown={before_shutdown:?} shutdown={shutdown:?} final_snapshot={final_snapshot:?}"
    );
    drop(marker);
    drop(child);
    drop(held);
    drop(runtime);

    assert!(
        cancelled.is_err(),
        "the original destructor panic must propagate"
    );
    assert_eq!(panic_text.as_deref(), Some(PANIC_MESSAGE));
    let held_worker = started.expect("single real worker entered its finite gate");
    assert_ne!(held_worker, thread::current().id());
    assert_eq!(
        first_drop.unwrap(),
        CaptureObservation {
            name: "panic",
            cleanup: Some(context),
            status: "cancelling",
            terminal: false,
            queue_consumed: false,
            accounting_finished: false,
        }
    );
    assert_eq!(
        remaining_drop.unwrap(),
        CaptureObservation {
            name: "after",
            cleanup: Some(context),
            status: "cancelling",
            terminal: false,
            queue_consumed: false,
            accounting_finished: false,
        }
    );
    assert_eq!(child_status_after_panic, "timed_out");
    assert_eq!(child_context_after_panic, Some(context));
    assert_eq!(cleanup_after_panic, None);
    assert!(!drain_mode_after_panic);
    assert!(release_head_after_panic);
    assert!(!ran.load(Ordering::Acquire));
    let receipt = receipt.expect("subsequent marker executed on the actual worker");
    assert_eq!(receipt.thread, held_worker);
    assert_eq!(Some(receipt.task_id), marker_id);
    assert_ne!(receipt.task_id, 0);
    assert_eq!(receipt.cleanup_tls, None);
    assert_eq!(receipt.execution_tls, None);
    assert_eq!(receipt.child_status, "timed_out");
    assert!(receipt.child_queue_consumed);
    assert!(receipt.child_accounting_finished);
    assert_eq!(receipt.child_cancellation, Some(context));
    assert_eq!(receipt.child_result_termination, Some(context));
    assert!(marker_retired);
    assert_eq!(marker_value, Some(99));
    assert!(child_queue_consumed);
    assert!(child_accounting_finished);
    shutdown.expect("capture panic cannot strand active cancellation accounting");
    let snapshot = final_snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, 3);
    assert_eq!(snapshot.cleanup_timeouts, 0);
}

#[derive(Debug)]
struct CompletionReceipt {
    status: &'static str,
    queue_consumed: bool,
    accounting_finished: bool,
    owner_released: bool,
    cancellation: Option<CancellationContext>,
    result_termination: Option<CancellationContext>,
}

impl CompletionReceipt {
    fn observe(state: &TaskState) -> Self {
        let result_termination = state
            .result
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .and_then(KuError::runtime_termination);
        Self {
            status: state.status_name(),
            queue_consumed: state.queue_consumed.load(Ordering::Acquire),
            accounting_finished: state.accounting_finished.load(Ordering::Acquire),
            owner_released: state.owner_released.load(Ordering::Acquire),
            cancellation: state.cancellation(),
            result_termination,
        }
    }
}

#[derive(Debug)]
struct LatePayloadReceipt {
    thread: thread::ThreadId,
    task_id: i64,
    parent: CompletionReceipt,
    child: Option<CompletionReceipt>,
    cleanup_tls: Option<CancellationContext>,
    execution_tls: Option<CancellationContext>,
}

#[test]
fn losing_owned_payload_panic_completes_real_worker_cleanup_and_preserves_the_original_timeout() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let child_runtime = runtime.clone();
    let (mut release, release_rx) = ReleaseGate::pair();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let state_slot = Arc::new(Mutex::new(None));
    let captured_state_slot = Arc::clone(&state_slot);
    let (drop_tx, drop_rx) = mpsc::sync_channel(2);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listener_id = crate::runtime::http_listener_registry::insert(listener).unwrap();
    let listener_lease = crate::runtime::value::HttpListenerLease::new(listener_id);
    let listener_observer = Arc::downgrade(&listener_lease);
    let captures = (
        DropProbe {
            name: "panic",
            panics: true,
            state: Arc::clone(&state_slot),
            observed: drop_tx.clone(),
        },
        // This real lease must finish dropping during unwind before the tail
        // probe sends its receipt. No extra strong resource observer survives.
        listener_lease,
        DropProbe {
            name: "after",
            panics: false,
            state: Arc::clone(&state_slot),
            observed: drop_tx,
        },
    );
    let ran = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran);
    let parent = runtime
        .spawn(move || {
            let child = child_runtime.spawn(move || {
                child_ran.store(true, Ordering::Release);
                drop(captures);
                Ok(Value::Null)
            })?;
            *captured_state_slot.lock().unwrap() = Some(Arc::downgrade(&child.state));
            let child_id = child.id();
            let child_observer = Arc::downgrade(&child.state);
            let mut env = crate::env::Env::new();
            env.define_owned("Child".into(), Value::Task(child), false, Span::default())?;
            let payload = Value::Function {
                params: Vec::new(),
                param_modes: Vec::new(),
                body: Vec::new(),
                captures: env,
                self_name: None,
                is_async: false,
            };
            ready_tx
                .send((thread::current().id(), child_id, child_observer))
                .unwrap();
            release_rx
                .recv_timeout(TEST_WAIT)
                .expect("release real late payload producer");
            // The actual worker completion path, not the callback, decides that
            // this sole Owned payload loses to the already latched cancellation.
            Ok(payload)
        })
        .unwrap();
    let ready = ready_rx.recv_timeout(TEST_WAIT);
    // Keep only a control-block observer while the sole Task owner remains in
    // the function capture. This is not a TaskHandle or an Owned payload clone.
    let child_state = ready.as_ref().ok().and_then(|(_, _, weak)| weak.upgrade());
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    };
    let cancellation_won = parent.request_cancel_with(context);
    let cancelled_before_completion = parent.state.cancellation();
    let status_before_completion = parent.status();
    let resource_before_completion = listener_observer.upgrade().is_some();
    release.release();
    let first_drop = drop_rx.recv_timeout(TEST_WAIT);
    let remaining_drop = drop_rx.recv_timeout(TEST_WAIT);
    let resource_after_drop = listener_observer.upgrade().is_some();
    let parent_after_drop = CompletionReceipt::observe(&parent.state);
    let parent_state = Arc::clone(&parent.state);
    let marker_child_state = child_state.as_ref().map(Arc::clone);
    let (marker_tx, marker_rx) = mpsc::sync_channel(1);
    let marker = runtime.spawn(move || {
        marker_tx
            .send(LatePayloadReceipt {
                thread: thread::current().id(),
                task_id: current_task_id(),
                parent: CompletionReceipt::observe(&parent_state),
                child: marker_child_state
                    .as_ref()
                    .map(|state| CompletionReceipt::observe(state)),
                cleanup_tls: current_cleanup_context(),
                execution_tls: current_execution_termination(),
            })
            .unwrap();
        Ok(Value::Int(99))
    });
    let receipt = marker_rx.recv_timeout(TEST_WAIT);
    // A lost baseline worker already has a finite missing-receipt result;
    // only a callback that actually entered needs this retirement observation.
    if receipt.is_ok() {
        let deadline = Instant::now() + TEST_WAIT;
        if let Ok(marker) = &marker {
            while !marker.state.accounting_finished.load(Ordering::Acquire)
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
        }
    }
    let marker_id = marker.as_ref().ok().map(TaskHandle::id);
    let marker_retired = marker.as_ref().ok().is_some_and(|marker| {
        marker.state.queue_consumed.load(Ordering::Acquire)
            && marker.state.accounting_finished.load(Ordering::Acquire)
    });
    let marker_value = marker.as_ref().ok().and_then(|marker| {
        let result = marker.state.result.lock().unwrap();
        match result.as_ref() {
            Some(Ok(Value::Int(value))) => Some(*value),
            _ => None,
        }
    });
    let parent_before_shutdown = CompletionReceipt::observe(&parent.state);
    let child_before_shutdown = child_state
        .as_ref()
        .map(|state| CompletionReceipt::observe(state));
    let before_shutdown = runtime.snapshot();
    let shutdown = runtime.cancel_all_and_wait(SHUTDOWN_WAIT);
    let final_snapshot = runtime.snapshot();
    eprintln!(
        "losing_owned_payload_panic ready={ready:?} cancellation_won={cancellation_won} \
         prior={status_before_completion}/{cancelled_before_completion:?} \
         drops={first_drop:?},{remaining_drop:?} resource_before={resource_before_completion} \
         resource_after={resource_after_drop} parent_after_drop={parent_after_drop:?} \
         marker={receipt:?} marker_retired={marker_retired} marker_value={marker_value:?} \
         parent_before_shutdown={parent_before_shutdown:?} child_before_shutdown={child_before_shutdown:?} \
         before_shutdown={before_shutdown:?} shutdown={shutdown:?} final_snapshot={final_snapshot:?}"
    );
    drop(marker);
    drop(parent);
    drop(child_state);
    drop(runtime);

    let (worker, _, _) = ready.expect("real single worker built its sole late Owned payload");
    assert_ne!(worker, thread::current().id());
    assert!(cancellation_won);
    assert_eq!(status_before_completion, "cancelling");
    assert_eq!(cancelled_before_completion, Some(context));
    assert!(resource_before_completion);
    for (observed, name) in [(first_drop, "panic"), (remaining_drop, "after")] {
        assert_eq!(
            observed.unwrap(),
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
    assert!(
        !resource_after_drop,
        "tuple tail ACK preceded the actual listener lease drop"
    );
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(parent_after_drop.status, "timed_out");
    assert_eq!(parent_after_drop.cancellation, Some(context));
    assert_eq!(parent_after_drop.result_termination, Some(context));
    let receipt =
        receipt.expect("the same real worker must survive losing-payload destructor panic");
    assert_eq!(receipt.thread, worker);
    assert_eq!(Some(receipt.task_id), marker_id);
    assert_ne!(receipt.task_id, 0);
    assert_eq!(receipt.cleanup_tls, None);
    assert_eq!(receipt.execution_tls, None);
    assert_eq!(receipt.parent.status, "timed_out");
    assert!(receipt.parent.queue_consumed);
    assert!(receipt.parent.accounting_finished);
    assert!(!receipt.parent.owner_released);
    assert_eq!(receipt.parent.cancellation, Some(context));
    assert_eq!(receipt.parent.result_termination, Some(context));
    let child = receipt
        .child
        .expect("actual queued child's control block remains observable");
    assert_eq!(child.status, "timed_out");
    assert!(child.queue_consumed);
    assert!(child.accounting_finished);
    assert!(child.owner_released);
    assert_eq!(child.cancellation, Some(context));
    assert_eq!(
        child.result_termination, None,
        "the released child owner must not retain a result"
    );
    assert!(marker_retired);
    assert_eq!(marker_value, Some(99));
    shutdown.expect("losing Owned payload panic cannot strand worker task accounting");
    let snapshot = final_snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, 3);
    assert_eq!(snapshot.cleanup_timeouts, 0);
    assert!(current_cleanup_context().is_none());
    assert!(current_execution_termination().is_none());
}
