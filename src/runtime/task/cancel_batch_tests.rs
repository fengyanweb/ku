use super::*;

const TEST_WAIT: Duration = Duration::from_secs(3);

// Every held worker/destructor has a finite receive and a supervisor-owned
// release that also runs on assertion failure. The supervisor never helps jobs.
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
    let task = runtime
        .spawn(move || {
            started_tx.send(thread::current().id()).unwrap();
            release_rx.recv_timeout(TEST_WAIT).expect("release worker");
            Ok(Value::Int(7))
        })
        .expect("admit held worker");
    let worker = started_rx
        .recv_timeout(TEST_WAIT)
        .expect("real worker started");
    (task, release, worker)
}

struct CaptureDrop {
    observed: SyncSender<Option<CancellationContext>>,
    release: Option<Receiver<()>>,
}

impl Drop for CaptureDrop {
    fn drop(&mut self) {
        self.observed.send(current_cleanup_context()).unwrap();
        if let Some(release) = self.release.take() {
            release
                .recv_timeout(TEST_WAIT)
                .expect("release capture drop");
        }
    }
}

fn drain(runtime: &TaskRuntime, finished: usize) {
    runtime
        .cancel_all_and_wait(Duration::from_secs(1))
        .expect("released workers and queue nodes drain");
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.queued_blocking_jobs, 0);
    assert_eq!(snapshot.running_blocking_jobs, 0);
    assert_eq!(snapshot.finished_tasks, finished);
}

#[test]
fn expired_prepare_broadcasts_to_real_running_and_queued_tasks_before_one_sticky_timeout() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(2, 4, 0, 0, 4);
    let (first, mut first_release, first_worker) = held_worker(&runtime);
    let (second, mut second_release, second_worker) = held_worker(&runtime);
    let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
    let executed = Arc::new(AtomicBool::new(false));
    let child_executed = Arc::clone(&executed);
    let capture = CaptureDrop {
        observed: dropped_tx,
        release: None,
    };
    let queued = runtime
        .spawn(move || {
            child_executed.store(true, Ordering::Release);
            drop(capture);
            Ok(Value::Null)
        })
        .unwrap();
    let handles = [queued, first, second];
    let deadline = Instant::now();
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: deadline,
    };
    let mut batch = TaskCancelBatch::prepare(handles.as_slice(), context);
    let capture_context = dropped_rx.recv_timeout(TEST_WAIT);
    let cancellations = handles.each_ref().map(|task| task.state.cancellation());
    let before_poll = runtime.snapshot().unwrap();
    // A stale lower bound cannot carry the actual post-scan clock past D.
    let timed_out = batch.poll(&runtime, deadline - Duration::from_secs(1));
    let first_count = runtime.snapshot().unwrap();
    let repeated = batch.poll(&runtime, deadline);
    let second_count = runtime.snapshot().unwrap();

    first_release.release();
    second_release.release();
    drain(&runtime, 3);
    let after_completion = batch.poll(&runtime, Instant::now());
    let final_count = runtime.snapshot().unwrap();
    drop(batch);

    assert_ne!(first_worker, thread::current().id());
    assert_ne!(second_worker, thread::current().id());
    assert_eq!(capture_context.unwrap(), Some(context));
    assert_eq!(cancellations, [Some(context); 3]);
    assert!(!executed.load(Ordering::Acquire));
    assert_eq!(before_poll.cleanup_timeouts, 0);
    assert_eq!(timed_out, CancelBatchPoll::TimedOut(context));
    assert_eq!(repeated, timed_out);
    assert_eq!(after_completion, timed_out);
    assert_eq!(first_count.cleanup_timeouts, 1);
    assert_eq!(first_count.cleanup_unfinished_tasks, 2);
    assert_eq!(second_count.cleanup_timeouts, 1);
    assert_eq!(second_count.cleanup_unfinished_tasks, 2);
    assert_eq!(final_count.cleanup_timeouts, 1);
    assert_eq!(final_count.cleanup_unfinished_tasks, 2);
    assert!(handles
        .iter()
        .all(|task| !task.state.awaited.load(Ordering::Acquire)));
    assert!(current_cleanup_context().is_none());
}

