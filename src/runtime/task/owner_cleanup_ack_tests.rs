use super::*;
use crate::env::Env;

const WAIT: Duration = Duration::from_secs(3);

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
    let (release, rx) = Release::pair();
    let (started, ready) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            started.send(()).unwrap();
            rx.recv_timeout(WAIT).expect("release held worker");
            Ok(Value::Null)
        })
        .unwrap();
    ready.recv_timeout(WAIT).unwrap();
    (task, release)
}

fn completed(runtime: &TaskRuntime, value: Value) -> TaskHandle {
    let task = runtime.spawn(move || Ok(value)).unwrap();
    assert!(until(|| task.state.accounting_done.load(Ordering::Acquire)));
    task
}

fn owning_function(child: TaskHandle) -> Value {
    let mut captures = Env::new();
    captures
        .define_owned("child".into(), Value::Task(child), false, Span::default())
        .unwrap();
    Value::Function {
        params: Vec::new(),
        param_modes: Vec::new(),
        body: Vec::new(),
        captures,
        self_name: None,
        is_async: false,
    }
}

fn context() -> CancellationContext {
    CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now() + WAIT,
    }
}

struct Probe {
    entered: SyncSender<Option<CancellationContext>>,
    release: Receiver<()>,
    finished: SyncSender<Option<CancellationContext>>,
    drops: Arc<AtomicUsize>,
    shorter: CancellationContext,
    panics: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.entered.send(current_cleanup_context()).unwrap();
        self.release
            .recv_timeout(WAIT)
            .expect("release actual payload drop");
        drop(CleanupGuard::enter(self.shorter));
        self.drops.fetch_add(1, Ordering::AcqRel);
        self.finished.send(current_cleanup_context()).unwrap();
        if self.panics {
            panic!("actual owner payload native drop panic");
        }
    }
}

