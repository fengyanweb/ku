use super::*;

const WAIT: Duration = Duration::from_secs(3);
const CAPTURE_PANIC: &str = "actual blocking capture destructor panic";

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

type ParentObserver = Arc<Mutex<Option<Weak<TaskState>>>>;

fn bind(observer: &ParentObserver, parent: &TaskHandle) -> Arc<TaskState> {
    *observer.lock().unwrap() = Some(Arc::downgrade(&parent.state));
    Arc::clone(&parent.state)
}

fn parent_finished(state: &TaskState) -> bool {
    state.execution_queue_retired.load(Ordering::Acquire)
        && state.completion_cleanup_done.load(Ordering::Acquire)
        && state.accounting_done.load(Ordering::Acquire)
        && state.owner_cleanup_done.load(Ordering::Acquire)
}

fn saved_context() -> CancellationContext {
    // WAIT bounds fixture handshakes, not the language cleanup window. A final
    // logical owner dropped outside a cleanup guard creates another ordinary
    // cancellation context; that must not shorten an artificial three-second D.
    CancellationContext::new(TerminationReason::TimedOut)
}

fn held_task(runtime: &TaskRuntime) -> (TaskHandle, Release) {
    let (release, release_rx) = Release::pair();
    let (entered, ready) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            entered.send(()).unwrap();
            release_rx
                .recv_timeout(WAIT)
                .expect("release actual task worker");
            Ok(Value::Null)
        })
        .unwrap();
    ready.recv_timeout(WAIT).unwrap();
    (task, release)
}

