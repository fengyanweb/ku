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
    cancellation: Option<CancellationContext>,
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
                cancellation: self.state.as_ref().and_then(|state| state.cancellation()),
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
    assert_eq!(runtime.inner.producer_count(), 0);
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, accepted);
    assert_eq!(snapshot.accepted_submissions, accepted);
}

#[test]
fn global_shutdown_waits_for_unregistered_no_worker_producer_capture_drop() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (returned_tx, returned_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicBool::new(false));
    let build_ran = Arc::clone(&built);
    let body_ran = Arc::clone(&ran);
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
        let task = producer_runtime.spawn_deferred(move || {
            build_ran.store(true, Ordering::Release);
            move || {
                body_ran.store(true, Ordering::Release);
                drop(probe);
                Ok(Value::Null)
            }
        });
        returned_tx.send(()).unwrap();
        task
    });
    let during = entered_rx.recv_timeout(WAIT).unwrap();
    let paused = runtime.snapshot().unwrap();
    let drops_while_paused = drops.load(Ordering::Acquire);
    let returned_while_paused = returned_rx.try_recv();
    let shutdown_while_paused = runtime.cancel_all_and_wait(Duration::ZERO);
    // Keep the safe old-API RED independent of the new producer gate. End the
    // actual Drop/API before semantic assertions even on the old implementation.
    capture_release.release();
    let task = producer.join().unwrap().unwrap();
    returned_rx.recv_timeout(WAIT).unwrap();
    let outcome = task.await_result();
    drop(task);
    let second_shutdown = runtime.cancel_all_and_wait(WAIT);
    let after = runtime.snapshot().unwrap();
    assert!(during.submission.is_none());
    assert_eq!(drops_while_paused, 0);
    assert!(matches!(returned_while_paused, Err(TryRecvError::Empty)));
    assert_eq!(
        (
            paused.active_tasks,
            paused.registered_tasks,
            paused.queued_tasks
        ),
        (0, 0, 0)
    );
    assert_eq!((paused.accepted_submissions, paused.finished_tasks), (0, 0));
    let error = shutdown_while_paused
        .expect_err("global shutdown must wait for the actual unregistered producer capture Drop");
    assert_eq!(error.domain.as_deref(), Some("task"));
    assert_eq!(error.code.as_deref(), Some("shutdown_timeout"));
    assert_eq!(error_code(&outcome.unwrap()), Some("runtime_stopped"));
    assert!(!built.load(Ordering::Acquire));
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(second_shutdown.unwrap(), 0);
    assert_eq!(
        (
            after.active_tasks,
            after.registered_tasks,
            after.queued_tasks,
            after.wait_edges
        ),
        (0, 0, 0, 0)
    );
    assert_eq!((after.accepted_submissions, after.finished_tasks), (0, 0));
}

