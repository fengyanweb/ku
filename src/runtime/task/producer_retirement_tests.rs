use super::*;

const WAIT: Duration = Duration::from_secs(3);
const DROP_PANIC: &str = "actual producer capture destructor panic";

struct Release(Option<SyncSender<()>>);

impl Release {
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

impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}

fn until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    for _ in 0..4096 {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

fn held_worker(runtime: &TaskRuntime) -> (TaskHandle, Release) {
    let deadline = Instant::now() + WAIT;
    for _ in 0..128 {
        let (release, release_rx) = Release::pair();
        let (started, ready) = mpsc::sync_channel(1);
        let task = runtime
            .spawn(move || {
                started.send(()).unwrap();
                release_rx.recv_timeout(WAIT).expect("release real worker");
                Ok(Value::Null)
            })
            .unwrap();
        if !task.state.is_terminal() {
            ready.recv_timeout(WAIT).expect("actual worker entered");
            return (task, release);
        }
        assert_eq!(
            error_code(&task.await_result().unwrap()),
            Some("queue_full")
        );
        drop(release);
        assert!(
            Instant::now() < deadline,
            "rendezvous worker never received"
        );
        thread::sleep(Duration::from_millis(2));
    }
    panic!("bounded real worker handoff never succeeded");
}

fn building_state(runtime: &TaskRuntime, previous_ids: &[i64]) -> Arc<TaskState> {
    runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .filter_map(Weak::upgrade)
        .find(|state| !previous_ids.contains(&state.id))
        .expect("the actual producer registered before running its builder")
}

fn error_code(value: &Value) -> Option<&str> {
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

#[derive(Debug)]
struct DropObservation {
    submission: Option<u8>,
    accounting_done: bool,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
}

struct Probe {
    state: Option<Arc<TaskState>>,
    entered: SyncSender<DropObservation>,
    release: Option<Receiver<()>>,
    drops: Arc<AtomicUsize>,
    shorter: Option<CancellationContext>,
    panics: bool,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.entered
            .send(DropObservation {
                submission: self
                    .state
                    .as_ref()
                    .map(|state| state.submission.load(Ordering::Acquire)),
                accounting_done: self
                    .state
                    .as_ref()
                    .is_some_and(|state| state.accounting_done.load(Ordering::Acquire)),
                cleanup: current_cleanup_context(),
                execution: current_execution_termination(),
            })
            .unwrap();
        if let Some(release) = self.release.take() {
            release
                .recv_timeout(WAIT)
                .expect("release actual capture Drop");
        }
        if let Some(shorter) = self.shorter {
            drop(CleanupGuard::enter(shorter));
        }
        self.drops.fetch_add(1, Ordering::AcqRel);
        assert!(!self.panics, "{DROP_PANIC}");
    }
}

fn assert_producer_retired(state: &TaskState) {
    assert_eq!(
        state.submission.load(Ordering::Acquire),
        SUBMISSION_PRODUCER_RETIRED
    );
    assert!(state.is_terminal());
    assert!(!state.queue_consumed.load(Ordering::Acquire));
    assert!(!state.execution_queue_retired.load(Ordering::Acquire));
    assert!(!state.accounting_finished.load(Ordering::Acquire));
    assert!(!state.accounting_done.load(Ordering::Acquire));
    assert!(state.pending_run.lock().unwrap().is_none());
}

fn assert_drained(runtime: &TaskRuntime, accepted: usize) {
    runtime.cancel_all_and_wait(WAIT).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, accepted);
    assert_eq!(snapshot.accepted_submissions, accepted);
}

