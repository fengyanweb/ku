use super::*;

// This only bounds controlled test handshakes. Every cleanup batch below uses
// the ordinary one-second language context or a strictly shorter deadline.
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

fn actual_stages_done(state: &TaskState) -> bool {
    state.owner_cleanup_done.load(Ordering::Acquire)
        && state.completion_cleanup_done.load(Ordering::Acquire)
        && state.execution_queue_retired.load(Ordering::Acquire)
        && state.accounting_done.load(Ordering::Acquire)
        && state.pending_blocking.load(Ordering::Acquire) == 0
}

fn task_stages_done(state: &TaskState) -> bool {
    state.owner_cleanup_done.load(Ordering::Acquire)
        && state.completion_cleanup_done.load(Ordering::Acquire)
        && state.execution_queue_retired.load(Ordering::Acquire)
        && state.accounting_done.load(Ordering::Acquire)
}

fn context() -> CancellationContext {
    CancellationContext::new(TerminationReason::TimedOut)
}

fn held_worker(runtime: &TaskRuntime) -> (TaskHandle, Release) {
    let (release, rx) = Release::pair();
    let (started, ready) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            started.send(()).unwrap();
            rx.recv_timeout(WAIT).expect("release real task worker");
            Ok(Value::Null)
        })
        .unwrap();
    ready.recv_timeout(WAIT).unwrap();
    (task, release)
}

fn finish_held(task: TaskHandle, release: &mut Release) {
    let state = Arc::clone(&task.state);
    release.release();
    task.await_result().unwrap();
    drop(task);
    assert!(until(|| actual_stages_done(&state)));
}

fn assert_drained(runtime: &TaskRuntime, accepted: usize) {
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.accepted_submissions, accepted);
    assert_eq!(snapshot.finished_tasks, accepted);
}

