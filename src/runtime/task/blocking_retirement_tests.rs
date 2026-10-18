use super::*;

const WAIT: Duration = Duration::from_secs(3);
const CAPTURE_PANIC: &str = "actual blocking capture destructor panic";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockingSubmissionCheckpoint {
    OpenChecked,
    BeforeUnlock,
}

type BlockingSubmissionHook = Box<dyn FnMut(BlockingSubmissionCheckpoint)>;

thread_local! {
    // Installed only by the submitting test thread. Other calls and workers
    // retain the original path without a scheduler hook or global fixture.
    static BLOCKING_SUBMISSION_HOOK: RefCell<Option<BlockingSubmissionHook>> = const { RefCell::new(None) };
}

pub(super) fn blocking_submission_checkpoint(stage: BlockingSubmissionCheckpoint) {
    BLOCKING_SUBMISSION_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(stage);
        }
    });
}

struct BlockingSubmissionHookGuard;

impl BlockingSubmissionHookGuard {
    fn install(hook: impl FnMut(BlockingSubmissionCheckpoint) + 'static) -> Self {
        BLOCKING_SUBMISSION_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
        Self
    }
}

impl Drop for BlockingSubmissionHookGuard {
    fn drop(&mut self) {
        BLOCKING_SUBMISSION_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

fn blocking_submission_child(test_name: &str) -> bool {
    use crate::registry_server::native_test_harness as bounded_process;

    const CHILD: &str = "KU_BLOCKING_SUBMISSION_CHILD";
    if std::env::var(CHILD).is_ok_and(|value| value == test_name) {
        return false;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, test_name);
    let output = bounded_process::run_bounded(
        &mut command,
        Duration::from_secs(20),
        bounded_process::OutputLimits::new(64 * 1024, 128 * 1024),
    )
    .expect("blocking submission regression has a bounded subprocess");
    assert!(
        output.status.success(),
        "blocking submission subprocess failed: {:?}\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let completed = format!("blocking submission regression completed {test_name}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line == completed),
        "blocking submission subprocess did not complete\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

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
    thread: thread::ThreadId,
    task_id: i64,
    task_cancelled: bool,
    task_cancellation: Option<CancellationContext>,
    task_state_present: bool,
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
        // A failed receiving-side assertion may already have closed this
        // diagnostic channel. Do not turn that failure into a second panic
        // while real tuple drop glue is unwinding the intended native panic.
        let _ = self.entered.send(Observation {
            pending: parent.pending_blocking.load(Ordering::Acquire),
            execution_retired: parent.execution_queue_retired.load(Ordering::Acquire),
            completion_done: parent.completion_cleanup_done.load(Ordering::Acquire),
            accounting_done: parent.accounting_done.load(Ordering::Acquire),
            owner_done: parent.owner_cleanup_done.load(Ordering::Acquire),
            cleanup: current_cleanup_context(),
            execution: current_execution_termination(),
            thread: thread::current().id(),
            task_id: current_task_id(),
            task_cancelled: current_task_cancelled(),
            task_cancellation: current_task_cancellation(),
            task_state_present: has_current_task_state(),
        });
        if let Some(release) = self.release.take() {
            let released = release.recv_timeout(WAIT).is_ok();
            // The fixture's Release guard runs if the receiving assertion
            // fails. A lost diagnostic handshake must not manufacture a
            // second native panic while the real callback body is unwinding.
            if !released && !thread::panicking() {
                panic!("release actual blocking capture Drop");
            }
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

fn has_current_task_state() -> bool {
    CURRENT_TASK_STATE.with(|slot| slot.borrow().is_some())
}

#[derive(Debug)]
struct BodyObservation {
    thread: thread::ThreadId,
    task_id: i64,
    task_cancelled: bool,
    task_cancellation: Option<CancellationContext>,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
    task_state_present: bool,
}

impl BodyObservation {
    fn current() -> Self {
        Self {
            thread: thread::current().id(),
            task_id: current_task_id(),
            task_cancelled: current_task_cancelled(),
            task_cancellation: current_task_cancellation(),
            cleanup: current_cleanup_context(),
            execution: current_execution_termination(),
            task_state_present: has_current_task_state(),
        }
    }

    fn assert_ordinary_blocking_identity(&self) {
        assert_eq!(self.task_id, 0);
        assert!(!self.task_cancelled);
        assert!(self.task_cancellation.is_none());
        assert!(self.cleanup.is_none());
        assert!(self.execution.is_none());
        assert!(!self.task_state_present);
    }
}

fn assert_blocking_probe_identity(observation: &Observation) {
    assert_eq!(observation.task_id, 0);
    assert!(!observation.task_cancelled);
    assert!(observation.task_cancellation.is_none());
    assert!(observation.execution.is_none());
    assert!(!observation.task_state_present);
}

fn running_implicit_child_owner_case(
    panic_in_body: bool,
    panic_in_capture: bool,
    ambient_owner_drop: bool,
) {
    assert!(!(panic_in_body && panic_in_capture));
    assert!(!ambient_owner_drop || !(panic_in_body || panic_in_capture));
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 1, 2, 3);
    let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (blocker, mut child_worker_release) = held_task(&child_runtime);
    let observer: ParentObserver = Arc::new(Mutex::new(None));
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(2);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran_child = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran_child);
    let original = saved_context();
    let shorter = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
    };
    let expected = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: shorter.cleanup_deadline,
    };
    let first_child_context = CancellationContext {
        reason: if ambient_owner_drop {
            TerminationReason::Cancelled
        } else {
            TerminationReason::TimedOut
        },
        cleanup_deadline: original.cleanup_deadline,
    };
    let expected_child_context = CancellationContext {
        cleanup_deadline: shorter.cleanup_deadline,
        ..first_child_context
    };
    let payload = (
        Probe {
            parent: Arc::clone(&observer),
            entered: entered_tx.clone(),
            release: Some(capture_rx),
            drops: Arc::clone(&drops),
            shorter: Some(shorter),
            panics: panic_in_capture,
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
            std::hint::black_box(&payload);
            Ok(Value::Null)
        })
        .unwrap();
    let child_state = Arc::clone(&child.state);
    let child_owner = Arc::downgrade(&child.owner);
    let (mut begin, begin_rx) = Release::pair();
    let (mut run_release, run_release_rx) = Release::pair();
    let (body_tx, body_rx) = mpsc::sync_channel(2);
    let pool = runtime.clone();
    let parent = runtime
        .spawn(move || {
            begin_rx
                .recv_timeout(WAIT)
                .map_err(|_| KuError::runtime("bind implicit owner parent", Span::default()))?;
            pool.run_blocking(
                move || {
                    // The three implicit cases only borrow the sole logical
                    // owner; return/unwind drop glue releases it. The separate
                    // ambient case below deliberately releases it in a guard.
                    std::hint::black_box(&child);
                    let _ = body_tx.send(BodyObservation::current());
                    run_release_rx.recv_timeout(WAIT).map_err(|_| {
                        KuError::runtime(
                            "release actual implicit-owner blocking body",
                            Span::default(),
                        )
                    })?;
                    let _ = body_tx.send(BodyObservation::current());
                    assert!(!panic_in_body, "actual implicit-owner blocking body panic");
                    if ambient_owner_drop {
                        // A later ambient candidate must preserve its own
                        // first cause while using the already-cancelled real
                        // parent's earlier D. This fixture does not extend
                        // the parent or the production cleanup allowance.
                        let _ambient = CleanupGuard::enter(CancellationContext {
                            reason: TerminationReason::Cancelled,
                            cleanup_deadline: original.cleanup_deadline + Duration::from_secs(1),
                        });
                        drop(child);
                    }
                    Ok(Value::Int(41))
                },
                Span::default(),
            )
        })
        .unwrap();
    let state = bind(&observer, &parent);
    let parent_owner = Arc::downgrade(&parent.owner);
    begin.release();
    let body = body_rx.recv_timeout(WAIT).unwrap();
    body.assert_ordinary_blocking_identity();
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    assert!(state.cancellation().is_none());
    parent.release_scope_owner(original);
    drop(parent);
    assert!(until(|| parent_finished(&state)));
    assert_eq!(parent_owner.strong_count(), 0);
    assert_eq!(state.cancellation(), Some(original));
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    run_release.release();
    let leaving_body = body_rx.recv_timeout(WAIT).unwrap();
    leaving_body.assert_ordinary_blocking_identity();
    assert_eq!(leaving_body.thread, body.thread);
    let first = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(first.thread, body.thread);
    assert_eq!(first.pending, 1);
    assert_all_parent_stages(&first);
    assert_blocking_probe_identity(&first);
    assert_eq!(first.cleanup, Some(first_child_context));
    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    capture_release.release();
    let tail = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(tail.thread, body.thread);
    assert_eq!(tail.pending, 1);
    assert_all_parent_stages(&tail);
    assert_blocking_probe_identity(&tail);
    assert_eq!(tail.cleanup, Some(expected_child_context));
    assert!(until(|| state.pending_blocking.load(Ordering::Acquire)
        == 0
        && runtime.inner.running_blocking_jobs.load(Ordering::Acquire)
            == 0));
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran_child.load(Ordering::Acquire));
    assert_eq!(child_owner.strong_count(), 0);
    assert_eq!(state.cancellation(), Some(expected));
    assert_eq!(child_state.cancellation(), Some(expected_child_context));

    // There is one actual blocking worker. Its next uncancelled callback also
    // releases a sole queued child implicitly, proving both catch recovery and
    // restoration of the private observer, beyond an empty callback check.
    let (mut next_release, next_rx) = Release::pair();
    let (next_entered_tx, next_entered_rx) = mpsc::sync_channel(2);
    let next_drops = Arc::new(AtomicUsize::new(0));
    let next_ran = Arc::new(AtomicBool::new(false));
    let next_child_ran = Arc::clone(&next_ran);
    let next_payload = (
        Probe {
            parent: Arc::clone(&observer),
            entered: next_entered_tx.clone(),
            release: Some(next_rx),
            drops: Arc::clone(&next_drops),
            shorter: None,
            panics: false,
        },
        Probe {
            parent: Arc::clone(&observer),
            entered: next_entered_tx,
            release: None,
            drops: Arc::clone(&next_drops),
            shorter: None,
            panics: false,
        },
    );
    let next_child = child_runtime
        .spawn(move || {
            next_child_ran.store(true, Ordering::Release);
            std::hint::black_box(&next_payload);
            Ok(Value::Null)
        })
        .unwrap();
    let next_child_state = Arc::clone(&next_child.state);
    let next_child_owner = Arc::downgrade(&next_child.owner);
    let (next_body_tx, next_body_rx) = mpsc::sync_channel(1);
    let next_pool = runtime.clone();
    let next = thread::spawn(move || {
        next_pool.run_blocking(
            move || {
                std::hint::black_box(&next_child);
                let _ = next_body_tx.send(BodyObservation::current());
                Ok(Value::Int(43))
            },
            Span::default(),
        )
    });
    let next_body = next_body_rx.recv_timeout(WAIT).unwrap();
    next_body.assert_ordinary_blocking_identity();
    assert_eq!(next_body.thread, body.thread);
    let next_first = next_entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(next_first.thread, body.thread);
    assert_eq!(next_first.pending, 0);
    assert_all_parent_stages(&next_first);
    assert_blocking_probe_identity(&next_first);
    let next_context = next_first.cleanup.expect("new child owner cleanup context");
    assert_eq!(next_context.reason, TerminationReason::Cancelled);
    assert!(next_context.cleanup_deadline > expected.cleanup_deadline);
    assert_eq!(state.cancellation(), Some(expected));
    assert_eq!(next_drops.load(Ordering::Acquire), 0);
    next_release.release();
    let next_tail = next_entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(next_tail.thread, body.thread);
    assert_eq!(next_tail.pending, 0);
    assert_all_parent_stages(&next_tail);
    assert_blocking_probe_identity(&next_tail);
    assert_eq!(next_tail.cleanup, Some(next_context));
    assert_eq!(next.join().unwrap().unwrap(), Value::Int(43));
    assert_eq!(next_drops.load(Ordering::Acquire), 2);
    assert!(!next_ran.load(Ordering::Acquire));
    assert_eq!(next_child_owner.strong_count(), 0);
    assert_eq!(next_child_state.cancellation(), Some(next_context));
    assert_eq!(state.cancellation(), Some(expected));
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 0);
    assert!(current_cleanup_context().is_none());
    assert!(current_execution_termination().is_none());
    assert!(!has_current_task_state());
    child_worker_release.release();
    blocker.await_result().unwrap();
    child_runtime.cancel_all_and_wait(WAIT).unwrap();
    runtime.cancel_all_and_wait(WAIT).unwrap();
    assert!(until(|| child_state
        .accounting_done
        .load(Ordering::Acquire)
        && next_child_state.accounting_done.load(Ordering::Acquire)));
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    assert_eq!(child_runtime.snapshot().unwrap().active_tasks, 0);
}