#[test]
fn borrowed_and_moved_vec_batches_preserve_owners_await_and_wait_edges() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 2);
    let (task, mut release, worker) = held_worker(&runtime);
    let state = Arc::clone(&task.state);
    let mut handles = Vec::with_capacity(8);
    handles.push(task);
    let backing = handles.as_ptr();
    let context = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() + TEST_WAIT,
    };
    let owner_count = Arc::strong_count(&handles[0].owner);
    let (borrowed_poll, borrowed_owner_count, borrowed_context) = {
        let mut borrowed = TaskCancelBatch::prepare(handles.as_slice(), context);
        (
            borrowed.poll(&runtime, Instant::now()),
            Arc::strong_count(&borrowed.handles()[0].owner),
            borrowed.context(),
        )
    };
    let owner_released_after_borrow = state.owner_released.load(Ordering::Acquire);
    let mut owned = TaskCancelBatch::prepare(handles, context);
    let owned_backing = owned.handles().as_ptr();
    let owned_owner_count = Arc::strong_count(&owned.handles()[0].owner);
    let owned_poll = owned.poll(&runtime, Instant::now());
    let waiting = runtime.snapshot().unwrap();
    let awaited_before_drain = state.awaited.load(Ordering::Acquire);

    release.release();
    drain(&runtime, 1);
    // Fully terminal handles win even when the explicit lower bound is past D.
    let terminal = owned.poll(&runtime, context.cleanup_deadline);
    let final_snapshot = runtime.snapshot().unwrap();
    drop(owned);

    assert_ne!(worker, thread::current().id());
    assert_eq!(owner_count, 1);
    assert_eq!(borrowed_owner_count, owner_count);
    assert_eq!(owned_owner_count, owner_count);
    assert_eq!(owned_backing, backing);
    assert_eq!(borrowed_context, context);
    assert_eq!(
        borrowed_poll,
        CancelBatchPoll::Pending {
            first_index: 0,
            unfinished: 1
        }
    );
    assert_eq!(owned_poll, borrowed_poll);
    assert!(!owner_released_after_borrow);
    assert!(!awaited_before_drain);
    assert!(!state.awaited.load(Ordering::Acquire));
    assert_eq!(waiting.wait_edges, 0);
    assert_eq!(terminal, CancelBatchPoll::TerminalObserved);
    assert_eq!(final_snapshot.cleanup_timeouts, 0);
    assert_eq!(final_snapshot.cleanup_unfinished_tasks, 0);
    assert!(state.owner_released.load(Ordering::Acquire));
    assert!(state.result().unwrap().is_none());
    assert!(current_cleanup_context().is_none());
    eprintln!(
        "cancel_batch_header borrowed={} owned={}",
        std::mem::size_of::<TaskCancelBatch<&[TaskHandle]>>(),
        std::mem::size_of::<TaskCancelBatch<Vec<TaskHandle>>>()
    );
}

#[test]
fn prepare_preserves_first_cause_absolute_deadline_and_restores_nested_cleanup_tls() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 2);
    let (task, mut release, _) = held_worker(&runtime);
    let now = Instant::now();
    let ambient = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now + TEST_WAIT,
    };
    let tighter = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now + Duration::from_secs(2),
    };
    let requested = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now + Duration::from_secs(4),
    };
    let effective = CancellationContext {
        cleanup_deadline: tighter.cleanup_deadline,
        ..ambient
    };
    let outer = CleanupGuard::enter(ambient);
    let inner = CleanupGuard::enter(tighter);
    let mut batch = TaskCancelBatch::prepare(std::slice::from_ref(&task), requested);
    let prepared_context = batch.context();
    let prepared_tls = current_cleanup_context();
    let child_context = task.state.cancellation();
    drop(inner);
    let restored_outer = current_cleanup_context();
    drop(outer);
    let restored_empty = current_cleanup_context();
    let pending = batch.poll(&runtime, Instant::now());
    // This is an explicit future deadline observation, without sleeping.
    let timed_out = batch.poll(&runtime, effective.cleanup_deadline);
    let poll_tls = current_cleanup_context();
    drop(batch);
    let drop_tls = current_cleanup_context();

    release.release();
    drain(&runtime, 1);
    assert_eq!(prepared_context, effective);
    assert_eq!(prepared_tls, Some(effective));
    assert_eq!(child_context, Some(effective));
    assert_eq!(restored_outer, Some(effective));
    assert_eq!(restored_empty, None);
    assert_eq!(
        pending,
        CancelBatchPoll::Pending {
            first_index: 0,
            unfinished: 1
        }
    );
    assert_eq!(timed_out, CancelBatchPoll::TimedOut(effective));
    assert_eq!(poll_tls, None);
    assert_eq!(drop_tls, None);
    assert!(current_execution_termination().is_none());
}