struct Tail(Arc<AtomicUsize>);
impl Drop for Tail {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
struct Payload {
    _probe: Probe,
    _tail: Tail,
}

fn panic_message(panic: &CleanupPanic) -> Option<&str> {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
}

#[test]
fn real_payload_drop_and_duplicate_release_do_not_publish_early_owner_ack() {
    for panics in [false, true] {
        let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let parent_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let (blocker, mut worker_release) = held_worker(&child_runtime);
        let saved = context();
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + Duration::from_secs(1),
        };
        let (mut payload_release, release_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let tails = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let child_ran = Arc::clone(&ran);
        let payload = Payload {
            _probe: Probe {
                entered: entered_tx,
                release: release_rx,
                finished: finished_tx,
                drops: Arc::clone(&drops),
                shorter,
                panics,
            },
            _tail: Tail(Arc::clone(&tails)),
        };
        let child = child_runtime
            .spawn(move || {
                child_ran.store(true, Ordering::Release);
                drop(payload);
                Ok(Value::Null)
            })
            .unwrap();
        let child_state = Arc::clone(&child.state);
        let child_owner = Arc::downgrade(&child.owner);
        let parent = completed(&parent_runtime, owning_function(child));
        let parent_state = Arc::clone(&parent.state);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let releasing = thread::spawn(move || {
            let runtime = parent.runtime.clone();
            let result = catch_unwind(AssertUnwindSafe(|| parent.release_scope_owner(saved)));
            drop(parent);
            let clean = current_cleanup_context().is_none()
                && !OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get)
                && OWNER_RESULT_FLUSH_DEPTH.with(Cell::get) == 0
                && OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none())
                && OWNER_RESULT_FINISHED_HEAD.with(|head| head.borrow().is_none());
            // Reuse this physical thread after either a normal drain or native
            // panic. A leaked ACTIVE/depth/head would strand the next owner.
            let fresh = completed(&runtime, Value::String("next owner".into()));
            fresh.release_scope_owner(saved);
            let fresh_done = fresh.state.owner_cleanup_done.load(Ordering::Acquire);
            drop(fresh);
            done_tx.send((result, clean, fresh_done)).unwrap();
        });
        let actual_context = entered_rx.recv_timeout(WAIT).unwrap();
        let before = (
            parent_state.owner_released.load(Ordering::Acquire),
            parent_state.owner_cleanup_done.load(Ordering::Acquire),
            parent_state.owner_payload_done.load(Ordering::Acquire),
            parent_state.accounting_done.load(Ordering::Acquire),
            parent_state.result.lock().unwrap().is_none(),
        );
        parent_state.release_owner(saved);
        child_state.release_owner(saved);
        let duplicate_done = parent_state.owner_cleanup_done.load(Ordering::Acquire)
            || child_state.owner_cleanup_done.load(Ordering::Acquire);
        payload_release.release();
        let (result, clean, fresh_done) = done_rx.recv_timeout(WAIT).unwrap();
        releasing.join().unwrap();
        let finished_context = finished_rx.recv_timeout(WAIT).unwrap();
        worker_release.release();
        blocker.await_result().unwrap();
        child_runtime.cancel_all_and_wait(WAIT).unwrap();
        parent_runtime.cancel_all_and_wait(WAIT).unwrap();
        assert_eq!(actual_context, Some(saved));
        assert_eq!(before, (true, false, false, true, true));
        assert!(!duplicate_done);
        assert!(clean);
        assert!(fresh_done);
        assert!(parent_state.owner_cleanup_done.load(Ordering::Acquire));
        assert!(child_state.owner_cleanup_done.load(Ordering::Acquire));
        assert_eq!(child_owner.strong_count(), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert_eq!(tails.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(
            finished_context,
            Some(CancellationContext {
                cleanup_deadline: shorter.cleanup_deadline,
                ..saved
            })
        );
        if panics {
            assert_eq!(
                panic_message(&result.expect_err("original native panic propagates")),
                Some("actual owner payload native drop panic")
            );
        } else {
            result.unwrap();
        }
        assert_eq!(child_runtime.snapshot().unwrap().finished_tasks, 2);
        assert_eq!(parent_runtime.snapshot().unwrap().finished_tasks, 2);
        assert!(current_cleanup_context().is_none());
    }
}

#[test]
fn deferred_parent_chain_waits_for_the_actual_leaf_drop_before_payload_ack() {
    for panics in [false, true] {
        let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let parent_runtime = TaskRuntime::with_limits(1, 4, 0, 0, 6);
        let (blocker, mut worker_release) = held_worker(&child_runtime);
        let saved = context();
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: Instant::now() + Duration::from_secs(1),
        };
        let (mut payload_release, release_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let tails = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let child_ran = Arc::clone(&ran);
        let payload = Payload {
            _probe: Probe {
                entered: entered_tx,
                release: release_rx,
                finished: finished_tx,
                drops: Arc::clone(&drops),
                shorter,
                panics,
            },
            _tail: Tail(Arc::clone(&tails)),
        };
        let child = child_runtime
            .spawn(move || {
                child_ran.store(true, Ordering::Release);
                drop(payload);
                Ok(Value::Null)
            })
            .unwrap();
        let child_state = Arc::clone(&child.state);
        let leaf = completed(&parent_runtime, owning_function(child));
        let leaf_state = Arc::clone(&leaf.state);
        let middle = completed(&parent_runtime, Value::Task(leaf));
        let middle_state = Arc::clone(&middle.state);
        let top = completed(&parent_runtime, Value::Task(middle));
        let top_state = Arc::clone(&top.state);
        let states = [&top_state, &middle_state, &leaf_state];
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let releasing = thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| top.release_scope_owner(saved)));
            drop(top);
            let clean = current_cleanup_context().is_none()
                && !OWNER_RESULT_RELEASE_ACTIVE.with(Cell::get)
                && OWNER_RESULT_FLUSH_DEPTH.with(Cell::get) == 0
                && OWNER_RESULT_RELEASE_HEAD.with(|head| head.borrow().is_none())
                && OWNER_RESULT_FINISHED_HEAD.with(|head| head.borrow().is_none());
            done_tx.send((result, clean)).unwrap();
        });
        let actual_context = entered_rx.recv_timeout(WAIT).unwrap();
        let before = states.map(|state| {
            (
                state.owner_released.load(Ordering::Acquire),
                state.owner_payload_done.load(Ordering::Acquire),
                state.owner_cleanup_done.load(Ordering::Acquire),
                state.accounting_done.load(Ordering::Acquire),
            )
        });
        // A duplicate release must not drain another thread's inline nodes or
        // publish its still-live payload obligation.
        for state in states {
            state.release_owner(saved);
        }
        let duplicate_done = states.iter().any(|state| {
            state.owner_payload_done.load(Ordering::Acquire)
                || state.owner_cleanup_done.load(Ordering::Acquire)
        });
        payload_release.release();
        let (result, clean) = done_rx.recv_timeout(WAIT).unwrap();
        releasing.join().unwrap();
        let actual_finished = finished_rx.recv_timeout(WAIT).unwrap();
        worker_release.release();
        blocker.await_result().unwrap();
        child_runtime.cancel_all_and_wait(WAIT).unwrap();
        parent_runtime.cancel_all_and_wait(WAIT).unwrap();
        assert_eq!(actual_context, Some(saved));
        assert_eq!(before, [(true, false, false, true); 3]);
        assert!(!duplicate_done);
        assert!(clean);
        for state in states {
            assert!(state.owner_cleanup_done.load(Ordering::Acquire));
            assert!(state.owner_result_release.lock().unwrap().is_none());
        }
        assert!(child_state.owner_cleanup_done.load(Ordering::Acquire));
        assert_eq!(
            actual_finished,
            Some(CancellationContext {
                cleanup_deadline: shorter.cleanup_deadline,
                ..saved
            })
        );
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert_eq!(tails.load(Ordering::Acquire), 1);
        assert!(!ran.load(Ordering::Acquire));
        if panics {
            assert_eq!(
                panic_message(&result.expect_err("first native panic propagates")),
                Some("actual owner payload native drop panic")
            );
        } else {
            result.unwrap();
        }
        assert_eq!(child_runtime.snapshot().unwrap().finished_tasks, 2);
        assert_eq!(parent_runtime.snapshot().unwrap().finished_tasks, 3);
    }
}