#[test]
fn early_rejections_wait_for_uninvoked_builder_capture_drop_without_fake_admission() {
    for kind in ["no_worker", "closing", "limit"] {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(usize::from(kind != "no_worker"), 2, 0, 0, 1);
        let mut blocker = (kind != "no_worker").then(|| held_worker(&runtime));
        if kind == "closing" {
            let error = runtime.cancel_all_and_wait(Duration::ZERO).unwrap_err();
            assert!(error.message.contains("bounded shutdown timeout"));
        }
        let before = runtime.snapshot().unwrap();
        let (mut capture_release, capture_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (returned_tx, returned_rx) = mpsc::sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let built = Arc::new(AtomicBool::new(false));
        let build_ran = Arc::clone(&built);
        let probe = Probe {
            state: None,
            entered: entered_tx,
            release: Some(capture_rx),
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        };
        let producer_runtime = runtime.clone();
        let producer = thread::spawn(move || {
            let task = producer_runtime
                .spawn_deferred(move || {
                    build_ran.store(true, Ordering::Release);
                    move || {
                        drop(probe);
                        Ok(Value::Null)
                    }
                })
                .unwrap();
            returned_tx.send(()).unwrap();
            assert!(current_cleanup_context().is_none());
            assert!(current_execution_termination().is_none());
            task
        });
        let during = entered_rx.recv_timeout(WAIT).unwrap();
        // These early states were never registered. Observe actual capture
        // destruction and API return, rather than creating a synthetic entry.
        assert!(during.submission.is_none());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(!built.load(Ordering::Acquire));
        assert!(matches!(returned_rx.try_recv(), Err(TryRecvError::Empty)));
        let paused = runtime.snapshot().unwrap();
        assert_eq!(paused.active_tasks, before.active_tasks);
        assert_eq!(paused.registered_tasks, before.registered_tasks);
        assert_eq!(paused.queued_tasks, before.queued_tasks);
        assert_eq!(paused.accepted_submissions, before.accepted_submissions);
        capture_release.release();
        let task = producer.join().unwrap();
        returned_rx.recv_timeout(WAIT).unwrap();
        assert_producer_retired(&task.state);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!built.load(Ordering::Acquire));
        let code = if kind == "limit" {
            "too_many_tasks"
        } else {
            "runtime_stopped"
        };
        assert_eq!(error_code(&task.await_result().unwrap()), Some(code));
        drop(task);
        if let Some((task, release)) = blocker.as_mut() {
            release.release();
            let _ = task.await_result();
        }
        assert_drained(&runtime, before.accepted_submissions);
    }
}