#[test]
fn actual_running_fn_once_implicit_child_owner_keeps_first_cause_and_restores_worker() {
    for (panic_in_body, panic_in_capture) in [(false, false), (false, true), (true, false)] {
        running_implicit_child_owner_case(panic_in_body, panic_in_capture, false);
    }
}

#[test]
fn actual_running_owner_cleanup_preserves_ambient_cause_and_minimum_parent_deadline() {
    running_implicit_child_owner_case(false, false, true);
}

struct HelperPanicRunner;

impl LocalTaskRunner for HelperPanicRunner {
    fn dispatch(
        &mut self,
        _budget: std::num::NonZeroUsize,
    ) -> std::ops::ControlFlow<KuResult<Value>> {
        std::ops::ControlFlow::Break(Ok(Value::Null))
    }
}

#[test]
fn actual_helped_runner_constructor_panic_isolates_blocking_parent_and_restores_observer() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 1, 2, 2);
    let helper_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 2);
    // Occupy both real Task workers, so only the actual blocking body can
    // claim the queued runner constructor through help_one.
    let (helper_blocker, mut helper_worker_release) = held_task(&helper_runtime);
    let (child_blocker, mut child_worker_release) = held_task(&child_runtime);
    let observer: ParentObserver = Arc::new(Mutex::new(None));
    let (mut capture_release, capture_rx) = Release::pair();
    let (entered_tx, entered_rx) = mpsc::sync_channel(2);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran_child = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran_child);
    let original = saved_context();
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
            std::hint::black_box(&payload);
            Ok(Value::Null)
        })
        .unwrap();
    let child_state = Arc::clone(&child.state);
    let child_owner = Arc::downgrade(&child.owner);
    let (constructor_tx, constructor_rx) = mpsc::sync_channel(1);
    let helper = helper_runtime
        .spawn_runner_deferred(move || {
            move || -> HelperPanicRunner {
                // The factory's genuine unwind releases this sole child
                // owner while the real helper identity is installed.
                std::hint::black_box(&child);
                let _ = constructor_tx.send(BodyObservation::current());
                panic!("actual helped runner constructor panic");
            }
        })
        .unwrap();
    assert_eq!(
        helper.state.submission.load(Ordering::Acquire),
        SUBMISSION_ACCEPTED
    );
    assert!(!helper.state.execution_queue_retired.load(Ordering::Acquire));
    let helper_id = helper.id();
    let (mut begin, begin_rx) = Release::pair();
    let (mut run_release, run_release_rx) = Release::pair();
    let (body_tx, body_rx) = mpsc::sync_channel(2);
    let pool = runtime.clone();
    let work_helper = helper_runtime.clone();
    let parent = runtime
        .spawn(move || {
            begin_rx
                .recv_timeout(WAIT)
                .expect("bind actual helper parent");
            pool.run_blocking(
                move || {
                    let _ = body_tx.send(BodyObservation::current());
                    run_release_rx
                        .recv_timeout(WAIT)
                        .expect("release actual blocking helper caller");
                    assert!(
                        work_helper.help_one().unwrap(),
                        "claim actual queued helper"
                    );
                    let _ = body_tx.send(BodyObservation::current());
                    Ok(Value::Int(47))
                },
                Span::default(),
            )
        })
        .unwrap();
    let state = bind(&observer, &parent);
    let parent_owner = Arc::downgrade(&parent.owner);
    begin.release();
    let body = body_rx.recv_timeout(WAIT).unwrap();
    body.assert_ordinary_blocking_identity();
    parent.release_scope_owner(original);
    drop(parent);
    assert!(until(|| parent_finished(&state)));
    assert_eq!(parent_owner.strong_count(), 0);
    assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
    assert_eq!(state.cancellation(), Some(original));
    run_release.release();
    let constructor = constructor_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(constructor.thread, body.thread);
    assert_eq!(constructor.task_id, helper_id);
    assert!(!constructor.task_cancelled);
    assert!(constructor.task_cancellation.is_none());
    assert!(constructor.cleanup.is_none());
    assert!(constructor.execution.is_none());
    assert!(constructor.task_state_present);
    let first = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(first.thread, body.thread);
    assert_eq!(first.pending, 1);
    assert_all_parent_stages(&first);
    assert_eq!(first.task_id, helper_id);
    assert!(!first.task_cancelled);
    assert!(first.task_cancellation.is_none());
    assert!(first.execution.is_none());
    assert!(first.task_state_present);
    let helper_child_context = first.cleanup.expect("actual helper child cleanup");
    assert_eq!(helper_child_context.reason, TerminationReason::Cancelled);
    assert!(helper_child_context.cleanup_deadline > original.cleanup_deadline);
    assert_eq!(state.cancellation(), Some(original));
    assert_eq!(drops.load(Ordering::Acquire), 0);
    capture_release.release();
    let tail = entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(tail.thread, body.thread);
    assert_eq!(tail.pending, 1);
    assert_all_parent_stages(&tail);
    assert_eq!(tail.task_id, helper_id);
    assert!(!tail.task_cancelled);
    assert!(tail.task_cancellation.is_none());
    assert!(tail.execution.is_none());
    assert!(tail.task_state_present);
    assert_eq!(tail.cleanup, Some(helper_child_context));
    let after_helper = body_rx.recv_timeout(WAIT).unwrap();
    after_helper.assert_ordinary_blocking_identity();
    assert_eq!(after_helper.thread, body.thread);
    assert!(until(|| state.pending_blocking.load(Ordering::Acquire)
        == 0
        && runtime.inner.running_blocking_jobs.load(Ordering::Acquire)
            == 0));
    assert_eq!(state.cancellation(), Some(original));
    assert_eq!(child_state.cancellation(), Some(helper_child_context));
    assert_eq!(child_owner.strong_count(), 0);
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran_child.load(Ordering::Acquire));
    assert!(helper.state.cancellation().is_none());
    assert_eq!(helper.status(), "panicked");
    assert_eq!(error_code(&helper.await_result().unwrap()), Some("panic"));
    assert!(helper.state.execution_queue_retired.load(Ordering::Acquire));
    assert!(helper.state.completion_cleanup_done.load(Ordering::Acquire));
    assert!(helper.state.accounting_done.load(Ordering::Acquire));
    assert_eq!(helper.state.pending_blocking.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime
            .run_blocking(
                || {
                    BodyObservation::current().assert_ordinary_blocking_identity();
                    Ok(Value::Int(49))
                },
                Span::default(),
            )
            .unwrap(),
        Value::Int(49)
    );
    assert!(current_cleanup_context().is_none());
    assert!(current_execution_termination().is_none());
    assert!(!has_current_task_state());
    drop(helper);
    helper_worker_release.release();
    child_worker_release.release();
    helper_blocker.await_result().unwrap();
    child_blocker.await_result().unwrap();
    helper_runtime.cancel_all_and_wait(WAIT).unwrap();
    child_runtime.cancel_all_and_wait(WAIT).unwrap();
    runtime.cancel_all_and_wait(WAIT).unwrap();
    assert!(until(|| child_state
        .accounting_done
        .load(Ordering::Acquire)));
    assert_eq!(runtime.snapshot().unwrap().active_tasks, 0);
    assert_eq!(helper_runtime.snapshot().unwrap().active_tasks, 0);
    assert_eq!(child_runtime.snapshot().unwrap().active_tasks, 0);
}