#[test]
fn shutdown_preserves_original_expired_deadline_when_observed_producer_retires_before_registry_unlock(
) {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
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
        producer_runtime.spawn_deferred(move || {
            build_ran.store(true, Ordering::Release);
            move || {
                drop(probe);
                Ok(Value::Null)
            }
        })
    });
    let during = entered_rx.recv_timeout(WAIT).unwrap();
    let paused = runtime.snapshot().unwrap();
    // Hold the actual empty registry, not a surrogate shutdown gate. Closing
    // becomes visible before cancel_all_with can acquire this real mutex.
    let registry = runtime.inner.states.lock().unwrap();
    let shutdown_runtime = runtime.clone();
    let shutdown = thread::spawn(move || shutdown_runtime.cancel_all_and_wait(Duration::ZERO));
    let closing_observed = until(|| runtime.inner.closing.load(Ordering::Acquire));
    capture_release.release();
    let task = producer.join().unwrap().unwrap();
    // Keep this real owner alive until releasing the registry: last-owner
    // cleanup may acquire it. No cleanup/ACK is manufactured by this fixture.
    drop(registry);
    let shutdown_after_producer_retired = shutdown.join().unwrap();
    let outcome = task.await_result();
    drop(task);
    let second_shutdown = runtime.cancel_all_and_wait(WAIT);
    let empty_zero_deadline_shutdown = runtime.cancel_all_and_wait(Duration::ZERO);
    let after = runtime.snapshot().unwrap();
    assert!(during.submission.is_none());
    assert_eq!(
        (
            paused.active_tasks,
            paused.registered_tasks,
            paused.queued_tasks
        ),
        (0, 0, 0)
    );
    assert_eq!((paused.accepted_submissions, paused.finished_tasks), (0, 0));
    assert!(
        closing_observed,
        "bounded observation of the actual shutdown start"
    );
    assert_eq!(
        shutdown_after_producer_retired.unwrap_err().code.as_deref(),
        Some("shutdown_timeout")
    );
    assert_eq!(error_code(&outcome.unwrap()), Some("runtime_stopped"));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(!built.load(Ordering::Acquire));
    assert_eq!(second_shutdown.unwrap(), 0);
    assert_eq!(empty_zero_deadline_shutdown.unwrap(), 0);
    assert_eq!(
        (
            after.active_tasks,
            after.registered_tasks,
            after.queued_tasks,
            after.wait_edges
        ),
        (0, 0, 0, 0)
    );
    assert_eq!((after.accepted_submissions, after.finished_tasks), (0, 0));
}