struct WorkerExit(SyncSender<()>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

thread_local! {
    // This observes the real physical thread exiting after the old uncaught
    // late-payload destructor panic, without repairing worker metadata.
    static WORKER_EXIT: RefCell<Option<WorkerExit>> = const { RefCell::new(None) };
}

struct PayloadProbe {
    entered: SyncSender<Option<CancellationContext>>,
    release: Option<Receiver<()>>,
    shorter: Option<CancellationContext>,
    drops: Arc<AtomicUsize>,
    panics: bool,
}

impl Drop for PayloadProbe {
    fn drop(&mut self) {
        // Diagnostics must not manufacture a second panic if a receiving-side
        // assertion already closed its channel during the intended native Drop.
        let _ = self.entered.send(current_cleanup_context());
        if let Some(release) = self.release.take() {
            let released = release.recv_timeout(WAIT).is_ok();
            assert!(released || thread::panicking(), "release real payload Drop");
        }
        if let Some(shorter) = self.shorter {
            drop(CleanupGuard::enter(shorter));
        }
        self.drops.fetch_add(1, Ordering::AcqRel);
        assert!(!self.panics, "retirement batch actual late payload panic");
    }
}

#[test]
fn actual_late_blocking_payload_retirement_preserves_shorter_deadline_on_return_and_native_panic() {
    for panics in [false, true] {
        let _execution = ExecutionTerminationGuard::enter();
        let parent_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let pool = TaskRuntime::with_limits(0, 0, 1, 2, 1);
        let child_runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
        let (blocker, mut worker_release) = held_worker(&child_runtime);
        let (mut capture_release, capture_rx) = Release::pair();
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let (exit_tx, exit_rx) = mpsc::sync_channel(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let body_ran = Arc::clone(&ran);
        let original = context();
        let shorter = CancellationContext {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: original.cleanup_deadline - Duration::from_millis(100),
        };
        let expected = CancellationContext {
            reason: original.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        let captures = (
            PayloadProbe {
                entered: entered_tx.clone(),
                release: Some(capture_rx),
                shorter: Some(shorter),
                drops: Arc::clone(&drops),
                panics,
            },
            PayloadProbe {
                entered: entered_tx,
                release: None,
                shorter: None,
                drops: Arc::clone(&drops),
                panics: false,
            },
        );
        let child = child_runtime
            .spawn(move || {
                body_ran.store(true, Ordering::Release);
                drop(captures);
                Ok(Value::Null)
            })
            .unwrap();
        let child_state = Arc::clone(&child.state);
        let child_owner = Arc::downgrade(&child.owner);
        let (mut run_release, run_rx) = Release::pair();
        let (running_tx, running_rx) = mpsc::sync_channel(1);
        let work_pool = pool.clone();
        let parent = parent_runtime
            .spawn(move || {
                work_pool.run_blocking(
                    move || {
                        if panics {
                            WORKER_EXIT.with(|slot| {
                                assert!(slot.borrow_mut().replace(WorkerExit(exit_tx)).is_none());
                            });
                        }
                        running_tx.send(()).unwrap();
                        run_rx
                            .recv_timeout(WAIT)
                            .expect("release actual BlockingFn");
                        Ok(Value::Task(child))
                    },
                    Span::default(),
                )
            })
            .unwrap();
        let state = Arc::clone(&parent.state);
        let owner = Arc::downgrade(&parent.owner);
        let owner_count = Arc::strong_count(&parent.owner);
        let witness = parent.retirement_witness();
        assert_eq!(Arc::strong_count(&parent.owner), owner_count);
        let reserved = vec![witness];
        let capacity = reserved.capacity();
        let mut batch = TaskRetirementBatch::from_reserved(reserved, original);
        assert_eq!(batch.capacity(), capacity);
        running_rx.recv_timeout(WAIT).unwrap();
        parent.release_scope_owner(original);
        drop(parent);
        assert!(until(|| task_stages_done(&state)));
        assert_eq!(owner.strong_count(), 0);
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
        assert_eq!(parent_runtime.snapshot().unwrap().active_tasks, 0);
        assert!(matches!(
            batch.poll(Instant::now()),
            RetirementBatchPoll::Pending {
                first_index: 0,
                unfinished: 1
            }
        ));
        run_release.release();
        assert_eq!(entered_rx.recv_timeout(WAIT).unwrap(), Some(original));
        assert!(task_stages_done(&state));
        assert_eq!(state.pending_blocking.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(matches!(
            batch.poll(Instant::now()),
            RetirementBatchPoll::Pending {
                first_index: 0,
                unfinished: 1
            }
        ));
        capture_release.release();
        assert_eq!(entered_rx.recv_timeout(WAIT).unwrap(), Some(expected));
        assert!(until(|| actual_stages_done(&state)));
        assert_eq!(batch.poll(Instant::now()), RetirementBatchPoll::Retired);
        assert_eq!(batch.context(), expected);
        assert_eq!(state.cancellation(), Some(expected));
        assert_eq!(child_state.cancellation(), Some(expected));
        assert_eq!(child_owner.strong_count(), 0);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert!(!ran.load(Ordering::Acquire));
        if panics {
            exit_rx
                .recv_timeout(WAIT)
                .expect("actual native worker exit");
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
        finish_held(blocker, &mut worker_release);
        assert!(until(|| actual_stages_done(&child_state)));
        assert_drained(&child_runtime, 2);
        assert_drained(&parent_runtime, 1);
        assert!(until(|| pool.snapshot().unwrap().running_blocking_jobs == 0));
        assert_eq!(pool.snapshot().unwrap().queued_blocking_jobs, 0);
        assert_eq!(parent_runtime.snapshot().unwrap().cleanup_timeouts, 0);
    }
}

#[test]
fn busy_real_worker_batch_retires_at_most_one_cancelled_queue_node_per_poll() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (blocker, mut release) = held_worker(&runtime);
    let ran = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let body_ran = Arc::clone(&ran);
        handles.push(
            runtime
                .spawn(move || {
                    body_ran.fetch_add(1, Ordering::AcqRel);
                    Ok(Value::Null)
                })
                .unwrap(),
        );
    }
    let states = handles
        .iter()
        .map(|task| Arc::clone(&task.state))
        .collect::<Vec<_>>();
    let owners = handles
        .iter()
        .map(|task| Arc::downgrade(&task.owner))
        .collect::<Vec<_>>();
    let original = context();
    let mut reserved = Vec::with_capacity(2);
    for task in &handles {
        let before = Arc::strong_count(&task.owner);
        reserved.push(task.retirement_witness());
        assert_eq!(Arc::strong_count(&task.owner), before);
        task.release_scope_owner(original);
    }
    drop(handles);
    assert!(owners.iter().all(|owner| owner.strong_count() == 0));
    assert!(states
        .iter()
        .all(|state| state.owner_cleanup_done.load(Ordering::Acquire)));
    assert!(states
        .iter()
        .all(|state| !state.execution_queue_retired.load(Ordering::Acquire)));
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 2);
    let mut batch = TaskRetirementBatch::from_reserved(reserved, original);
    assert_eq!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::Pending {
            first_index: 1,
            unfinished: 1
        }
    );
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 1);
    assert!(actual_stages_done(&states[0]));
    assert!(!states[1].execution_queue_retired.load(Ordering::Acquire));
    assert_eq!(batch.poll(Instant::now()), RetirementBatchPoll::Retired);
    assert_eq!(runtime.snapshot().unwrap().queued_tasks, 0);
    assert!(states.iter().all(|state| actual_stages_done(state)));
    assert_eq!(ran.load(Ordering::Acquire), 0);
    assert!(states
        .iter()
        .all(|state| !state.awaited.load(Ordering::Acquire)));
    finish_held(blocker, &mut release);
    assert_drained(&runtime, 3);
    assert_eq!(runtime.snapshot().unwrap().cleanup_timeouts, 0);
    assert!(current_cleanup_context().is_none());
}