struct WorkerExit(SyncSender<()>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

thread_local! {
    // This observes the real physical thread's final TLS destruction. Its
    // blocking_worker_loop arguments, including the receiver Arc, have already
    // unwound; no production publisher or scheduler hook is installed.
    static WORKER_EXIT: RefCell<Option<WorkerExit>> = const { RefCell::new(None) };
}

fn held_blocking(runtime: &TaskRuntime) -> (thread::JoinHandle<KuResult<Value>>, Release) {
    held_blocking_with_exit(runtime, None)
}

fn held_blocking_with_exit(
    runtime: &TaskRuntime,
    exited: Option<SyncSender<()>>,
) -> (thread::JoinHandle<KuResult<Value>>, Release) {
    let deadline = Instant::now() + WAIT;
    // sync_channel(0) remains a rendezvous. Only fixture setup retries a real
    // Full admission before the newly spawned blocking worker is receiving.
    for _ in 0..128 {
        let (release, release_rx) = Release::pair();
        let (entered, ready) = mpsc::sync_channel(1);
        let work_runtime = runtime.clone();
        let exited = exited.clone();
        let caller = thread::spawn(move || {
            work_runtime.run_blocking(
                move || {
                    if let Some(exited) = exited {
                        WORKER_EXIT.with(|slot| {
                            assert!(slot.borrow_mut().replace(WorkerExit(exited)).is_none());
                        });
                    }
                    entered.send(()).unwrap();
                    release_rx
                        .recv_timeout(WAIT)
                        .expect("release actual blocking worker");
                    Ok(Value::Int(1))
                },
                Span::default(),
            )
        });
        while !caller.is_finished() {
            if ready.try_recv().is_ok() {
                return (caller, release);
            }
            assert!(
                Instant::now() < deadline,
                "actual blocking worker never entered"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            error_code(&caller.join().unwrap().unwrap()),
            Some("queue_full")
        );
        drop(release);
        assert!(
            Instant::now() < deadline,
            "blocking rendezvous never admitted"
        );
    }
    panic!("bounded actual blocking handoff never succeeded");
}

#[derive(Debug)]
struct Observation {
    pending: usize,
    execution_retired: bool,
    completion_done: bool,
    accounting_done: bool,
    owner_done: bool,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
}

struct Probe {
    parent: ParentObserver,
    entered: SyncSender<Observation>,
    release: Option<Receiver<()>>,
    drops: Arc<AtomicUsize>,
    shorter: Option<CancellationContext>,
    panics: bool,
}

impl Drop for Probe {
    fn drop(&mut self) {
        let parent = self
            .parent
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("observe the actual parent control block without a Task owner");
        self.entered
            .send(Observation {
                pending: parent.pending_blocking.load(Ordering::Acquire),
                execution_retired: parent.execution_queue_retired.load(Ordering::Acquire),
                completion_done: parent.completion_cleanup_done.load(Ordering::Acquire),
                accounting_done: parent.accounting_done.load(Ordering::Acquire),
                owner_done: parent.owner_cleanup_done.load(Ordering::Acquire),
                cleanup: current_cleanup_context(),
                execution: current_execution_termination(),
            })
            .unwrap();
        if let Some(release) = self.release.take() {
            release
                .recv_timeout(WAIT)
                .expect("release actual blocking capture Drop");
        }
        if let Some(shorter) = self.shorter {
            drop(CleanupGuard::enter(shorter));
        }
        self.drops.fetch_add(1, Ordering::AcqRel);
        assert!(!self.panics, "{CAPTURE_PANIC}");
    }
}

fn assert_all_parent_stages(observation: &Observation) {
    assert!(observation.execution_retired);
    assert!(observation.completion_done);
    assert!(observation.accounting_done);
    assert!(observation.owner_done);
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

fn running_late_result_case(foreign: bool) {
    let _execution = ExecutionTerminationGuard::enter();
    let parent_runtime = TaskRuntime::with_limits(1, 2, usize::from(!foreign), 2, 3);
    let pool = if foreign {
        TaskRuntime::with_limits(0, 0, 1, 2, 1)
    } else {
        parent_runtime.clone()
    };
    let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (blocker, mut child_worker_release) = held_task(&child_runtime);
    let observer: ParentObserver = Arc::new(Mutex::new(None));
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(2);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran_child = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran_child);
    let payload = (
        Probe {
            parent: Arc::clone(&observer),
            entered: entered_tx.clone(),
            release: Some(capture_rx),
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        },
        Probe {
            parent: Arc::clone(&observer),
            entered: entered_tx,
            release: None,
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        },
    );
    let child = child_runtime
        .spawn(move || {
            child_ran.store(true, Ordering::Release);
            drop(payload);
            Ok(Value::Null)
        })
        .unwrap();
    let child_state = Arc::clone(&child.state);
    let child_owner = Arc::downgrade(&child.owner);
    let (mut parent_begin, parent_begin_rx) = Release::pair();
    let (mut run_release, run_release_rx) = Release::pair();
    let (run_entered, run_ready) = mpsc::sync_channel(1);
    let work_pool = pool.clone();
    let parent = parent_runtime
        .spawn(move || {
            parent_begin_rx
                .recv_timeout(WAIT)
                .expect("bind actual parent observer");
            work_pool.run_blocking(
                move || {
                    run_entered.send(()).unwrap();
                    run_release_rx
                        .recv_timeout(WAIT)
                        .expect("release running BlockingFn");
                    Ok(Value::Task(child))
                },
                Span::default(),
            )
        })
        .unwrap();
    let state = bind(&observer, &parent);
    let parent_owner = Arc::downgrade(&parent.owner);
    parent_begin.release();
    run_ready.recv_timeout(WAIT).unwrap();
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    assert_eq!(pool.snapshot().unwrap().running_blocking_jobs, 1);
    if foreign {
        let own = parent_runtime.snapshot().unwrap();
        assert_eq!(own.queued_blocking_jobs, 0);
        assert_eq!(own.running_blocking_jobs, 0);
        assert_eq!(pool.snapshot().unwrap().active_tasks, 0);
    }
    let original = saved_context();
    parent.release_scope_owner(original);
    drop(parent);
    assert!(until(|| parent_finished(&state)));
    assert_eq!(parent_owner.strong_count(), 0);
    assert_eq!(
        state.submission.load(Ordering::Acquire),
        SUBMISSION_ACCEPTED
    );
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    assert_eq!(parent_runtime.snapshot().unwrap().active_tasks, 0);
    run_release.release();
    let first = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(first.pending, 1);
    assert_all_parent_stages(&first);
    assert_eq!(first.cleanup, Some(original));
    assert!(first.execution.is_none());
    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    capture_release.release();
    let tail = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(tail.pending, 1);
    assert_all_parent_stages(&tail);
    assert_eq!(tail.cleanup, Some(original));
    assert!(until(|| state.pending_blocking.load(Ordering::Acquire) == 0));
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran_child.load(Ordering::Acquire));
    assert_eq!(child_owner.strong_count(), 0);
    assert_eq!(child_state.cancellation(), Some(original));
    assert!(current_cleanup_context().is_none());
    assert!(current_execution_termination().is_none());
    child_worker_release.release();
    blocker.await_result().unwrap();
    child_runtime.cancel_all_and_wait(WAIT).unwrap();
    assert!(child_state.accounting_done.load(Ordering::Acquire));
    pool.cancel_all_and_wait(WAIT).unwrap();
    parent_runtime.cancel_all_and_wait(WAIT).unwrap();
    let snapshot = pool.snapshot().unwrap();
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
}

#[test]
fn parent_g1_and_owner_completion_do_not_retire_running_job_or_its_late_result() {
    running_late_result_case(false);
}

#[test]
fn foreign_blocking_pool_charges_actual_current_parent_beyond_its_runtime_counters() {
    running_late_result_case(true);
}

fn blocking_rejection_case(parent_runtime: &TaskRuntime, pool: &TaskRuntime, code: &str) {
    let observer: ParentObserver = Arc::new(Mutex::new(None));
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(2);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let callback_ran = Arc::clone(&ran);
    let captures = (
        Probe {
            parent: Arc::clone(&observer),
            entered: entered_tx.clone(),
            release: Some(capture_rx),
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        },
        Probe {
            parent: Arc::clone(&observer),
            entered: entered_tx,
            release: None,
            drops: Arc::clone(&drops),
            shorter: None,
            panics: false,
        },
    );
    let (mut begin, begin_rx) = Release::pair();
    let work_pool = pool.clone();
    let parent = parent_runtime
        .spawn(move || {
            begin_rx
                .recv_timeout(WAIT)
                .expect("bind rejected BlockingFn observer");
            work_pool.run_blocking(
                move || {
                    callback_ran.store(true, Ordering::Release);
                    drop(captures);
                    Ok(Value::Null)
                },
                Span::default(),
            )
        })
        .unwrap();
    let state = bind(&observer, &parent);
    begin.release();
    let first = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(first.pending, 1);
    assert!(!first.execution_retired);
    assert!(!first.accounting_done);
    assert!(first.cleanup.is_none());
    assert!(first.execution.is_none());
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert!(!ran.load(Ordering::Acquire));
    capture_release.release();
    let tail = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(tail.pending, 1);
    assert!(!tail.accounting_done);
    assert_eq!(error_code(&parent.await_result().unwrap()), Some(code));
    assert!(until(|| state.accounting_done.load(Ordering::Acquire)));
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 0);
    assert_eq!(
        state.submission.load(Ordering::Acquire),
        SUBMISSION_ACCEPTED
    );
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(Arc::strong_count(&parent.owner), 1);
    drop(parent);
    assert!(state.owner_cleanup_done.load(Ordering::Acquire));
}