#[test]
fn early_rejections_wait_for_uninvoked_builder_capture_drop_without_fake_admission() {
    for kind in ["no_worker", "closing", "limit", "termination"] {
        let _execution = ExecutionTerminationGuard::enter();
        let has_worker = matches!(kind, "closing" | "limit");
        let runtime = TaskRuntime::with_limits(usize::from(has_worker), 2, 0, 0, 1);
        let mut blocker = has_worker.then(|| held_worker(&runtime));
        let original = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now(),
        };
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
            let execution = ExecutionTerminationGuard::enter();
            if kind == "termination" {
                set_execution_termination(original);
            }
            let task = producer_runtime.spawn_deferred(move || {
                build_ran.store(true, Ordering::Release);
                move || {
                    drop(probe);
                    Ok(Value::Null)
                }
            });
            returned_tx.send(()).unwrap();
            assert!(current_cleanup_context().is_none());
            if kind == "termination" {
                assert_eq!(current_execution_termination(), Some(original));
            }
            drop(execution);
            assert!(current_execution_termination().is_none());
            task
        });
        let during = entered_rx.recv_timeout(WAIT).unwrap();
        // These early states were never registered. Observe actual capture
        // destruction and API return, rather than creating a synthetic entry.
        let drops_while_paused = drops.load(Ordering::Acquire);
        let returned_while_paused = returned_rx.try_recv();
        let paused = runtime.snapshot().unwrap();
        let producers_while_paused = runtime.inner.producer_count();
        let held = blocker.take().map(|(task, mut release)| {
            release.release();
            let result = task.await_result();
            let retired = until(|| task.state.accounting_done.load(Ordering::Acquire));
            drop(task);
            (result, retired)
        });
        let producer_only = runtime.snapshot().unwrap();
        let shutdown_while_paused = runtime.cancel_all_and_wait(Duration::ZERO);
        capture_release.release();
        let task = producer.join().unwrap();
        returned_rx.recv_timeout(WAIT).unwrap();
        let (state, outcome, early_error) = match task {
            Ok(task) => {
                let state = Arc::clone(&task.state);
                let outcome = task.await_result();
                drop(task);
                (Some(state), Some(outcome), None)
            }
            Err(error) => (None, None, Some(error)),
        };
        assert_drained(&runtime, before.accepted_submissions);
        assert!(during.submission.is_none());
        assert_eq!(drops_while_paused, 0);
        assert!(matches!(returned_while_paused, Err(TryRecvError::Empty)));
        assert_eq!(paused.active_tasks, before.active_tasks);
        assert_eq!(paused.registered_tasks, before.registered_tasks);
        assert_eq!(paused.queued_tasks, before.queued_tasks);
        assert_eq!(paused.accepted_submissions, before.accepted_submissions);
        assert_eq!(
            paused.total_submissions,
            before.total_submissions + usize::from(kind != "termination")
        );
        assert_eq!(producers_while_paused, usize::from(kind != "closing"));
        assert_eq!(
            (
                producer_only.active_tasks,
                producer_only.registered_tasks,
                producer_only.queued_tasks
            ),
            (0, 0, 0)
        );
        if kind == "closing" {
            // This later API still destroys its captures before returning,
            // but acquires no responsibility after the shutdown linearization.
            assert_eq!(shutdown_while_paused.unwrap(), 0);
        } else {
            assert_eq!(
                shutdown_while_paused.unwrap_err().code.as_deref(),
                Some("shutdown_timeout")
            );
        }
        if let Some((result, retired)) = held {
            assert!(retired);
            if kind == "closing" {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), Value::Null);
            }
        }
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!built.load(Ordering::Acquire));
        if kind == "termination" {
            // Admission precedes ensure_task_operations_allowed's real early
            // return. The uncalled constructor still owns its capture until
            // this paused Drop finishes, without creating a Task submission.
            assert!(state.is_none());
            assert!(outcome.is_none());
            assert_eq!(early_error.unwrap().runtime_termination(), Some(original));
            assert_eq!(during.execution, Some(original));
            assert_eq!(paused.total_submissions, 0);
            continue;
        }
        assert!(early_error.is_none());
        assert_producer_retired(&state.unwrap());
        let code = if kind == "limit" {
            "too_many_tasks"
        } else {
            "runtime_stopped"
        };
        assert_eq!(error_code(&outcome.unwrap().unwrap()), Some(code));
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
        let paused_submission = state.submission.load(Ordering::Acquire);
        let drops_while_paused = drops.load(Ordering::Acquire);
        let paused = runtime.snapshot().unwrap();
        let producers_while_paused = runtime.inner.producer_count();
        worker_release.release();
        let held_result = held.await_result();
        let held_retired = until(|| held.state.accounting_done.load(Ordering::Acquire));
        drop(held);
        let queued_result = queued.map(|task| {
            let result = task.await_result();
            let retired = until(|| task.state.accounting_done.load(Ordering::Acquire));
            drop(task);
            (result, retired)
        });
        let producer_only = runtime.snapshot().unwrap();
        let shutdown_while_paused = runtime.cancel_all_and_wait(Duration::ZERO);
        capture_release.release();
        let rejected = producer.join().unwrap();
        let result = rejected.await_result();
        drop(rejected);
        assert_drained(&runtime, before.accepted_submissions);
        assert_eq!(during.submission, Some(SUBMISSION_PENDING));
        assert!(!during.accounting_done);
        assert_eq!(paused_submission, SUBMISSION_PENDING);
        assert_eq!(drops_while_paused, 0);
        assert_eq!(producers_while_paused, 1);
        assert_eq!(paused.active_tasks, before.active_tasks);
        assert_eq!(paused.registered_tasks, before.registered_tasks);
        assert_eq!(paused.queued_tasks, before.queued_tasks);
        assert_eq!(paused.accepted_submissions, before.accepted_submissions);
        assert_eq!(
            (
                producer_only.active_tasks,
                producer_only.registered_tasks,
                producer_only.queued_tasks
            ),
            (0, 0, 0)
        );
        assert_eq!(
            shutdown_while_paused.unwrap_err().code.as_deref(),
            Some("shutdown_timeout")
        );
        assert_producer_retired(&state);
        assert_eq!(error_code(&result.unwrap()), Some("queue_full"));
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        assert!(held_retired);
        assert_eq!(held_result.unwrap(), Value::Null);
        if let Some((result, retired)) = queued_result {
            assert!(retired);
            assert_eq!(result.unwrap(), Value::Int(17));
        }
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