#[test]
fn queue_rejection_keeps_pending_until_real_factory_drop_for_positive_and_zero_capacity() {
    for capacity in [0, 1] {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(1, capacity, 0, 0, 4);
        let (held, mut worker_release) = held_worker(&runtime);
        let queued = (capacity != 0).then(|| runtime.spawn(|| Ok(Value::Int(17))).unwrap());
        let mut old_ids = vec![held.id()];
        if let Some(task) = &queued {
            old_ids.push(task.id());
        }
        let before = runtime.snapshot().unwrap();
        let (mut capture_release, capture_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (observer_tx, observer_rx) = mpsc::sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let drop_count = Arc::clone(&drops);
        let ran = Arc::new(AtomicBool::new(false));
        let body_ran = Arc::clone(&ran);
        let producer_runtime = runtime.clone();
        let producer = thread::spawn(move || {
            let task = producer_runtime
                .spawn_deferred(|| {
                    let state = building_state(&producer_runtime, &old_ids);
                    observer_tx.send(Arc::clone(&state)).unwrap();
                    let probe = Probe {
                        state: Some(state),
                        entered: entered_tx,
                        release: Some(capture_rx),
                        drops: drop_count,
                        shorter: None,
                        panics: false,
                    };
                    move || {
                        body_ran.store(true, Ordering::Release);
                        drop(probe);
                        Ok(Value::Null)
                    }
                })
                .unwrap();
            assert!(current_cleanup_context().is_none());
            assert!(current_execution_termination().is_none());
            task
        });
        let state = observer_rx.recv_timeout(WAIT).unwrap();
        let during = entered_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(during.submission, Some(SUBMISSION_PENDING));
        assert!(!during.accounting_done);
        assert_eq!(state.submission.load(Ordering::Acquire), SUBMISSION_PENDING);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        let paused = runtime.snapshot().unwrap();
        assert_eq!(paused.active_tasks, before.active_tasks);
        assert_eq!(paused.registered_tasks, before.registered_tasks);
        assert_eq!(paused.queued_tasks, before.queued_tasks);
        assert_eq!(paused.accepted_submissions, before.accepted_submissions);
        capture_release.release();
        let rejected = producer.join().unwrap();
        assert_producer_retired(&state);
        assert_eq!(
            error_code(&rejected.await_result().unwrap()),
            Some("queue_full")
        );
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        drop(rejected);
        worker_release.release();
        held.await_result().unwrap();
        if let Some(task) = queued {
            assert_eq!(task.await_result().unwrap(), Value::Int(17));
        }
        assert_drained(&runtime, before.accepted_submissions);
    }
}

#[test]
fn builder_panic_retires_real_capture_and_reservation_without_worker_accounting() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (held, mut worker_release) = held_worker(&runtime);
    let held_id = held.id();
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (observer_tx, observer_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let drop_count = Arc::clone(&drops);
    let producer_runtime = runtime.clone();
    let producer = thread::spawn(move || {
        producer_runtime
            .spawn_start_deferred(|| {
                let state = building_state(&producer_runtime, &[held_id]);
                observer_tx.send(Arc::clone(&state)).unwrap();
                let _capture = Probe {
                    state: Some(state),
                    entered: entered_tx,
                    release: Some(capture_rx),
                    drops: drop_count,
                    shorter: None,
                    panics: false,
                };
                panic!("actual deferred builder constructor panic");
            })
            .unwrap()
    });
    let state = observer_rx.recv_timeout(WAIT).unwrap();
    let during = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(during.submission, Some(SUBMISSION_PENDING));
    assert!(!during.accounting_done);
    assert_eq!(state.submission.load(Ordering::Acquire), SUBMISSION_PENDING);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    // The deferred constructor has not returned through its panic catch yet.
    let paused = runtime.snapshot().unwrap();
    assert_eq!(paused.active_tasks, 2);
    assert_eq!(paused.registered_tasks, 2);
    assert_eq!(paused.queued_tasks, 0);
    capture_release.release();
    let rejected = producer.join().unwrap();
    assert_producer_retired(&state);
    assert_eq!(
        error_code(&rejected.await_result().unwrap()),
        Some("spawn_panic")
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);
    let after = runtime.snapshot().unwrap();
    assert_eq!(after.active_tasks, 1);
    assert_eq!(after.registered_tasks, 1);
    assert_eq!(after.rejected_task_internal, 1);
    drop(rejected);
    worker_release.release();
    held.await_result().unwrap();
    assert_drained(&runtime, 1);
}

#[test]
fn native_capture_panic_retires_tail_with_same_first_cause_and_shorter_deadline() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (held, mut worker_release) = held_worker(&runtime);
    let held_id = held.id();
    let original = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now() + WAIT,
    };
    let shorter = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() + Duration::from_secs(1),
    };
    let expected = CancellationContext {
        reason: original.reason,
        cleanup_deadline: original.cleanup_deadline.min(shorter.cleanup_deadline),
    };
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(2);
    let (observer_tx, observer_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let drop_count = Arc::clone(&drops);
    let ran = Arc::new(AtomicBool::new(false));
    let body_ran = Arc::clone(&ran);
    let producer_runtime = runtime.clone();
    let producer = thread::spawn(move || {
        let _execution = ExecutionTerminationGuard::enter();
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            producer_runtime.spawn_deferred(|| {
                let state = building_state(&producer_runtime, &[held_id]);
                observer_tx.send(Arc::clone(&state)).unwrap();
                assert_eq!(producer_runtime.cancel_all_with(original), 2);
                let captures = (
                    Probe {
                        state: Some(Arc::clone(&state)),
                        entered: entered_tx.clone(),
                        release: Some(capture_rx),
                        drops: Arc::clone(&drop_count),
                        shorter: Some(shorter),
                        panics: true,
                    },
                    Probe {
                        state: Some(state),
                        entered: entered_tx,
                        release: None,
                        drops: drop_count,
                        shorter: None,
                        panics: false,
                    },
                );
                move || {
                    body_ran.store(true, Ordering::Release);
                    drop(captures);
                    Ok(Value::Null)
                }
            })
        }));
        let native = attempt.expect_err("the actual capture panic must escape submission");
        let message = native
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| native.downcast_ref::<&str>().copied())
            .map(str::to_owned);
        assert!(current_cleanup_context().is_none());
        assert!(current_execution_termination().is_none());
        message
    });
    let state = observer_rx.recv_timeout(WAIT).unwrap();
    let first = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(first.submission, Some(SUBMISSION_PENDING));
    assert!(!first.accounting_done);
    assert_eq!(first.cleanup, Some(original));
    assert!(first.execution.is_none());
    assert_eq!(state.submission.load(Ordering::Acquire), SUBMISSION_PENDING);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    capture_release.release();
    let tail = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(tail.submission, Some(SUBMISSION_PENDING));
    assert_eq!(tail.cleanup, Some(expected));
    assert!(!tail.accounting_done);
    assert_eq!(producer.join().unwrap().as_deref(), Some(DROP_PANIC));
    assert_producer_retired(&state);
    assert_eq!(state.cancellation(), Some(expected));
    assert_eq!(state.status_name(), "timed_out");
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran.load(Ordering::Acquire));
    assert!(state.owner_released.load(Ordering::Acquire));
    assert!(state.owner_cleanup_done.load(Ordering::Acquire));
    assert!(state.result.lock().unwrap().is_none());
    worker_release.release();
    assert!(held.await_result().is_err());
    assert_drained(&runtime, 1);
}