#[test]
fn queued_cancellation_keeps_count_through_capture_tail_and_native_worker_exit() {
    for panics in [false, true] {
        let _execution = ExecutionTerminationGuard::enter();
        let parent_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let pool = TaskRuntime::with_limits(0, 0, 1, 2, 1);
        let (exit_tx, exit_rx) = mpsc::sync_channel(1);
        let (occupier, mut worker_release) =
            held_blocking_with_exit(&pool, panics.then_some(exit_tx));
        let observer: ParentObserver = Arc::new(Mutex::new(None));
        let (mut capture_release, capture_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let drops = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let callback_ran = Arc::clone(&ran);
        let original = saved_context();
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + Duration::from_millis(100),
        };
        let expected = CancellationContext {
            reason: original.reason,
            cleanup_deadline: original.cleanup_deadline.min(shorter.cleanup_deadline),
        };
        let captures = (
            Probe {
                parent: Arc::clone(&observer),
                entered: entered_tx.clone(),
                release: Some(capture_rx),
                drops: Arc::clone(&drops),
                shorter: Some(shorter),
                panics,
            },
            Probe {
                parent: Arc::clone(&observer),
                entered: entered_tx,
                release: None,
                drops: Arc::clone(&drops),
                shorter: None,
                panics: false,
            },
        );
        let (mut begin, begin_rx) = Release::pair();
        let work_pool = pool.clone();
        let parent = parent_runtime
            .spawn(move || {
                begin_rx
                    .recv_timeout(WAIT)
                    .expect("bind queued BlockingFn observer");
                work_pool.run_blocking(
                    move || {
                        callback_ran.store(true, Ordering::Release);
                        drop(captures);
                        Ok(Value::Null)
                    },
                    Span::default(),
                )
            })
            .unwrap();
        let state = bind(&observer, &parent);
        begin.release();
        assert!(until(|| {
            state.pending_blocking.load(Ordering::Acquire) == 1
                && pool.inner.queued_blocking_jobs.load(Ordering::Acquire) == 1
        }));
        parent.release_scope_owner(original);
        drop(parent);
        assert!(until(|| parent_finished(&state)));
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
        worker_release.release();
        assert_eq!(occupier.join().unwrap().unwrap(), Value::Int(1));
        let first = entered_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(first.pending, 1);
        assert_all_parent_stages(&first);
        assert_eq!(first.cleanup, Some(original));
        assert!(first.execution.is_none());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
        capture_release.release();
        let tail = entered_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(tail.pending, 1);
        assert_all_parent_stages(&tail);
        assert_eq!(tail.cleanup, Some(expected));
        assert!(until(|| {
            state.pending_blocking.load(Ordering::Acquire) == 0
                && pool.inner.running_blocking_jobs.load(Ordering::Acquire) == 0
        }));
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(state.cancellation(), Some(original));
        if panics {
            // Preserve the existing uncaught cancellation-capture panic. The
            // unique real receiver is gone when this physical thread exits;
            // stale blocking_workers metadata is not patched by this test.
            exit_rx
                .recv_timeout(WAIT)
                .expect("actual blocking thread exited");
            blocking_rejection_case(&parent_runtime, &pool, "blocking_pool_stopped");
        } else {
            assert_eq!(
                pool.run_blocking(
                    || {
                        assert!(current_cleanup_context().is_none());
                        assert!(current_execution_termination().is_none());
                        Ok(Value::Int(9))
                    },
                    Span::default(),
                )
                .unwrap(),
                Value::Int(9)
            );
        }
        pool.cancel_all_and_wait(WAIT).unwrap();
        parent_runtime.cancel_all_and_wait(WAIT).unwrap();
        assert!(current_cleanup_context().is_none());
        assert!(current_execution_termination().is_none());
    }
}