#[test]
fn no_worker_native_capture_panic_keeps_global_responsibility_through_actual_tail_drop() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let (mut first_release, first_rx) = Release::pair();
    let (mut tail_release, tail_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(2);
    let (returned_tx, returned_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(AtomicBool::new(false));
    let build_ran = Arc::clone(&built);
    let captures = (
        Probe {
            state: None,
            entered: entered_tx.clone(),
            release: Some(first_rx),
            drops: Arc::clone(&drops),
            shorter: None,
            panics: true,
        },
        Probe {
            state: None,
            entered: entered_tx,
            release: Some(tail_rx),
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        },
    );
    let producer_runtime = runtime.clone();
    let producer = thread::spawn(move || {
        let execution = ExecutionTerminationGuard::enter();
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            producer_runtime.spawn_deferred(move || {
                build_ran.store(true, Ordering::Release);
                move || {
                    drop(captures);
                    Ok(Value::Null)
                }
            })
        }));
        returned_tx.send(()).unwrap();
        let message = attempt.err().and_then(|native| {
            native
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| native.downcast_ref::<&str>().copied())
                .map(str::to_owned)
        });
        let restored_cleanup = current_cleanup_context();
        drop(execution);
        (message, restored_cleanup, current_execution_termination())
    });
    let first = entered_rx.recv_timeout(WAIT).unwrap();
    let first_count = runtime.inner.producer_count();
    let first_shutdown = runtime.cancel_all_and_wait(Duration::ZERO);
    first_release.release();
    let tail = entered_rx.recv_timeout(WAIT).unwrap();
    let tail_snapshot = runtime.snapshot().unwrap();
    let tail_count = runtime.inner.producer_count();
    let tail_drops = drops.load(Ordering::Acquire);
    let returned_during_tail = returned_rx.try_recv();
    let tail_shutdown = runtime.cancel_all_and_wait(Duration::ZERO);
    // The native panic is real. Let Rust finish the remaining tuple field and
    // the API's outer catch/handle cleanup before checking retirement.
    tail_release.release();
    let (message, cleanup, execution) = producer.join().unwrap();
    returned_rx.recv_timeout(WAIT).unwrap();
    let second_shutdown = runtime.cancel_all_and_wait(WAIT);
    assert!(first.submission.is_none());
    assert!(tail.submission.is_none());
    assert!(first.cleanup.is_none());
    assert!(tail.cleanup.is_none());
    assert!(first.execution.is_none());
    assert!(tail.execution.is_none());
    assert_eq!((first_count, tail_count), (1, 1));
    assert_eq!(tail_drops, 1);
    assert!(matches!(returned_during_tail, Err(TryRecvError::Empty)));
    assert_eq!(
        (
            tail_snapshot.active_tasks,
            tail_snapshot.registered_tasks,
            tail_snapshot.queued_tasks
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            tail_snapshot.accepted_submissions,
            tail_snapshot.finished_tasks
        ),
        (0, 0)
    );
    assert_eq!(
        first_shutdown.unwrap_err().code.as_deref(),
        Some("shutdown_timeout")
    );
    assert_eq!(
        tail_shutdown.unwrap_err().code.as_deref(),
        Some("shutdown_timeout")
    );
    assert_eq!(message.as_deref(), Some(DROP_PANIC));
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!built.load(Ordering::Acquire));
    assert!(cleanup.is_none());
    assert!(execution.is_none());
    assert_eq!(second_shutdown.unwrap(), 0);
    assert_drained(&runtime, 0);
}