#[test]
fn terminal_observed_does_not_acknowledge_a_real_child_destructor_or_parent_accounting() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let child_runtime = runtime.clone();
    let (mut top_release, top_release_rx) = ReleaseGate::pair();
    let (mut drop_release, drop_release_rx) = ReleaseGate::pair();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let executed = Arc::new(AtomicBool::new(false));
    let child_executed = Arc::clone(&executed);
    let capture = CaptureDrop {
        observed: entered_tx,
        release: Some(drop_release_rx),
    };
    let top = runtime
        .spawn(move || {
            let child = child_runtime.spawn(move || {
                child_executed.store(true, Ordering::Release);
                drop(capture);
                Ok(Value::Null)
            })?;
            ready_tx
                .send((Arc::clone(&child.state), thread::current().id()))
                .unwrap();
            top_release_rx
                .recv_timeout(TEST_WAIT)
                .expect("release top result");
            // No cloned logical owner: cancellation discards this unique result.
            Ok(Value::Task(child))
        })
        .unwrap();
    let (child_state, worker) = ready_rx.recv_timeout(TEST_WAIT).expect("top created child");
    let context = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() + TEST_WAIT,
    };
    let mut batch = TaskCancelBatch::prepare(std::slice::from_ref(&top), context);
    let before_result = batch.poll(&runtime, Instant::now());
    top_release.release();
    let drop_context = entered_rx.recv_timeout(TEST_WAIT);
    let status_at_drop = top.status();
    let parent_accounting_at_drop = top.state.accounting_finished.load(Ordering::Acquire);
    let parent_completion_cleanup_at_drop =
        top.state.completion_cleanup_done.load(Ordering::Acquire);
    let parent_execution_retired_at_drop =
        top.state.execution_queue_retired.load(Ordering::Acquire);
    let parent_accounting_done_at_drop = top.state.accounting_done.load(Ordering::Acquire);
    let child_accounting_at_drop = child_state.accounting_finished.load(Ordering::Acquire);
    let child_status_at_drop = child_state.status_name();
    let observed = batch.poll(&runtime, Instant::now());
    let at_drop = runtime.snapshot().unwrap();

    drop_release.release();
    drain(&runtime, 2);
    let repeated = batch.poll(&runtime, context.cleanup_deadline);
    drop(batch);
    assert_ne!(worker, thread::current().id());
    assert_eq!(
        before_result,
        CancelBatchPoll::Pending {
            first_index: 0,
            unfinished: 1
        }
    );
    assert_eq!(drop_context.unwrap(), Some(context));
    assert_eq!(status_at_drop, "cancelled");
    assert!(!parent_accounting_at_drop);
    assert!(!parent_completion_cleanup_at_drop);
    assert!(!parent_execution_retired_at_drop);
    assert!(!parent_accounting_done_at_drop);
    assert!(top.state.completion_cleanup_done.load(Ordering::Acquire));
    assert!(top.state.execution_queue_retired.load(Ordering::Acquire));
    assert!(top.state.accounting_done.load(Ordering::Acquire));
    assert!(!child_accounting_at_drop);
    assert_eq!(child_status_at_drop, "cancelling");
    assert_eq!(observed, CancelBatchPoll::TerminalObserved);
    assert_eq!(repeated, observed);
    assert_eq!(at_drop.active_tasks, 2);
    assert_eq!(at_drop.finished_tasks, 0);
    assert_eq!(at_drop.cleanup_timeouts, 0);
    assert!(!executed.load(Ordering::Acquire));
    assert!(!top.state.awaited.load(Ordering::Acquire));
    assert!(!child_state.awaited.load(Ordering::Acquire));
    assert!(current_cleanup_context().is_none());
}