#[test]
fn running_task_owner_ack_is_separate_from_late_completion_and_accounting() {
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (blocker, mut worker_release) = held_worker(&child_runtime);
    let child = child_runtime.spawn(|| Ok(Value::Null)).unwrap();
    let child_state = Arc::clone(&child.state);
    let (mut finish, finish_rx) = Release::pair();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv_timeout(WAIT).unwrap();
            Ok(owning_function(child))
        })
        .unwrap();
    started_rx.recv_timeout(WAIT).unwrap();
    let state = Arc::clone(&task.state);
    task.release_scope_owner(context());
    let before = (
        state.owner_cleanup_done.load(Ordering::Acquire),
        state.completion_cleanup_done.load(Ordering::Acquire),
        state.execution_queue_retired.load(Ordering::Acquire),
        state.accounting_done.load(Ordering::Acquire),
    );
    finish.release();
    assert!(until(|| state.accounting_done.load(Ordering::Acquire)));
    worker_release.release();
    blocker.await_result().unwrap();
    runtime.cancel_all_and_wait(WAIT).unwrap();
    child_runtime.cancel_all_and_wait(WAIT).unwrap();
    assert_eq!(before, (true, false, false, false));
    assert!(state.result.lock().unwrap().is_none());
    assert!(child_state.owner_cleanup_done.load(Ordering::Acquire));
    assert!(child_state.accounting_done.load(Ordering::Acquire));
    assert_eq!(runtime.snapshot().unwrap().finished_tasks, 1);
    assert_eq!(child_runtime.snapshot().unwrap().finished_tasks, 2);
    drop(task);
}