#[test]
fn real_full_producer_stays_pending_through_capture_drop_then_retires_without_fake_g1() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 4);
    let (blocker, mut worker_release) = held_worker(&runtime);
    let queued = runtime.spawn(|| Ok(Value::Int(17))).unwrap();
    let old_ids = [blocker.id(), queued.id()];
    let (mut builder_release, builder_rx) = Release::pair();
    let (mut capture_release, capture_rx) = Release::pair();
    let (observer_tx, observer_rx) = mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let drop_count = Arc::clone(&drops);
    let ran = Arc::new(AtomicBool::new(false));
    let body_ran = Arc::clone(&ran);
    let producer_runtime = runtime.clone();
    let producer = thread::spawn(move || {
        producer_runtime
            .spawn_deferred(|| {
                let state = producer_runtime
                    .inner
                    .states
                    .lock()
                    .unwrap()
                    .values()
                    .filter_map(Weak::upgrade)
                    .find(|state| !old_ids.contains(&state.id))
                    .unwrap();
                observer_tx.send(state).unwrap();
                builder_rx
                    .recv_timeout(WAIT)
                    .expect("release actual registered builder");
                let capture = PayloadProbe {
                    entered: entered_tx,
                    release: Some(capture_rx),
                    shorter: None,
                    drops: drop_count,
                    panics: false,
                };
                move || {
                    body_ran.store(true, Ordering::Release);
                    drop(capture);
                    Ok(Value::Null)
                }
            })
            .unwrap()
    });
    let state = observer_rx.recv_timeout(WAIT).unwrap();
    let original = context();
    // The state comes from the actual registered, still executing producer.
    // No synthetic TaskHandle or accounting publication is created.
    let reserved = vec![TaskRetirementWitness::new(
        Arc::clone(&state),
        runtime.clone(),
    )];
    let mut batch = TaskRetirementBatch::from_reserved(reserved, original);
    assert_eq!(state.submission.load(Ordering::Acquire), SUBMISSION_PENDING);
    assert!(matches!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::Pending { unfinished: 1, .. }
    ));
    builder_release.release();
    assert!(entered_rx.recv_timeout(WAIT).unwrap().is_none());
    assert_eq!(state.submission.load(Ordering::Acquire), SUBMISSION_PENDING);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert!(matches!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::Pending { unfinished: 1, .. }
    ));
    capture_release.release();
    let rejected = producer.join().unwrap();
    assert_eq!(
        state.submission.load(Ordering::Acquire),
        SUBMISSION_PRODUCER_RETIRED
    );
    assert!(!state.owner_cleanup_done.load(Ordering::Acquire));
    assert!(!state.queue_consumed.load(Ordering::Acquire));
    assert!(!state.execution_queue_retired.load(Ordering::Acquire));
    assert!(!state.accounting_finished.load(Ordering::Acquire));
    assert!(!state.accounting_done.load(Ordering::Acquire));
    assert!(matches!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::Pending { unfinished: 1, .. }
    ));
    assert!(matches!(
        rejected.await_result().unwrap(),
        Value::Result { ok: false, .. }
    ));
    let owner = Arc::downgrade(&rejected.owner);
    rejected.release_scope_owner(original);
    drop(rejected);
    assert_eq!(owner.strong_count(), 0);
    assert_eq!(batch.poll(Instant::now()), RetirementBatchPoll::Retired);
    assert!(state.owner_cleanup_done.load(Ordering::Acquire));
    assert!(!state.execution_queue_retired.load(Ordering::Acquire));
    assert!(!state.accounting_done.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(!ran.load(Ordering::Acquire));
    finish_held(blocker, &mut worker_release);
    let queued_state = Arc::clone(&queued.state);
    assert_eq!(queued.await_result().unwrap(), Value::Int(17));
    drop(queued);
    assert!(until(|| actual_stages_done(&queued_state)));
    assert_drained(&runtime, 2);
    assert_eq!(runtime.snapshot().unwrap().rejected_task_queue, 1);
}