struct OversizedRunner {
    _header: [u8; local_driver::MAX_TYPED_RUNNER_HEADER + 1],
    _capture: Probe,
}

impl LocalTaskRunner for OversizedRunner {
    fn dispatch(
        &mut self,
        _budget: std::num::NonZeroUsize,
    ) -> std::ops::ControlFlow<KuResult<Value>> {
        std::ops::ControlFlow::Break(Ok(Value::Null))
    }
}

#[test]
fn poisoned_after_registration_rolls_back_actual_weak_before_producer_retirement() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (held, mut worker_release) = held_worker(&runtime);
    let held_id = held.id();
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (observer_tx, observer_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let drop_count = Arc::clone(&drops);
    let constructed = Arc::new(AtomicBool::new(false));
    let constructor_ran = Arc::clone(&constructed);
    let producer_runtime = runtime.clone();
    let producer = thread::spawn(move || {
        producer_runtime
            .spawn_runner_deferred(|| {
                let state = building_state(&producer_runtime, &[held_id]);
                observer_tx.send(Arc::clone(&state)).unwrap();
                let poison = catch_unwind(AssertUnwindSafe(|| {
                    let registry = producer_runtime.inner.states.lock().unwrap();
                    assert!(registry.contains_key(&state.id));
                    panic!("actual registry poisoned after producer insertion");
                }));
                assert!(poison.is_err());
                let capture = Probe {
                    state: Some(state),
                    entered: entered_tx,
                    release: Some(capture_rx),
                    drops: drop_count,
                    shorter: None,
                    panics: false,
                };
                move || {
                    constructor_ran.store(true, Ordering::Release);
                    OversizedRunner {
                        _header: [0; local_driver::MAX_TYPED_RUNNER_HEADER + 1],
                        _capture: capture,
                    }
                }
            })
            .unwrap()
    });
    let state = observer_rx.recv_timeout(WAIT).unwrap();
    let during = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(during.submission, Some(SUBMISSION_PENDING));
    assert!(!during.accounting_done);
    assert!(runtime.inner.states.is_poisoned());
    assert!(!runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&state.id));
    assert_eq!(runtime.inner.active_tasks.load(Ordering::Acquire), 1);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    capture_release.release();
    let rejected = producer.join().unwrap();
    assert_producer_retired(&state);
    assert!(rejected
        .await_result()
        .unwrap_err()
        .message
        .contains("frame/header budget"));
    assert!(!constructed.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    let next_built = Arc::new(AtomicBool::new(false));
    let next_builder = Arc::clone(&next_built);
    let next = runtime
        .spawn_deferred(move || {
            next_builder.store(true, Ordering::Release);
            || Ok(Value::Null)
        })
        .unwrap();
    assert!(next
        .await_result()
        .unwrap_err()
        .message
        .contains("registry is poisoned"));
    assert!(!next_built.load(Ordering::Acquire));
    assert_producer_retired(&next.state);
    assert!(runtime.inner.states.is_poisoned());
    assert!(!runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&next.id()));
    drop(next);
    drop(rejected);
    worker_release.release();
    held.await_result().unwrap();
    assert!(until(|| held.state.accounting_done.load(Ordering::Acquire)));
    assert_eq!(runtime.inner.active_tasks.load(Ordering::Acquire), 0);
    assert_eq!(runtime.inner.queued_tasks.load(Ordering::Acquire), 0);
    assert_eq!(runtime.inner.finished_tasks.load(Ordering::Acquire), 1);
    assert!(runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_empty());
    runtime.cancel_all_and_wait(WAIT).unwrap();
}