fn running_late_result_case(foreign: bool, panics: bool) {
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
    // Save the inherited window before constructing the child owner payload.
    // A shorter nested cancellation may tighten D, but never replace its cause.
    let original = saved_context();
    let shorter = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
    };
    let expected = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: shorter.cleanup_deadline,
    };
    let (exit_tx, exit_rx) = mpsc::sync_channel(1);
    let payload = (
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
                    if panics {
                        WORKER_EXIT.with(|slot| {
                            assert!(slot.borrow_mut().replace(WorkerExit(exit_tx)).is_none());
                        });
                    }
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
    let live_metrics = pool.snapshot().unwrap();
    assert_eq!(live_metrics.resident_blocking_jobs, 1);
    assert_eq!(live_metrics.accepted_blocking_jobs, 1);
    assert_eq!(live_metrics.finished_blocking_jobs, 0);
    if foreign {
        let own = parent_runtime.snapshot().unwrap();
        assert_eq!(own.queued_blocking_jobs, 0);
        assert_eq!(own.running_blocking_jobs, 0);
        assert_eq!(own.resident_blocking_jobs, 0);
        assert_eq!(own.accepted_blocking_jobs, 0);
        assert_eq!(own.finished_blocking_jobs, 0);
        assert_eq!(pool.snapshot().unwrap().active_tasks, 0);
    }
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
    assert_eq!(pool.snapshot().unwrap().resident_blocking_jobs, 1);
    assert_eq!(pool.snapshot().unwrap().finished_blocking_jobs, 0);
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
    assert_eq!(tail.cleanup, Some(expected));
    assert!(tail.execution.is_none());
    assert!(until(|| state.pending_blocking.load(Ordering::Acquire) == 0));
    assert_eq!(pool.snapshot().unwrap().resident_blocking_jobs, 0);
    assert_eq!(pool.snapshot().unwrap().finished_blocking_jobs, 1);
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran_child.load(Ordering::Acquire));
    assert_eq!(child_owner.strong_count(), 0);
    assert_eq!(state.cancellation(), Some(expected));
    assert_eq!(child_state.cancellation(), Some(expected));
    if panics {
        // The uncaught late-payload destructor panic retains the old worker
        // exit behavior. Observe that actual physical exit; do not repair its
        // stale blocking_workers metadata or claim the pool is still healthy.
        exit_rx
            .recv_timeout(WAIT)
            .expect("actual blocking thread exited after late payload panic");
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
    for panics in [false, true] {
        running_late_result_case(false, panics);
    }
}