#[test]
fn same_numeric_id_foreign_runtime_witnesses_keep_distinct_real_nodes_and_no_task_owner() {
    let _execution = ExecutionTerminationGuard::enter();
    let first_runtime = TaskRuntime::with_limits(1, 1, 0, 0, 2);
    let second_runtime = TaskRuntime::with_limits(1, 1, 0, 0, 2);
    let (first_blocker, mut first_release) = held_worker(&first_runtime);
    let (second_blocker, mut second_release) = held_worker(&second_runtime);
    let ran = Arc::new(AtomicUsize::new(0));
    let first_ran = Arc::clone(&ran);
    let first = first_runtime
        .spawn(move || {
            first_ran.fetch_add(1, Ordering::AcqRel);
            Ok(Value::Null)
        })
        .unwrap();
    let second_ran = Arc::clone(&ran);
    let second = second_runtime
        .spawn(move || {
            second_ran.fetch_add(1, Ordering::AcqRel);
            Ok(Value::Null)
        })
        .unwrap();
    assert_eq!(first.id(), second.id());
    assert_ne!(
        first.internal_identity_key(),
        second.internal_identity_key()
    );
    let states = [Arc::clone(&first.state), Arc::clone(&second.state)];
    let blocker_states = [
        Arc::clone(&first_blocker.state),
        Arc::clone(&second_blocker.state),
    ];
    let runtimes = [
        Arc::downgrade(&first_runtime.inner),
        Arc::downgrade(&second_runtime.inner),
    ];
    let owners = [Arc::downgrade(&first.owner), Arc::downgrade(&second.owner)];
    let original = context();
    let mut reserved = Vec::with_capacity(2);
    let first_before = Arc::strong_count(&first.owner);
    reserved.push(first.retirement_witness());
    assert_eq!(Arc::strong_count(&first.owner), first_before);
    let second_before = Arc::strong_count(&second.owner);
    reserved.push(second.retirement_witness());
    assert_eq!(Arc::strong_count(&second.owner), second_before);
    first.release_scope_owner(original);
    second.release_scope_owner(original);
    first_blocker.release_scope_owner(original);
    second_blocker.release_scope_owner(original);
    drop((first, second, first_blocker, second_blocker));
    drop((first_runtime, second_runtime));
    assert!(owners.iter().all(|owner| owner.strong_count() == 0));
    let mut batch = TaskRetirementBatch::from_reserved(reserved, original);
    assert!(matches!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::Pending {
            first_index: 1,
            unfinished: 1
        }
    ));
    assert!(actual_stages_done(&states[0]));
    assert!(!states[1].execution_queue_retired.load(Ordering::Acquire));
    assert_eq!(batch.poll(Instant::now()), RetirementBatchPoll::Retired);
    assert!(states.iter().all(|state| actual_stages_done(state)));
    assert_eq!(ran.load(Ordering::Acquire), 0);
    for runtime in &runtimes {
        let inner = runtime
            .upgrade()
            .expect("witness retains actual foreign runtime");
        assert_eq!(inner.queued_tasks.load(Ordering::Acquire), 0);
        assert_eq!(inner.active_tasks.load(Ordering::Acquire), 1);
        assert_eq!(inner.finished_tasks.load(Ordering::Acquire), 1);
    }
    drop(batch);
    first_release.release();
    second_release.release();
    assert!(until(|| blocker_states
        .iter()
        .all(|state| actual_stages_done(state))));
    // The held workers also retain their current physical execution Arc. Once
    // those real frames finish, no Task owner or witness leaks either runtime.
    assert!(until(|| runtimes
        .iter()
        .all(|runtime| runtime.upgrade().is_none())));
    assert!(current_cleanup_context().is_none());
}