struct NullRunner;

impl LocalTaskRunner for NullRunner {
    fn dispatch(
        &mut self,
        _budget: std::num::NonZeroUsize,
    ) -> std::ops::ControlFlow<KuResult<Value>> {
        std::ops::ControlFlow::Break(Ok(Value::Null))
    }
}

struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
fn accepted_runner_constructor_panic_keeps_worker_ack_separate_from_producer_rejection() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = CountDrop(Arc::clone(&drops));
    let task = runtime
        .spawn_runner_deferred(move || {
            move || -> NullRunner {
                let _capture = capture;
                panic!("actual accepted runner constructor panic");
            }
        })
        .unwrap();
    assert!(until(|| task.state.accounting_done.load(Ordering::Acquire)));
    assert_eq!(
        task.state.submission.load(Ordering::Acquire),
        SUBMISSION_ACCEPTED
    );
    assert_eq!(task.status(), "panicked");
    assert_eq!(error_code(&task.await_result().unwrap()), Some("panic"));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(task.state.execution_queue_retired.load(Ordering::Acquire));
    assert!(task.state.completion_cleanup_done.load(Ordering::Acquire));
    assert_eq!(Arc::strong_count(&task.owner), 1);
    let healthy = runtime.spawn(|| Ok(Value::Int(19))).unwrap();
    assert_eq!(healthy.await_result().unwrap(), Value::Int(19));
    assert!(until(|| healthy
        .state
        .accounting_done
        .load(Ordering::Acquire)));
    assert_eq!(
        healthy.state.submission.load(Ordering::Acquire),
        SUBMISSION_ACCEPTED
    );
    assert_drained(&runtime, 2);
}