#[test]
fn producer_shutdown_is_runtime_local_and_closed_admission_does_not_reopen_the_prefix() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let independent = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let drops = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(AtomicBool::new(false));
    let spawn_paused = |release_rx, entered_tx| {
        let producer_runtime = runtime.clone();
        let probe = Probe {
            state: None,
            entered: entered_tx,
            release: Some(release_rx),
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        };
        let build_ran = Arc::clone(&built);
        thread::spawn(move || {
            producer_runtime.spawn_deferred(move || {
                build_ran.store(true, Ordering::Release);
                move || {
                    drop(probe);
                    Ok(Value::Null)
                }
            })
        })
    };
    let (mut admitted_release, admitted_rx) = Release::pair();
    let (admitted_entered_tx, admitted_entered_rx) = mpsc::sync_channel(1);
    let admitted = spawn_paused(admitted_rx, admitted_entered_tx);
    let first_drop = admitted_entered_rx.recv_timeout(WAIT).unwrap();
    let independent_shutdown = independent.cancel_all_and_wait(Duration::ZERO);
    let first_count = runtime.inner.producer_count();
    let first_shutdown = runtime.cancel_all_and_wait(Duration::ZERO);
    let (mut closed_release, closed_rx) = Release::pair();
    let (closed_entered_tx, closed_entered_rx) = mpsc::sync_channel(1);
    let closed = spawn_paused(closed_rx, closed_entered_tx);
    let closed_drop = closed_entered_rx.recv_timeout(WAIT).unwrap();
    let count_with_both_captures = runtime.inner.producer_count();
    admitted_release.release();
    let first_task = admitted.join().unwrap().unwrap();
    let first_outcome = first_task.await_result();
    drop(first_task);
    let count_with_only_post_close_capture = runtime.inner.producer_count();
    let drops_with_only_post_close_capture = drops.load(Ordering::Acquire);
    // A shutdown covers its admitted prefix. This later API remains blocked
    // in its own capture Drop, but cannot retroactively reopen that prefix.
    let completed_prefix_shutdown = runtime.cancel_all_and_wait(Duration::ZERO);
    closed_release.release();
    let closed_task = closed.join().unwrap().unwrap();
    let closed_outcome = closed_task.await_result();
    drop(closed_task);
    assert_drained(&runtime, 0);
    assert_drained(&independent, 0);
    assert!(first_drop.submission.is_none());
    assert!(closed_drop.submission.is_none());
    assert_eq!(independent_shutdown.unwrap(), 0);
    assert_eq!(
        first_shutdown.unwrap_err().code.as_deref(),
        Some("shutdown_timeout")
    );
    assert_eq!((first_count, count_with_both_captures), (1, 1));
    assert_eq!(count_with_only_post_close_capture, 0);
    assert_eq!(drops_with_only_post_close_capture, 1);
    assert_eq!(completed_prefix_shutdown.unwrap(), 0);
    assert_eq!(error_code(&first_outcome.unwrap()), Some("runtime_stopped"));
    assert_eq!(
        error_code(&closed_outcome.unwrap()),
        Some("runtime_stopped")
    );
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!built.load(Ordering::Acquire));
    assert!(current_cleanup_context().is_none());
    assert!(current_execution_termination().is_none());
}