#[test]
fn later_expired_witness_stops_prefix_effects_and_records_timeout_once() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (blocker, mut release) = held_worker(&runtime);
    let first = runtime.spawn(|| Ok(Value::Null)).unwrap();
    let second = runtime.spawn(|| Ok(Value::Null)).unwrap();
    let states = [Arc::clone(&first.state), Arc::clone(&second.state)];
    let original = context();
    let short = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() - Duration::from_millis(1),
    };
    let reserved = vec![first.retirement_witness(), second.retirement_witness()];
    first.release_scope_owner(original);
    assert!(second.request_cancel_with(short));
    second.release_scope_owner(original);
    drop((first, second));
    let expected = CancellationContext {
        reason: original.reason,
        cleanup_deadline: short.cleanup_deadline,
    };
    let before = runtime.snapshot().unwrap();
    assert_eq!(before.queued_tasks, 2);
    let mut batch = TaskRetirementBatch::from_reserved(reserved, original);
    assert_eq!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::TimedOut(expected)
    );
    let after = runtime.snapshot().unwrap();
    assert_eq!(after.queued_tasks, 2);
    assert_eq!(after.cleanup_timeouts, before.cleanup_timeouts + 1);
    assert_eq!(
        after.cleanup_unfinished_tasks,
        before.cleanup_unfinished_tasks + 2
    );
    assert!(states
        .iter()
        .all(|state| !state.execution_queue_retired.load(Ordering::Acquire)));
    for _ in 0..3 {
        assert_eq!(
            batch.poll(Instant::now()),
            RetirementBatchPoll::TimedOut(expected)
        );
    }
    let repeated = runtime.snapshot().unwrap();
    assert_eq!(repeated.queued_tasks, 2);
    assert_eq!(repeated.cleanup_timeouts, after.cleanup_timeouts);
    assert_eq!(
        repeated.cleanup_unfinished_tasks,
        after.cleanup_unfinished_tasks
    );
    finish_held(blocker, &mut release);
    assert!(until(|| states
        .iter()
        .all(|state| actual_stages_done(state))));
    assert_eq!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::TimedOut(expected)
    );
    assert_drained(&runtime, 3);
    assert_eq!(
        runtime.snapshot().unwrap().cleanup_timeouts,
        after.cleanup_timeouts
    );
    assert!(current_cleanup_context().is_none());
}