#[test]
fn owned_batch_drop_releases_real_function_capture_under_the_tightened_prepare_window() {
    struct TightenDrop(CancellationContext);
    impl Drop for TightenDrop {
        fn drop(&mut self) {
            let _tighter = CleanupGuard::enter(self.0);
        }
    }

    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 4, 0, 0, 4);
    let child_runtime = runtime.clone();
    let (mut blocker_release, blocker_release_rx) = ReleaseGate::pair();
    let (blocker_started_tx, blocker_started_rx) = mpsc::sync_channel(1);
    let (payload_tx, payload_rx) = mpsc::sync_channel(1);
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let executed = Arc::new(AtomicBool::new(false));
    let child_executed = Arc::clone(&executed);
    let capture = CaptureDrop {
        observed: drop_tx,
        release: None,
    };
    let top = runtime
        .spawn(move || {
            // This queue node precedes the captured child and holds the one
            // worker after the top's genuine completion/result publication.
            let blocker = child_runtime.spawn(move || {
                blocker_started_tx.send(thread::current().id()).unwrap();
                blocker_release_rx
                    .recv_timeout(TEST_WAIT)
                    .expect("release blocker");
                Ok(Value::Null)
            })?;
            let child = child_runtime.spawn(move || {
                child_executed.store(true, Ordering::Release);
                drop(capture);
                Ok(Value::Null)
            })?;
            payload_tx
                .send((blocker, Arc::clone(&child.state)))
                .unwrap();
            let mut captures = crate::env::Env::new();
            captures.define_owned("Child".into(), Value::Task(child), false, Span::default())?;
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
    let (blocker, child_state) = payload_rx
        .recv_timeout(TEST_WAIT)
        .expect("owned captures built");
    let worker = blocker_started_rx
        .recv_timeout(TEST_WAIT)
        .expect("blocker started after top completion");
    let top_state = Arc::clone(&top.state);
    let top_was_terminal = top_state.is_terminal();
    let top_had_function = matches!(
        top_state.result.lock().unwrap().as_ref(),
        Some(Ok(Value::Function { .. }))
    );
    let context = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() + TEST_WAIT,
    };
    let tighter = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now(),
    };
    let expected_release = CancellationContext {
        cleanup_deadline: tighter.cleanup_deadline,
        ..context
    };
    let tightening_capture = TightenDrop(tighter);
    let tightener = runtime
        .spawn(move || {
            drop(tightening_capture);
            Ok(Value::Null)
        })
        .unwrap();
    let mut batch = TaskCancelBatch::prepare(vec![tightener, top], context);
    let wait_snapshot = batch.context();
    let after_prepare_tls = current_cleanup_context();
    let terminal = batch.poll(&runtime, context.cleanup_deadline);
    // Last logical top owner releases a real Function -> Env -> child Task.
    // The stored release D must survive after prepare's short guard is gone.
    drop(batch);
    let released_context = drop_rx.recv_timeout(TEST_WAIT);
    let released_child_context = child_state.cancellation();
    let restored_tls = current_cleanup_context();
    let top_result_released = top_state.result().unwrap().is_none();
    let top_owner_released = top_state.owner_released.load(Ordering::Acquire);

    blocker_release.release();
    drain(&runtime, 4);
    drop(blocker);
    assert_ne!(worker, thread::current().id());
    assert!(top_was_terminal);
    assert!(top_had_function);
    assert_eq!(wait_snapshot, context);
    assert_eq!(after_prepare_tls, None);
    assert_eq!(terminal, CancelBatchPoll::TerminalObserved);
    assert_eq!(released_context.unwrap(), Some(expected_release));
    assert_eq!(released_child_context, Some(expected_release));
    assert_eq!(restored_tls, None);
    assert!(top_result_released);
    assert!(top_owner_released);
    assert!(!executed.load(Ordering::Acquire));
    assert!(!top_state.awaited.load(Ordering::Acquire));
    assert!(!child_state.awaited.load(Ordering::Acquire));
    assert_eq!(runtime.snapshot().unwrap().cleanup_timeouts, 0);
}

#[test]
fn prepare_panic_drops_owned_handles_under_the_original_effective_context_and_restores_tls() {
    struct PanickingHandles {
        handles: Vec<TaskHandle>,
        observed: SyncSender<Option<CancellationContext>>,
    }
    impl AsRef<[TaskHandle]> for PanickingHandles {
        fn as_ref(&self) -> &[TaskHandle] {
            panic!("cancel batch owned handle view panic");
        }
    }
    impl Drop for PanickingHandles {
        fn drop(&mut self) {
            self.observed.send(current_cleanup_context()).unwrap();
            // The actual final logical leases are fields, dropped under the
            // batch's guard after this observer, rather than simulated states.
            assert_eq!(self.handles.len(), 1);
        }
    }

    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 2);
    let (task, mut release, _) = held_worker(&runtime);
    let state = Arc::clone(&task.state);
    let now = Instant::now();
    let ambient = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now + Duration::from_secs(2),
    };
    let requested = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: now + TEST_WAIT,
    };
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let handles = PanickingHandles {
        handles: vec![task],
        observed: observed_tx,
    };
    let outer = CleanupGuard::enter(ambient);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        TaskCancelBatch::prepare(handles, requested)
    }));
    let owned_drop_context = observed_rx.recv_timeout(TEST_WAIT);
    let restored_ambient = current_cleanup_context();
    let cancellation = state.cancellation();
    let owner_released = state.owner_released.load(Ordering::Acquire);
    drop(outer);
    let restored_empty = current_cleanup_context();
    release.release();
    drain(&runtime, 1);

    assert!(panic.is_err());
    assert_eq!(owned_drop_context.unwrap(), Some(ambient));
    assert_eq!(cancellation, Some(ambient));
    assert!(owner_released);
    assert_eq!(restored_ambient, Some(ambient));
    assert_eq!(restored_empty, None);
    assert!(!state.awaited.load(Ordering::Acquire));
    assert!(current_execution_termination().is_none());
}