#[test]
fn fast_accepted_tasks_keep_real_g1_accounting_and_monotonic_submission_classification() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let bodies = Arc::new(AtomicUsize::new(0));
    let pending_at_body = Arc::new(AtomicUsize::new(0));
    for index in 0..64 {
        let body_count = Arc::clone(&bodies);
        let early_count = Arc::clone(&pending_at_body);
        let task = runtime
            .spawn(move || {
                let state = CURRENT_TASK_STATE
                    .with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))
                    .expect("actual claimant installed its control-block observer");
                match state.submission.load(Ordering::Acquire) {
                    SUBMISSION_PENDING => {
                        early_count.fetch_add(1, Ordering::AcqRel);
                    }
                    SUBMISSION_ACCEPTED => {}
                    other => panic!("accepted execution observed invalid submission {other}"),
                }
                body_count.fetch_add(1, Ordering::AcqRel);
                Ok(Value::Int(index))
            })
            .unwrap();
        assert_eq!(
            task.state.submission.load(Ordering::Acquire),
            SUBMISSION_ACCEPTED
        );
        assert!(until(|| task.state.accounting_done.load(Ordering::Acquire)));
        assert_eq!(task.await_result().unwrap(), Value::Int(index));
        assert!(task.state.execution_queue_retired.load(Ordering::Acquire));
        assert!(task.state.completion_cleanup_done.load(Ordering::Acquire));
        assert_eq!(
            task.state.submission.load(Ordering::Acquire),
            SUBMISSION_ACCEPTED
        );
        assert_eq!(Arc::strong_count(&task.owner), 1);
    }
    // No assertion requires a scheduler race to occur. Any observed Pending
    // body is legal; all real accepted tasks must still finish their G1 work.
    eprintln!(
        "producer_fast_accept actual_bodies={} pending_at_body={}",
        bodies.load(Ordering::Acquire),
        pending_at_body.load(Ordering::Acquire)
    );
    assert_eq!(bodies.load(Ordering::Acquire), 64);
    assert_drained(&runtime, 64);
}

#[test]
fn disconnected_real_receiver_retires_factory_before_producer_publication() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 3);
    let (held, mut worker_release) = held_worker(&runtime);
    let old_id = held.id();
    // Fault injection drops the actual channel receiver while its real worker
    // is executing the held callback. No admission counter is fabricated.
    let (replacement_tx, replacement_rx) = mpsc::sync_channel(1);
    let original_receiver = {
        let mut queue = runtime.inner.task_rx.lock().unwrap();
        assert!(queue.head.is_none());
        std::mem::replace(&mut queue.receiver, replacement_rx)
    };
    drop(original_receiver);
    let before = runtime.snapshot().unwrap();
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (observer_tx, observer_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let drop_count = Arc::clone(&drops);
    let ran = Arc::new(AtomicBool::new(false));
    let body_ran = Arc::clone(&ran);
    let producer_runtime = runtime.clone();
    let producer = thread::spawn(move || {
        producer_runtime
            .spawn_deferred(|| {
                let state = building_state(&producer_runtime, &[old_id]);
                observer_tx.send(Arc::clone(&state)).unwrap();
                let probe = Probe {
                    state: Some(state),
                    entered: entered_tx,
                    release: Some(capture_rx),
                    drops: drop_count,
                    shorter: None,
                    panics: false,
                };
                move || {
                    body_ran.store(true, Ordering::Release);
                    drop(probe);
                    Ok(Value::Null)
                }
            })
            .unwrap()
    });
    let state = observer_rx.recv_timeout(WAIT).unwrap();
    let during = entered_rx.recv_timeout(WAIT).unwrap();
    let paused = runtime.snapshot().unwrap();
    let paused_submission = state.submission.load(Ordering::Acquire);
    capture_release.release();
    let rejected = producer.join().unwrap();
    let result = rejected.await_result().unwrap();
    drop(replacement_tx);
    worker_release.release();
    assert!(until(|| held.state.accounting_done.load(Ordering::Acquire)));
    held.await_result().unwrap();
    assert_drained(&runtime, before.accepted_submissions);
    assert_eq!(during.submission, Some(SUBMISSION_PENDING));
    assert!(!during.accounting_done);
    assert_eq!(paused_submission, SUBMISSION_PENDING);
    assert_eq!(paused.active_tasks, before.active_tasks);
    assert_eq!(paused.registered_tasks, before.registered_tasks);
    assert_eq!(paused.queued_tasks, before.queued_tasks);
    assert_eq!(paused.accepted_submissions, before.accepted_submissions);
    assert_eq!(
        paused.rejected_task_internal,
        before.rejected_task_internal + 1
    );
    assert_producer_retired(&state);
    assert_eq!(error_code(&result), Some("runtime_stopped"));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(!ran.load(Ordering::Acquire));
}