#[test]
fn wait_resamples_expired_ambient_deadline_before_the_actual_poisoned_result_lock() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 1);
    let (task, mut release) = held_worker(&runtime);
    let state = Arc::clone(&task.state);
    let original = context();
    let reserved = vec![task.retirement_witness()];
    task.release_scope_owner(original);
    drop(task);
    let mut batch = TaskRetirementBatch::from_reserved(reserved, original);
    assert!(matches!(
        batch.poll(Instant::now()),
        RetirementBatchPoll::Pending { unfinished: 1, .. }
    ));
    let fault = catch_unwind(AssertUnwindSafe(|| {
        let _guard = state.result.lock().unwrap();
        panic!("actual retirement wait result-lock poison fault");
    }));
    assert!(fault.is_err() && state.result.is_poisoned());
    struct RestorePoison<'a>(&'a Mutex<Option<KuResult<Value>>>);
    impl Drop for RestorePoison<'_> {
        fn drop(&mut self) {
            self.0.clear_poison();
        }
    }
    let restore = RestorePoison(&state.result);
    let span = Span::point(crate::span::Position::new(7, 11, 19));
    let live_error = batch.wait_once(0, span).unwrap_err();
    let expired = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() - Duration::from_millis(1),
    };
    let expected = CancellationContext {
        reason: original.reason,
        ..expired
    };
    let expired_wait = {
        let _ambient = CleanupGuard::enter(expired);
        batch.wait_once(0, span)
    };
    let saved = batch.context();
    let first_poll = batch.poll(Instant::now());
    let second_poll = batch.poll(Instant::now());
    let after_wait = runtime.snapshot().unwrap();
    // Restore only the injected Mutex fault before the real held execution
    // finishes; completion and accounting remain actual production actions.
    drop(restore);
    release.release();
    assert!(until(|| actual_stages_done(&state)));
    assert_drained(&runtime, 1);
    assert_eq!(live_error.span, span);
    assert_eq!(live_error.message, "async task state is poisoned");
    assert!(
        expired_wait.is_ok(),
        "expired wait must not enter the actual poisoned result lock"
    );
    assert_eq!(saved, expected);
    assert_eq!(first_poll, RetirementBatchPoll::TimedOut(expected));
    assert_eq!(second_poll, RetirementBatchPoll::TimedOut(expected));
    assert_eq!(after_wait.cleanup_timeouts, 1);
    assert_eq!(after_wait.cleanup_unfinished_tasks, 1);
    assert_eq!(after_wait.queued_tasks, 0);
    assert_eq!(current_cleanup_context(), None);
    assert_eq!(current_execution_termination(), None);
}

#[test]
fn actual_current_task_cannot_retire_itself_and_fails_at_its_original_deadline() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 1);
    let (batch_tx, batch_rx) = mpsc::sync_channel::<TaskRetirementBatch>(1);
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let (mut finish_release, finish_rx) = Release::pair();
    let task = runtime
        .spawn(move || {
            started_tx.send(()).unwrap();
            let mut batch = batch_rx
                .recv_timeout(WAIT)
                .expect("receive actual self witness");
            let original = batch.context();
            let mut saw_pending = false;
            let mut outcome = None;
            for _ in 0..2048 {
                match batch.poll(Instant::now()) {
                    RetirementBatchPoll::Pending {
                        first_index: 0,
                        unfinished: 1,
                    } => {
                        saw_pending = true;
                        batch.wait_once(0, Span::default()).unwrap();
                    }
                    RetirementBatchPoll::TimedOut(context) => {
                        outcome = Some(context);
                        break;
                    }
                    other => panic!("actual executing self cannot retire: {other:?}"),
                }
            }
            assert!(saw_pending);
            assert_eq!(outcome, Some(original));
            assert!(Instant::now() >= original.cleanup_deadline);
            assert!(current_cleanup_context().is_none());
            done_tx.send((current_task_id(), outcome)).unwrap();
            finish_rx
                .recv_timeout(WAIT)
                .expect("release actual self task after timeout");
            Ok(Value::Null)
        })
        .unwrap();
    started_rx.recv_timeout(WAIT).unwrap();
    let state = Arc::clone(&task.state);
    let owner = Arc::downgrade(&task.owner);
    let id = task.id();
    let original = context();
    let reserved = vec![task.retirement_witness()];
    task.release_scope_owner(original);
    drop(task);
    assert_eq!(owner.strong_count(), 0);
    assert!(state.owner_cleanup_done.load(Ordering::Acquire));
    assert!(!state.execution_queue_retired.load(Ordering::Acquire));
    batch_tx
        .send(TaskRetirementBatch::from_reserved(reserved, original))
        .unwrap();
    let (observed_id, outcome) = done_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(observed_id, id);
    assert_eq!(outcome, Some(original));
    assert!(!state.execution_queue_retired.load(Ordering::Acquire));
    assert!(!state.accounting_done.load(Ordering::Acquire));
    assert_eq!(runtime.snapshot().unwrap().cleanup_timeouts, 1);
    assert_eq!(runtime.snapshot().unwrap().cleanup_unfinished_tasks, 1);
    finish_release.release();
    assert!(until(|| actual_stages_done(&state)));
    assert_eq!(state.cancellation(), Some(original));
    assert_drained(&runtime, 1);
    assert!(current_cleanup_context().is_none());
    assert!(current_execution_termination().is_none());
}