#[test]
fn foreign_blocking_pool_charges_actual_current_parent_beyond_its_runtime_counters() {
    for panics in [false, true] {
        running_late_result_case(true, panics);
    }
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
    let metrics_before = pool.snapshot().unwrap();
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
    let rejecting = pool.snapshot().unwrap();
    assert_eq!(
        rejecting.resident_blocking_jobs,
        metrics_before.resident_blocking_jobs + 1
    );
    assert_eq!(
        rejecting.accepted_blocking_jobs,
        metrics_before.accepted_blocking_jobs
    );
    assert_eq!(
        rejecting.finished_blocking_jobs,
        metrics_before.finished_blocking_jobs
    );
    assert_eq!(
        rejecting.rejected_blocking_jobs,
        metrics_before.rejected_blocking_jobs + 1
    );
    assert_eq!(
        rejecting.rejected_blocking_queue,
        metrics_before.rejected_blocking_queue + usize::from(code == "queue_full")
    );
    assert!(rejecting.peak_resident_blocking_jobs >= rejecting.resident_blocking_jobs);
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
    let retired = pool.snapshot().unwrap();
    assert_eq!(
        retired.resident_blocking_jobs,
        metrics_before.resident_blocking_jobs
    );
    assert_eq!(
        retired.accepted_blocking_jobs,
        metrics_before.accepted_blocking_jobs
    );
    assert_eq!(
        retired.finished_blocking_jobs,
        metrics_before.finished_blocking_jobs
    );
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
            cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
        };
        let expected = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: shorter.cleanup_deadline,
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
        let queued_metrics = pool.snapshot().unwrap();
        assert_eq!(queued_metrics.accepted_blocking_jobs, 2);
        assert_eq!(queued_metrics.finished_blocking_jobs, 0);
        assert_eq!(queued_metrics.resident_blocking_jobs, 2);
        assert_eq!(queued_metrics.peak_resident_blocking_jobs, 2);
        assert!(queued_metrics.peak_queued_blocking_jobs >= 1);
        assert_eq!(queued_metrics.peak_running_blocking_jobs, 1);
        parent.release_scope_owner(original);
        drop(parent);
        assert!(until(|| parent_finished(&state)));
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
        let parent_retired = pool.snapshot().unwrap();
        assert_eq!(parent_retired.resident_blocking_jobs, 2);
        assert_eq!(parent_retired.finished_blocking_jobs, 0);
        worker_release.release();
        assert_eq!(occupier.join().unwrap().unwrap(), Value::Int(1));
        let first = entered_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(first.pending, 1);
        assert_all_parent_stages(&first);
        assert_eq!(first.cleanup, Some(original));
        assert!(first.execution.is_none());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
        let capture_held = pool.snapshot().unwrap();
        assert_eq!(capture_held.resident_blocking_jobs, 1);
        assert_eq!(capture_held.finished_blocking_jobs, 1);
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
        assert_eq!(state.cancellation(), Some(expected));
        let retired_metrics = pool.snapshot().unwrap();
        assert_eq!(retired_metrics.resident_blocking_jobs, 0);
        assert_eq!(retired_metrics.accepted_blocking_jobs, 2);
        assert_eq!(retired_metrics.finished_blocking_jobs, 2);
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
fn unsent_result_kernel_persists_shorter_cleanup_deadline_on_normal_and_native_drop() {
    // A direct kernel regression for the real send rejection cleanup window.
    // There is no BlockingJob here and no fabricated worker-send race: the
    // actual response receiver is already gone, and the parent/child are real
    // spawned tasks. Their pending_blocking count must remain exactly zero.
    for panics in [false, true] {
        let _execution = ExecutionTerminationGuard::enter();
        let parent_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 2);
        let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let (blocker, mut child_worker_release) = held_task(&child_runtime);
        let (parent, mut parent_run_release) = held_task(&parent_runtime);
        let observer: ParentObserver = Arc::new(Mutex::new(None));
        let state = bind(&observer, &parent);
        let parent_owner = Arc::downgrade(&parent.owner);
        let (mut capture_release, capture_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let drops = Arc::new(AtomicUsize::new(0));
        let ran_child = Arc::new(AtomicBool::new(false));
        let child_ran = Arc::clone(&ran_child);
        let original = saved_context();
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
        };
        let expected = CancellationContext {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        let payload = (
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
        let child = child_runtime
            .spawn(move || {
                child_ran.store(true, Ordering::Release);
                drop(payload);
                Ok(Value::Null)
            })
            .unwrap();
        let child_state = Arc::clone(&child.state);
        let child_owner = Arc::downgrade(&child.owner);
        parent.release_scope_owner(original);
        drop(parent);
        parent_run_release.release();
        assert!(until(|| parent_finished(&state)));
        assert_eq!(parent_owner.strong_count(), 0);
        assert_eq!(state.cancellation(), Some(original));
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 0);

        let (response, response_rx) = mpsc::sync_channel(1);
        drop(response_rx);
        let sending_parent = Arc::clone(&state);
        let sender = thread::spawn(move || {
            let _execution = ExecutionTerminationGuard::enter();
            assert!(current_cleanup_context().is_none());
            assert!(current_execution_termination().is_none());
            let result = catch_unwind(AssertUnwindSafe(|| {
                send_blocking_result(&response, Some(&sending_parent), Ok(Value::Task(child)));
            }));
            // Catch the original destructor panic only after all real drop
            // glue has unwound; tail payload has no panic and cannot double
            // panic. The cleanup window must restore this physical thread.
            assert!(current_cleanup_context().is_none());
            assert!(current_execution_termination().is_none());
            match result {
                Ok(()) => assert!(!panics, "native destructor panic was swallowed"),
                Err(payload) => {
                    assert!(panics, "ordinary unsent cleanup unexpectedly panicked");
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied());
                    assert_eq!(message, Some(CAPTURE_PANIC));
                }
            }
        });
        let first = entered_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(first.pending, 0);
        assert_all_parent_stages(&first);
        assert_eq!(first.cleanup, Some(original));
        assert!(first.execution.is_none());
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        capture_release.release();
        let tail = entered_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(tail.pending, 0);
        assert_all_parent_stages(&tail);
        assert_eq!(tail.cleanup, Some(expected));
        assert!(tail.execution.is_none());
        sender.join().unwrap();
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert_eq!(state.cancellation(), Some(expected));
        assert_eq!(child_state.cancellation(), Some(expected));
        assert_eq!(child_owner.strong_count(), 0);
        assert!(!ran_child.load(Ordering::Acquire));
        assert!(current_cleanup_context().is_none());
        assert!(current_execution_termination().is_none());
        child_worker_release.release();
        blocker.await_result().unwrap();
        child_runtime.cancel_all_and_wait(WAIT).unwrap();
        parent_runtime.cancel_all_and_wait(WAIT).unwrap();
        assert!(child_state.accounting_done.load(Ordering::Acquire));
        assert_eq!(parent_runtime.snapshot().unwrap().active_tasks, 0);
        assert_eq!(child_runtime.snapshot().unwrap().active_tasks, 0);
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
fn shutdown_keeps_admitted_blocking_work_after_late_retirement_before_registry_unlock() {
    const TEST: &str = "runtime::task::blocking_retirement_tests::shutdown_keeps_admitted_blocking_work_after_late_retirement_before_registry_unlock";
    if blocking_submission_child(TEST) {
        return;
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    let _execution = ExecutionTerminationGuard::enter();
    let pool = TaskRuntime::with_limits(0, 0, 1, 1, 0);
    let (mut open_release, open_rx) = Release::pair();
    let (open_tx, opened) = mpsc::sync_channel(1);
    let (mut registry_release, registry_rx) = Release::pair();
    let (retired_tx, retired_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let callback_ran = Arc::clone(&ran);
    let capture = Capture(Arc::clone(&drops));
    let submitting_pool = pool.clone();
    let inner = Arc::clone(&pool.inner);
    let caller = thread::spawn(move || {
        let _hook = BlockingSubmissionHookGuard::install(move |stage| match stage {
            BlockingSubmissionCheckpoint::OpenChecked => {
                open_tx.send(()).expect("publish real open admission");
                open_rx
                    .recv_timeout(WAIT)
                    .expect("release real open admission");
            }
            BlockingSubmissionCheckpoint::BeforeUnlock => {
                // Receiving the response is not retirement. Observe all real
                // worker obligations while retaining the actual registry lock.
                let retired = until(|| {
                    inner.blocking_metrics.resident.load(Ordering::Acquire) == 0
                        && inner.queued_blocking_jobs.load(Ordering::Acquire) == 0
                        && inner.running_blocking_jobs.load(Ordering::Acquire) == 0
                });
                retired_tx
                    .send(retired)
                    .expect("publish actual blocking retirement");
                registry_rx
                    .recv_timeout(WAIT)
                    .expect("release real registry admission");
            }
        });
        submitting_pool.run_blocking(
            move || {
                callback_ran.store(true, Ordering::Release);
                drop(capture);
                Ok(Value::Int(7))
            },
            Span::default(),
        )
    });

    let open_seen = opened.recv_timeout(WAIT).is_ok();
    let before = (
        pool.inner.active_tasks.load(Ordering::Acquire),
        pool.inner.queued_blocking_jobs.load(Ordering::Acquire),
        pool.inner.running_blocking_jobs.load(Ordering::Acquire),
        pool.inner.blocking_metrics.resident.load(Ordering::Acquire),
    );
    let shutdown_pool = pool.clone();
    let shutdown = thread::spawn(move || shutdown_pool.cancel_all_and_wait(Duration::ZERO));
    // This atomic close follows the original initial counter observations.
    // The shutdown then waits for the submitter's real states mutex.
    let closed_seen =
        until(|| pool.inner.producer_gate.load(Ordering::Acquire) & PRODUCER_CLOSED_BIT != 0);
    open_release.release();
    let worker_retired = retired_rx.recv_timeout(WAIT).ok();
    let after_worker = (
        pool.inner.queued_blocking_jobs.load(Ordering::Acquire),
        pool.inner.running_blocking_jobs.load(Ordering::Acquire),
        pool.inner.blocking_metrics.resident.load(Ordering::Acquire),
    );
    registry_release.release();
    let response = caller.join();
    let shutdown = shutdown.join();
    let drained = pool.cancel_all_and_wait(WAIT);
    let empty_zero = pool.cancel_all_and_wait(Duration::ZERO);
    let final_snapshot = pool.snapshot();

    // All channel gates, callers and worker obligations have been released
    // before either the old-code failure or the fixed-code result is asserted.
    eprintln!("blocking admission race open={open_seen} closed={closed_seen} before={before:?} retired={worker_retired:?} after_worker={after_worker:?} response={response:?} shutdown={shutdown:?} drained={drained:?} empty_zero={empty_zero:?} final={final_snapshot:?}");
    assert!(open_seen && closed_seen);
    assert_eq!(before, (0, 0, 0, 0));
    assert_eq!(worker_retired, Some(true));
    assert_eq!(after_worker, (0, 0, 0));
    assert_eq!(response.unwrap().unwrap(), Value::Int(7));
    assert!(ran.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(
        shutdown.unwrap().unwrap_err().code.as_deref(),
        Some("shutdown_timeout")
    );
    assert_eq!(drained.unwrap(), 0);
    assert_eq!(empty_zero.unwrap(), 0);
    let final_snapshot = final_snapshot.unwrap();
    assert_eq!(final_snapshot.accepted_blocking_jobs, 1);
    assert_eq!(final_snapshot.finished_blocking_jobs, 1);
    assert_eq!(final_snapshot.resident_blocking_jobs, 0);
    assert_eq!(final_snapshot.running_blocking_jobs, 0);
    assert_eq!(final_snapshot.queued_blocking_jobs, 0);
    eprintln!("blocking submission regression completed {TEST}");
}

#[test]
fn shutdown_tracks_early_blocking_capture_drop_without_reopening_closed_prefix() {
    const TEST: &str = "runtime::task::blocking_retirement_tests::shutdown_tracks_early_blocking_capture_drop_without_reopening_closed_prefix";
    if blocking_submission_child(TEST) {
        return;
    }

    struct HeldCapture {
        entered: SyncSender<()>,
        release: Receiver<()>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for HeldCapture {
        fn drop(&mut self) {
            let _ = self.entered.try_send(());
            let released = self.release.recv_timeout(WAIT).is_ok();
            self.drops.fetch_add(1, Ordering::AcqRel);
            if !released && !thread::panicking() {
                panic!("release real early blocking capture");
            }
        }
    }

    let _execution = ExecutionTerminationGuard::enter();
    let pool = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let independent_empty = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    let initially_empty = independent_empty.cancel_all_and_wait(Duration::ZERO);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let mut outcomes = Vec::new();

    // First the real no-worker rejection starts before close; then an actual
    // post-close call drops its captures without reopening the completed prefix.
    for after_close in [false, true] {
        let (mut release, release_rx) = Release::pair();
        let (entered, ready) = mpsc::sync_channel(1);
        let capture = HeldCapture {
            entered,
            release: release_rx,
            drops: Arc::clone(&drops),
        };
        let callback_ran = Arc::clone(&ran);
        let submitting_pool = pool.clone();
        let caller = thread::spawn(move || {
            submitting_pool.run_blocking(
                move || {
                    callback_ran.store(true, Ordering::Release);
                    drop(capture);
                    Ok(Value::Null)
                },
                Span::default(),
            )
        });
        let capture_entered = ready.recv_timeout(WAIT).is_ok();
        let while_drop = pool.snapshot();
        let shutdown = pool.cancel_all_and_wait(Duration::ZERO);
        release.release();
        let response = caller.join();
        let drained = pool.cancel_all_and_wait(WAIT);
        outcomes.push((
            after_close,
            capture_entered,
            while_drop,
            shutdown,
            response,
            drained,
        ));
    }
    let final_empty = pool.cancel_all_and_wait(Duration::ZERO);
    let final_snapshot = pool.snapshot();
    eprintln!("early blocking capture outcomes={outcomes:?} initial_empty={initially_empty:?} final_empty={final_empty:?} final={final_snapshot:?}");

    // Cleanup and join both paths before reporting the missing early-drop
    // responsibility on the original implementation.
    assert_eq!(initially_empty.unwrap(), 0);
    assert_eq!(final_empty.unwrap(), 0);
    assert_eq!(drops.load(Ordering::Acquire), 2);
    assert!(!ran.load(Ordering::Acquire));
    for (after_close, capture_entered, while_drop, shutdown, response, drained) in outcomes {
        assert!(capture_entered);
        let while_drop = while_drop.unwrap();
        assert_eq!(while_drop.active_tasks, 0);
        assert_eq!(while_drop.resident_blocking_jobs, 0);
        assert_eq!(while_drop.running_blocking_jobs, 0);
        assert_eq!(while_drop.queued_blocking_jobs, 0);
        assert_eq!(
            error_code(&response.unwrap().unwrap()),
            Some("blocking_pool_stopped")
        );
        if after_close {
            assert_eq!(shutdown.unwrap(), 0);
        } else {
            assert_eq!(
                shutdown.unwrap_err().code.as_deref(),
                Some("shutdown_timeout")
            );
        }
        assert_eq!(drained.unwrap(), 0);
    }
    let final_snapshot = final_snapshot.unwrap();
    assert_eq!(final_snapshot.accepted_blocking_jobs, 0);
    assert_eq!(final_snapshot.finished_blocking_jobs, 0);
    assert_eq!(final_snapshot.rejected_blocking_jobs, 2);
    assert_eq!(final_snapshot.resident_blocking_jobs, 0);
    assert_eq!(final_snapshot.total_submissions, 0);
    eprintln!("blocking submission regression completed {TEST}");
}

#[test]
fn shutdown_waits_for_rejected_blocking_capture_after_other_jobs_retire() {
    struct HeldCapture {
        entered: SyncSender<()>,
        release: Receiver<()>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for HeldCapture {
        fn drop(&mut self) {
            let _ = self.entered.try_send(());
            let released = self.release.recv_timeout(WAIT).is_ok();
            if !released && !thread::panicking() {
                panic!("release rejected blocking capture");
            }
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    let budget = Duration::from_millis(10);
    let mut outcomes = Vec::new();
    for foreign_parent in [true, false] {
        let _execution = ExecutionTerminationGuard::enter();
        let parent_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let pool = TaskRuntime::with_limits(0, 0, 1, 1, 1);
        let (occupier, mut worker_release) = held_blocking(&pool);
        let queued_pool = pool.clone();
        let queued =
            thread::spawn(move || queued_pool.run_blocking(|| Ok(Value::Int(2)), Span::default()));
        assert!(until(|| pool
            .inner
            .queued_blocking_jobs
            .load(Ordering::Acquire)
            == 1));

        let (mut capture_release, release) = Release::pair();
        let (entered, ready) = mpsc::sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = HeldCapture {
            entered,
            release,
            drops: Arc::clone(&drops),
        };
        let ran = Arc::new(AtomicBool::new(false));
        let callback_ran = Arc::clone(&ran);
        let rejected_pool = pool.clone();
        let reject = move || {
            rejected_pool.run_blocking(
                move || {
                    callback_ran.store(true, Ordering::Release);
                    drop(capture);
                    Ok(Value::Int(3))
                },
                Span::default(),
            )
        };
        let (parent, caller) = if foreign_parent {
            (Some(parent_runtime.spawn(reject).unwrap()), None)
        } else {
            (None, Some(thread::spawn(reject)))
        };
        ready.recv_timeout(WAIT).unwrap();
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Acquire), 0);
        if let Some(parent) = &parent {
            // Its real Task belongs to another runtime, so the pool's active
            // counter cannot stand in for this resident job's final capture.
            assert_eq!(parent.state.pending_blocking.load(Ordering::Acquire), 1);
            assert_eq!(parent_runtime.snapshot().unwrap().active_tasks, 1);
        } else {
            assert_eq!(parent_runtime.snapshot().unwrap().active_tasks, 0);
        }

        worker_release.release();
        assert_eq!(occupier.join().unwrap().unwrap(), Value::Int(1));
        assert_eq!(queued.join().unwrap().unwrap(), Value::Int(2));
        assert!(until(|| {
            let snapshot = pool.snapshot().unwrap();
            snapshot.queued_blocking_jobs == 0
                && snapshot.running_blocking_jobs == 0
                && snapshot.resident_blocking_jobs == 1
        }));
        let held = pool.snapshot().unwrap();
        assert_eq!(pool.inner.producer_count(), 0);
        assert_eq!(held.active_tasks, 0);
        assert_eq!(held.accepted_blocking_jobs, 2);
        assert_eq!(held.finished_blocking_jobs, 2);
        assert_eq!(held.rejected_blocking_jobs, 1);
        assert_eq!(held.rejected_blocking_queue, 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);

        let started = Instant::now();
        let shutdown = pool.cancel_all_and_wait(budget);
        let elapsed = started.elapsed();
        assert_eq!(pool.snapshot().unwrap().resident_blocking_jobs, 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        capture_release.release();
        let result = if let Some(parent) = parent {
            parent.await_result().unwrap()
        } else {
            caller.unwrap().join().unwrap().unwrap()
        };
        assert_eq!(error_code(&result), Some("queue_full"));
        assert!(until(
            || pool.snapshot().unwrap().resident_blocking_jobs == 0
        ));
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(pool.cancel_all_and_wait(Duration::ZERO).unwrap(), 0);
        parent_runtime.cancel_all_and_wait(WAIT).unwrap();
        outcomes.push((foreign_parent, shutdown, elapsed));
    }

    // Complete both real rejection paths and their cleanup before reporting
    // either failure; the old empty-counter exit incorrectly succeeds in both.
    assert!(
        outcomes.iter().all(|(_, shutdown, elapsed)| {
            shutdown.as_ref().is_err_and(|error| {
                error.domain.as_deref() == Some("task")
                    && error.code.as_deref() == Some("shutdown_timeout")
            }) && *elapsed >= budget
        }),
        "resident capture shutdown outcomes: {outcomes:?}"
    );
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
        // G1 and response delivery can finish before the real blocking job's
        // last field. Await both independent publications within the same
        // fixture bound, rather than treating Task accounting as a B2 ACK.
        assert!(until(|| state.accounting_done.load(Ordering::Acquire)
            && state.pending_blocking.load(Ordering::Acquire) == 0));
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
            .load(Ordering::Acquire)
            && healthy.state.pending_blocking.load(Ordering::Acquire) == 0));
        assert_eq!(healthy.state.pending_blocking.load(Ordering::Acquire), 0);
        runtime.cancel_all_and_wait(WAIT).unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.queued_blocking_jobs, 0);
        assert_eq!(snapshot.running_blocking_jobs, 0);
        assert_eq!(snapshot.finished_tasks, 2);
        assert_eq!(snapshot.resident_blocking_jobs, 0);
        assert_eq!(snapshot.accepted_blocking_jobs, 2);
        assert_eq!(snapshot.finished_blocking_jobs, 2);
        assert_eq!(snapshot.rejected_blocking_jobs, 0);
        assert_eq!(snapshot.rejected_blocking_queue, 0);
        assert_eq!(snapshot.peak_running_blocking_jobs, 1);
    }
}

#[test]
fn blocking_metrics_reconcile_fast_consumers_and_stopped_rejections() {
    let runtime = TaskRuntime::with_limits(0, 0, 2, 16, 0);
    let mut callers = Vec::new();
    let (done_tx, done_rx) = mpsc::sync_channel(8);
    for producer in 0..8 {
        let pool = runtime.clone();
        let done = done_tx.clone();
        callers.push(thread::spawn(move || {
            for index in 0..32 {
                let expected = producer * 32 + index;
                assert_eq!(
                    pool.run_blocking(move || Ok(Value::Int(expected)), Span::default())
                        .unwrap(),
                    Value::Int(expected)
                );
            }
            done.send(()).unwrap();
        }));
    }
    drop(done_tx);
    for _ in 0..8 {
        done_rx
            .recv_timeout(WAIT)
            .expect("fast blocking caller completed");
    }
    for caller in callers {
        caller.join().unwrap();
    }
    assert!(until(|| {
        let snapshot = runtime.snapshot().unwrap();
        snapshot.resident_blocking_jobs == 0
            && snapshot.queued_blocking_jobs == 0
            && snapshot.running_blocking_jobs == 0
    }));
    let finished = runtime.snapshot().unwrap();
    assert_eq!(finished.accepted_blocking_jobs, 256);
    assert_eq!(finished.finished_blocking_jobs, 256);
    assert_eq!(finished.rejected_blocking_jobs, 0);
    assert_eq!(finished.rejected_blocking_queue, 0);
    // Eight callers can begin their next jobs after delivery while the two
    // workers still retire their previous jobs' final fields.
    assert!((1..=10).contains(&finished.peak_resident_blocking_jobs));
    assert!((1..=8).contains(&finished.peak_queued_blocking_jobs));
    assert!((1..=2).contains(&finished.peak_running_blocking_jobs));
    assert_eq!(finished.active_tasks, 0);
    assert_eq!(finished.finished_tasks, 0);
    runtime.cancel_all_and_wait(WAIT).unwrap();
    assert_eq!(
        error_code(
            &runtime
                .run_blocking(|| panic!("closed pool ran callback"), Span::default())
                .unwrap()
        ),
        Some("blocking_pool_stopped")
    );
    let closed = runtime.snapshot().unwrap();
    assert_eq!(closed.rejected_blocking_jobs, 1);
    assert_eq!(closed.rejected_blocking_queue, 0);
    assert_eq!(closed.resident_blocking_jobs, 0);
    assert_eq!(closed.accepted_blocking_jobs, 256);
    assert_eq!(closed.finished_blocking_jobs, 256);
    assert_eq!(
        closed.peak_resident_blocking_jobs,
        finished.peak_resident_blocking_jobs
    );
    assert_eq!(
        closed.peak_queued_blocking_jobs,
        finished.peak_queued_blocking_jobs
    );
    assert_eq!(
        closed.peak_running_blocking_jobs,
        finished.peak_running_blocking_jobs
    );

    let empty = TaskRuntime::with_limits(0, 0, 0, 0, 0);
    assert_eq!(
        error_code(
            &empty
                .run_blocking(|| panic!("empty pool ran callback"), Span::default())
                .unwrap()
        ),
        Some("blocking_pool_stopped")
    );
    let stopped = empty.snapshot().unwrap();
    assert_eq!(stopped.rejected_blocking_jobs, 1);
    assert_eq!(stopped.resident_blocking_jobs, 0);
    assert_eq!(stopped.accepted_blocking_jobs, 0);
    assert_eq!(stopped.finished_blocking_jobs, 0);
    assert_eq!(stopped.peak_resident_blocking_jobs, 0);
    assert_eq!(stopped.peak_queued_blocking_jobs, 0);
    assert_eq!(stopped.peak_running_blocking_jobs, 0);
}