#[test]
fn actual_blocking_full_rejection_keeps_count_until_both_capture_drops_finish() {
    for capacity in [0, 1] {
        let _execution = ExecutionTerminationGuard::enter();
        let parent_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let pool = TaskRuntime::with_limits(0, 0, 1, capacity, 1);
        let (occupier, mut worker_release) = held_blocking(&pool);
        let queued = if capacity == 0 {
            None
        } else {
            let queued_pool = pool.clone();
            let queued = thread::spawn(move || {
                queued_pool.run_blocking(|| Ok(Value::Int(2)), Span::default())
            });
            assert!(until(|| pool
                .inner
                .queued_blocking_jobs
                .load(Ordering::Acquire)
                == 1));
            Some(queued)
        };
        blocking_rejection_case(&parent_runtime, &pool, "queue_full");
        assert_eq!(pool.inner.running_blocking_jobs.load(Ordering::Acquire), 1);
        assert_eq!(
            pool.inner.queued_blocking_jobs.load(Ordering::Acquire),
            capacity
        );
        worker_release.release();
        assert_eq!(occupier.join().unwrap().unwrap(), Value::Int(1));
        if let Some(queued) = queued {
            assert_eq!(queued.join().unwrap().unwrap(), Value::Int(2));
        }
        pool.cancel_all_and_wait(WAIT).unwrap();
        parent_runtime.cancel_all_and_wait(WAIT).unwrap();
        assert_eq!(pool.inner.running_blocking_jobs.load(Ordering::Acquire), 0);
        assert_eq!(pool.inner.queued_blocking_jobs.load(Ordering::Acquire), 0);
    }
}