#[test]
fn queued_capture_feedback_broadcasts_actual_minimum_deadline_without_replacing_first_cause() {
    for with_outer in [true, false] {
        let _execution = ExecutionTerminationGuard::enter();
        assert!(current_cleanup_context().is_none());
        let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 4);
        let (held, mut worker_release) = held_worker(&runtime);
        let supplied = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: Instant::now() + WAIT,
        };
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now(),
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let mut old_ids = vec![held.id()];
        let mut queued = Vec::new();
        for _ in 0..2 {
            let body_count = Arc::clone(&bodies);
            let task = runtime
                .spawn_deferred(|| {
                    let probe = Probe {
                        state: Some(building_state(&runtime, &old_ids)),
                        entered: entered_tx.clone(),
                        release: None,
                        drops: Arc::clone(&drops),
                        shorter: Some(shorter),
                        panics: false,
                    };
                    move || {
                        body_count.fetch_add(1, Ordering::AcqRel);
                        drop(probe);
                        Ok(Value::Null)
                    }
                })
                .unwrap();
            old_ids.push(task.id());
            queued.push(task);
        }
        let before = runtime.snapshot().unwrap();
        let cleanup = with_outer.then(|| CleanupGuard::enter(supplied));
        let cancelled = if with_outer {
            runtime.cancel_all_with(supplied)
        } else {
            runtime.cancel_all()
        };
        let first = entered_rx.recv_timeout(WAIT).unwrap();
        let second = entered_rx.recv_timeout(WAIT).unwrap();
        let effective_cleanup = current_cleanup_context();
        let queued_contexts = queued
            .iter()
            .map(|task| task.state.cancellation())
            .collect::<Vec<_>>();
        // For bare public cancel_all, measure its actual initial context instead
        // of inventing that API's deadline. All source-state receipts above were
        // collected before installing any private wait context below.
        let original = first
            .cancellation
            .expect("the real first queued request installed cancellation");
        let expected = CancellationContext {
            reason: original.reason,
            cleanup_deadline: original.cleanup_deadline.min(shorter.cleanup_deadline),
        };
        // Both Drop probes tighten the same real batch TLS D. HashMap ordering
        // may choose either Task first; the later actual Drop must see that D.
        // The first Drop begins with its original state context; real cleanup
        // feedback tightens that state before completion, so both final states
        // have the minimum D. Observe the second state before its own Drop
        // tightens anything, independently of CleanupGuard merging caller TLS.
        // This guard only controls the bounded cleanup part of this private
        // fixture. Bare cancel_all must restore the caller's original absent TLS;
        // it is not expected to keep a batch deadline in the caller afterward.
        let wait_cleanup = CleanupGuard::enter(expected);
        let shutdown_with_shorter_tls = runtime.cancel_all_and_wait(WAIT);
        let held_context = held.state.cancellation();
        drop(wait_cleanup);
        drop(cleanup);
        worker_release.release();
        let held_outcome = held.await_result();
        let all_retired = until(|| {
            std::iter::once(&held).chain(queued.iter()).all(|task| {
                task.state.accounting_done.load(Ordering::Acquire)
                    && task.state.execution_queue_retired.load(Ordering::Acquire)
                    && task.state.completion_cleanup_done.load(Ordering::Acquire)
            })
        });
        let queued_outcomes = queued
            .iter()
            .map(TaskHandle::await_result)
            .collect::<Vec<_>>();
        drop(queued);
        drop(held);
        let second_shutdown = runtime.cancel_all_and_wait(WAIT);
        assert_eq!(
            (
                before.active_tasks,
                before.registered_tasks,
                before.queued_tasks
            ),
            (3, 3, 2)
        );
        assert_eq!(cancelled, 3);
        assert_eq!(first.submission, Some(SUBMISSION_ACCEPTED));
        assert_eq!(second.submission, Some(SUBMISSION_ACCEPTED));
        assert!(!first.accounting_done);
        assert!(!second.accounting_done);
        assert_eq!(first.cleanup, Some(original));
        assert_eq!(second.cleanup, Some(expected));
        assert_eq!(first.cancellation, Some(original));
        assert_eq!(second.cancellation, Some(expected));
        assert!(first.execution.is_none());
        assert!(second.execution.is_none());
        if with_outer {
            assert_eq!(original, supplied);
            assert_eq!(effective_cleanup, Some(expected));
        } else {
            assert_eq!(original.reason, TerminationReason::Cancelled);
            assert!(effective_cleanup.is_none());
        }
        assert!(original.cleanup_deadline > shorter.cleanup_deadline);
        assert_eq!(queued_contexts, vec![Some(expected); 2]);
        assert_eq!(
            shutdown_with_shorter_tls.unwrap_err().code.as_deref(),
            Some("shutdown_timeout")
        );
        assert_eq!(held_context, Some(expected));
        assert_eq!(
            held_outcome.unwrap_err().runtime_termination(),
            Some(expected)
        );
        for (outcome, context) in queued_outcomes.into_iter().zip(queued_contexts) {
            assert_eq!(outcome.unwrap_err().runtime_termination(), context);
        }
        assert!(all_retired);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert_eq!(bodies.load(Ordering::Acquire), 0);
        assert_eq!(second_shutdown.unwrap(), 0);
        assert!(current_cleanup_context().is_none());
        assert!(current_execution_termination().is_none());
        assert_drained(&runtime, 3);
    }
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
    let registry_poisoned = runtime.inner.states.is_poisoned();
    let rejected_registered = runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&state.id);
    let active_while_paused = runtime.inner.active_tasks.load(Ordering::Acquire);
    let drops_while_paused = drops.load(Ordering::Acquire);
    let producers_while_paused = runtime.inner.producer_count();
    // Preserve the old poisoned-registry rejection before closing admission.
    // A subsequent post-close call would correctly return runtime_stopped.
    let next_built = Arc::new(AtomicBool::new(false));
    let next_builder = Arc::clone(&next_built);
    let next = runtime
        .spawn_deferred(move || {
            next_builder.store(true, Ordering::Release);
            || Ok(Value::Null)
        })
        .unwrap();
    let next_state = Arc::clone(&next.state);
    let next_error = next.await_result();
    let next_registered = runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&next.id());
    drop(next);
    worker_release.release();
    let held_result = held.await_result();
    let held_retired = until(|| held.state.accounting_done.load(Ordering::Acquire));
    drop(held);
    let producer_only_active = runtime.inner.active_tasks.load(Ordering::Acquire);
    let producer_only_queued = runtime.inner.queued_tasks.load(Ordering::Acquire);
    let producer_only_registry_empty = runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_empty();
    let shutdown_while_paused = runtime.cancel_all_and_wait(Duration::ZERO);
    capture_release.release();
    let rejected = producer.join().unwrap();
    let rejected_error = rejected.await_result();
    drop(rejected);
    let second_shutdown = runtime.cancel_all_and_wait(WAIT);
    assert_eq!(during.submission, Some(SUBMISSION_PENDING));
    assert!(!during.accounting_done);
    assert!(registry_poisoned);
    assert!(!rejected_registered);
    assert_eq!(active_while_paused, 1);
    assert_eq!(drops_while_paused, 0);
    assert_eq!(producers_while_paused, 1);
    assert!(held_retired);
    assert_eq!(held_result.unwrap(), Value::Null);
    assert_eq!((producer_only_active, producer_only_queued), (0, 0));
    assert!(producer_only_registry_empty);
    assert_eq!(
        shutdown_while_paused.unwrap_err().code.as_deref(),
        Some("shutdown_timeout")
    );
    assert_producer_retired(&state);
    assert!(rejected_error
        .unwrap_err()
        .message
        .contains("frame/header budget"));
    assert_producer_retired(&next_state);
    assert!(next_error
        .unwrap_err()
        .message
        .contains("registry is poisoned"));
    assert!(!next_built.load(Ordering::Acquire));
    assert!(!next_registered);
    assert!(!constructed.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(runtime.inner.states.is_poisoned());
    assert_eq!(second_shutdown.unwrap(), 0);
    assert_eq!(runtime.inner.producer_count(), 0);
    assert_eq!(runtime.inner.active_tasks.load(Ordering::Acquire), 0);
    assert_eq!(runtime.inner.queued_tasks.load(Ordering::Acquire), 0);
    assert_eq!(runtime.inner.finished_tasks.load(Ordering::Acquire), 1);
    assert!(runtime
        .inner
        .states
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_empty());
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