#[test]
fn actual_blocking_body_and_capture_panics_retire_before_healthy_followup() {
    for panic_in_body in [false, true] {
        let _execution = ExecutionTerminationGuard::enter();
        let runtime = TaskRuntime::with_limits(1, 2, 1, 2, 3);
        let observer: ParentObserver = Arc::new(Mutex::new(None));
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = (
            Probe {
                parent: Arc::clone(&observer),
                entered: entered_tx.clone(),
                release: None,
                drops: Arc::clone(&drops),
                shorter: None,
                panics: !panic_in_body,
            },
            Probe {
                parent: Arc::clone(&observer),
                entered: entered_tx,
                release: None,
                drops: Arc::clone(&drops),
                shorter: None,
                panics: false,
            },
        );
        let (mut begin, begin_rx) = Release::pair();
        let work_runtime = runtime.clone();
        let parent = runtime
            .spawn(move || {
                begin_rx
                    .recv_timeout(WAIT)
                    .expect("bind panicking BlockingFn observer");
                work_runtime.run_blocking(
                    move || {
                        if panic_in_body {
                            panic!("actual blocking callback body panic");
                        }
                        drop(captures);
                        Ok(Value::Null)
                    },
                    Span::default(),
                )
            })
            .unwrap();
        let state = bind(&observer, &parent);
        begin.release();
        for _ in 0..2 {
            let actual = entered_rx.recv_timeout(WAIT).unwrap();
            assert_eq!(actual.pending, 1);
            assert!(actual.cleanup.is_none());
            assert!(actual.execution.is_none());
        }
        let error = parent.await_result().unwrap_err();
        assert_eq!(error.domain.as_deref(), Some("task"));
        assert_eq!(error.code.as_deref(), Some("blocking_panic"));
        assert!(until(|| state.accounting_done.load(Ordering::Acquire)));
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 0);
        assert_eq!(
            state.submission.load(Ordering::Acquire),
            SUBMISSION_ACCEPTED
        );
        assert_eq!(drops.load(Ordering::Acquire), 2);
        let followup_runtime = runtime.clone();
        let healthy = runtime
            .spawn(move || {
                followup_runtime.run_blocking(
                    || {
                        assert!(current_cleanup_context().is_none());
                        assert!(current_execution_termination().is_none());
                        Ok(Value::Int(31))
                    },
                    Span::default(),
                )
            })
            .unwrap();
        assert_eq!(healthy.await_result().unwrap(), Value::Int(31));
        assert!(until(|| healthy
            .state
            .accounting_done
            .load(Ordering::Acquire)));
        assert_eq!(healthy.state.pending_blocking.load(Ordering::Acquire), 0);
        runtime.cancel_all_and_wait(WAIT).unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.queued_blocking_jobs, 0);
        assert_eq!(snapshot.running_blocking_jobs, 0);
        assert_eq!(snapshot.finished_tasks, 2);
    }
}
