use super::*;

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

fn held_worker(runtime: &TaskRuntime) -> (TaskHandle, Release) {
    let deadline = Instant::now() + WAIT;
    // sync_channel(0) correctly rejects before the receiver is waiting. Only
    // this fixture retries ordinary admission while the real worker starts.
    for _ in 0..128 {
        let (release, rx) = Release::pair();
        let (started, ready) = mpsc::sync_channel(1);
        let task = runtime
            .spawn(move || {
                started.send(()).unwrap();
                rx.recv_timeout(WAIT).expect("release held worker");
                Ok(Value::Null)
            })
            .unwrap();
        if !task.state.is_terminal() {
            ready.recv_timeout(WAIT).expect("real worker starts");
            return (task, release);
        }
        assert!(matches!(
            task.await_result().unwrap(),
            Value::Result { ok: false, .. }
        ));
        drop(release);
        assert!(Instant::now() < deadline, "worker never entered recv");
        thread::sleep(Duration::from_millis(2));
    }
    panic!("worker never accepted the bounded fixture handoff");
}

fn context() -> CancellationContext {
    CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now() + WAIT,
    }
}

fn assert_drained(runtime: &TaskRuntime, finished: usize) {
    runtime.cancel_all_and_wait(WAIT).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.finished_tasks, finished);
}

#[test]
fn busy_worker_scope_cancel_retires_actual_node_without_running_body() {
    struct Capture {
        dropped: SyncSender<Option<CancellationContext>>,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.dropped.send(current_cleanup_context()).unwrap();
        }
    }
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 3);
    let (blocker, mut release) = held_worker(&runtime);
    let (dropped, observed) = mpsc::sync_channel(1);
    let ran = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran);
    let capture = Capture { dropped };
    let child = runtime
        .spawn(move || {
            child_ran.store(true, Ordering::Release);
            drop(capture);
            Ok(Value::Null)
        })
        .unwrap();
    let owner_count = Arc::strong_count(&child.owner);
    let saved = context();
    let before = runtime.snapshot().unwrap();
    runtime
        .cancel_handles_and_wait(std::slice::from_ref(&child), saved)
        .unwrap();
    let mid = runtime.snapshot().unwrap();
    let actual_drop = observed.recv_timeout(WAIT).unwrap();
    let cancellation = child.state.cancellation();
    let retired = child.state.execution_queue_retired.load(Ordering::Acquire);
    let accounting = child.state.accounting_done.load(Ordering::Acquire);
    let owner_after = Arc::strong_count(&child.owner);
    release.release();
    blocker.await_result().unwrap();
    assert_drained(&runtime, 2);
    assert_eq!(before.queued_tasks, 1);
    assert_eq!(mid.queued_tasks, 0);
    assert_eq!(mid.active_tasks, 1);
    assert_eq!(mid.registered_tasks, 1);
    assert_eq!(mid.finished_tasks, 1);
    assert!(retired && accounting);
    assert_eq!(owner_count, owner_after);
    assert_eq!(actual_drop, Some(saved));
    assert_eq!(cancellation, Some(saved));
    assert!(!ran.load(Ordering::Acquire));
    assert!(current_cleanup_context().is_none());
}

#[test]
fn live_cached_head_keeps_credit_and_fifo_for_both_help_and_worker() {
    let runtime = TaskRuntime::with_limits(1, 2, 0, 0, 5);
    let (blocker, mut release) = held_worker(&runtime);
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_order = Arc::clone(&order);
    let first = runtime
        .spawn(move || {
            first_order.lock().unwrap().push(1);
            Ok(Value::Int(1))
        })
        .unwrap();
    let ran_cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_flag = Arc::clone(&ran_cancelled);
    let cancelled = runtime
        .spawn(move || {
            cancelled_flag.store(true, Ordering::Release);
            Ok(Value::Null)
        })
        .unwrap();
    runtime
        .cancel_handles_and_wait(std::slice::from_ref(&cancelled), context())
        .unwrap();
    let cached = runtime.snapshot().unwrap();
    let rejected_ran = Arc::new(AtomicBool::new(false));
    let rejected_flag = Arc::clone(&rejected_ran);
    let rejected = runtime
        .spawn(move || {
            rejected_flag.store(true, Ordering::Release);
            Ok(Value::Null)
        })
        .unwrap();
    let rejected_result = rejected.await_result().unwrap();
    assert!(runtime.help_one_bounded().unwrap());
    let first_result = first.await_result().unwrap();
    assert!(runtime.retire_cancelled_queue_prefix(context()));
    let last_order = Arc::clone(&order);
    let last = runtime
        .spawn(move || {
            last_order.lock().unwrap().push(3);
            Ok(Value::Int(3))
        })
        .unwrap();
    release.release();
    blocker.await_result().unwrap();
    let last_result = last.await_result().unwrap();
    assert_drained(&runtime, 4);
    assert_eq!(cached.queued_tasks, 2);
    assert_eq!(cached.finished_tasks, 0);
    assert!(matches!(rejected_result, Value::Result { ok: false, .. }));
    assert_eq!(runtime.snapshot().unwrap().rejected_task_queue, 1);
    assert_eq!(first_result, Value::Int(1));
    assert_eq!(last_result, Value::Int(3));
    assert_eq!(*order.lock().unwrap(), vec![1, 3]);
    assert!(!ran_cancelled.load(Ordering::Acquire));
    assert!(!rejected_ran.load(Ordering::Acquire));
}

#[test]
fn busy_worker_shutdown_drains_only_cancelled_nodes_without_help() {
    let runtime = TaskRuntime::with_limits(1, 3, 0, 0, 4);
    let (blocker, mut release) = held_worker(&runtime);
    let ran = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..3 {
        let ran = Arc::clone(&ran);
        handles.push(
            runtime
                .spawn(move || {
                    ran.fetch_add(1, Ordering::AcqRel);
                    Ok(Value::Null)
                })
                .unwrap(),
        );
    }
    let before = runtime.snapshot().unwrap();
    let shutdown_runtime = runtime.clone();
    let (done, complete) = mpsc::sync_channel(1);
    let shutdown = thread::spawn(move || {
        done.send(shutdown_runtime.cancel_all_and_wait(WAIT))
            .unwrap();
    });
    let deadline = Instant::now() + WAIT;
    while handles
        .iter()
        .any(|handle| !handle.state.accounting_done.load(Ordering::Acquire))
        && Instant::now() < deadline
    {
        thread::yield_now();
    }
    let held = runtime.snapshot().unwrap();
    release.release();
    let cancelled = complete.recv_timeout(WAIT).unwrap().unwrap();
    shutdown.join().unwrap();
    assert!(blocker
        .await_result()
        .unwrap_err()
        .runtime_termination()
        .is_some());
    let after = runtime.snapshot().unwrap();
    assert_eq!(before.queued_tasks, 3);
    assert_eq!(cancelled, 4);
    assert_eq!(held.active_tasks, 1);
    assert_eq!(held.queued_tasks, 0);
    assert_eq!(ran.load(Ordering::Acquire), 0);
    assert_eq!(after.accepted_submissions, 4);
    assert_eq!(after.finished_tasks, 4);
    assert_eq!(after.active_tasks, 0);
    assert_eq!(after.registered_tasks, 0);
    assert_eq!(after.queued_tasks, 0);
    assert_eq!(after.cleanup_timeouts, 0);
    for handle in handles {
        assert!(handle.state.accounting_done.load(Ordering::Acquire));
        assert!(!handle.state.awaited.load(Ordering::Acquire));
    }
}

#[test]
fn expired_cancel_keeps_terminal_predicate_without_retiring_new_node() {
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 2);
    let (blocker, mut release) = held_worker(&runtime);
    let child = runtime.spawn(|| Ok(Value::Int(9))).unwrap();
    let expired = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now(),
    };
    let mut batch = TaskCancelBatch::prepare(std::slice::from_ref(&child), expired);
    let poll = batch.poll(&runtime, expired.cleanup_deadline);
    let after = runtime.snapshot().unwrap();
    assert_eq!(poll, CancelBatchPoll::TerminalObserved);
    assert_eq!(after.queued_tasks, 1);
    assert_eq!(after.active_tasks, 2);
    assert_eq!(after.finished_tasks, 0);
    assert_eq!(after.cleanup_timeouts, 0);
    assert_eq!(child.state.cancellation(), Some(expired));
    assert!(!child.state.execution_queue_retired.load(Ordering::Acquire));
    assert!(!runtime.retire_cancelled_queue_prefix(expired));
    // A normal queue consumer still retires the already-cancelled node. It does
    // not resume its body or replace the original timeout with a fresh deadline.
    assert!(runtime.help_one_bounded().unwrap());
    release.release();
    blocker.await_result().unwrap();
    assert_drained(&runtime, 2);
    assert_eq!(child.state.cancellation(), Some(expired));
}

#[test]
fn zero_capacity_stays_a_rendezvous_and_rolls_back_rejected_capture() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    let runtime = TaskRuntime::with_limits(1, 0, 0, 0, 2);
    let (blocker, mut release) = held_worker(&runtime);
    let rejected_before = runtime.snapshot().unwrap().rejected_task_queue;
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(Arc::clone(&drops));
    let task = runtime
        .spawn(move || {
            drop(capture);
            Ok(Value::Null)
        })
        .unwrap();
    let result = task.await_result().unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert!(matches!(result, Value::Result { ok: false, .. }));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(snapshot.accepted_submissions, 1);
    assert_eq!(snapshot.rejected_task_queue, rejected_before + 1);
    assert_eq!(snapshot.active_tasks, 1);
    assert_eq!(snapshot.registered_tasks, 1);
    assert_eq!(snapshot.queued_tasks, 0);
    assert!(!task.state.accounting_done.load(Ordering::Acquire));
    assert!(!runtime.retire_cancelled_queue_prefix(context()));
    release.release();
    blocker.await_result().unwrap();
    assert_drained(&runtime, 1);
    eprintln!(
        "task_queue_header={} task_job={} retained_head_slots=1",
        std::mem::size_of::<TaskQueueReceiver>(),
        std::mem::size_of::<TaskJob>()
    );
}

#[test]
fn pending_capture_drop_is_not_acknowledged_or_executed() {
    struct Capture {
        started: SyncSender<()>,
        release: Receiver<()>,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.started.send(()).unwrap();
            self.release
                .recv_timeout(WAIT)
                .expect("release actual capture drop");
        }
    }
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 2);
    let (blocker, mut worker_release) = held_worker(&runtime);
    let (mut release, release_rx) = Release::pair();
    let (started, ready) = mpsc::sync_channel(1);
    let capture = Capture {
        started,
        release: release_rx,
    };
    let ran = Arc::new(AtomicBool::new(false));
    let child_ran = Arc::clone(&ran);
    let child = runtime
        .spawn(move || {
            child_ran.store(true, Ordering::Release);
            drop(capture);
            Ok(Value::Null)
        })
        .unwrap();
    let state = Arc::clone(&child.state);
    let saved = context();
    let (done, complete) = mpsc::sync_channel(1);
    let canceller = thread::spawn(move || {
        done.send(state.request_cancel_with(saved)).unwrap();
    });
    ready.recv_timeout(WAIT).unwrap();
    let premature = runtime.retire_cancelled_queue_prefix(saved);
    let before = runtime.snapshot().unwrap();
    let premature_retirement = child.state.execution_queue_retired.load(Ordering::Acquire);
    let premature_accounting = child.state.accounting_done.load(Ordering::Acquire);
    release.release();
    let cancelled = complete.recv_timeout(WAIT).unwrap();
    canceller.join().unwrap();
    let retired = runtime.retire_cancelled_queue_prefix(saved);
    worker_release.release();
    blocker.await_result().unwrap();
    assert_drained(&runtime, 2);
    assert!(!premature);
    assert!(!premature_retirement && !premature_accounting);
    assert_eq!(before.active_tasks, 2);
    assert_eq!(before.queued_tasks, 1);
    assert_eq!(before.finished_tasks, 0);
    assert!(cancelled && retired);
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(child.state.cancellation(), Some(saved));
}

#[test]
fn concurrent_producers_and_cancellation_keep_capacity_and_exact_retirement() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    for _ in 0..32 {
        let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 3);
        let (blocker, mut release) = held_worker(&runtime);
        let drops = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        let (done, complete) = mpsc::sync_channel(2);
        let mut producers = Vec::new();
        let mut starts = Vec::new();
        for _ in 0..2 {
            let (start, ready) = mpsc::sync_channel(1);
            starts.push(start);
            let runtime = runtime.clone();
            let done = done.clone();
            let capture = Capture(Arc::clone(&drops));
            let ran = Arc::clone(&ran);
            producers.push(thread::spawn(move || {
                ready.recv_timeout(WAIT).expect("start producer");
                let task = runtime
                    .spawn(move || {
                        ran.fetch_add(1, Ordering::AcqRel);
                        drop(capture);
                        Ok(Value::Null)
                    })
                    .unwrap();
                done.send(task).unwrap();
            }));
        }
        for start in starts {
            start.send(()).unwrap();
        }
        let first = complete.recv_timeout(WAIT).unwrap();
        let second = complete.recv_timeout(WAIT).unwrap();
        for producer in producers {
            producer.join().unwrap();
        }
        let before = runtime.snapshot().unwrap();
        let accepted = if first.state.is_terminal() {
            &second
        } else {
            &first
        };
        runtime
            .cancel_handles_and_wait(std::slice::from_ref(accepted), context())
            .unwrap();
        release.release();
        blocker.await_result().unwrap();
        assert_drained(&runtime, 2);
        assert_eq!(before.queued_tasks, 1);
        assert_eq!(before.active_tasks, 2);
        assert_eq!(before.accepted_submissions, 2);
        assert_eq!(before.rejected_task_queue, 1);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert_eq!(ran.load(Ordering::Acquire), 0);
        assert!(accepted.state.accounting_done.load(Ordering::Acquire));
    }
}

#[test]
fn producer_racing_prefix_retirement_never_adds_an_extra_queue_slot() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    for _ in 0..32 {
        let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 3);
        let (blocker, mut release) = held_worker(&runtime);
        let drops = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        let first_capture = Capture(Arc::clone(&drops));
        let first_ran = Arc::clone(&ran);
        let first = runtime
            .spawn(move || {
                first_ran.fetch_add(1, Ordering::AcqRel);
                drop(first_capture);
                Ok(Value::Null)
            })
            .unwrap();
        let saved = context();
        assert!(first.request_cancel_with(saved));
        let (start_retire, retire_ready) = mpsc::sync_channel(1);
        let (retired_tx, retired_rx) = mpsc::sync_channel(1);
        let retiring_runtime = runtime.clone();
        let retiring = thread::spawn(move || {
            retire_ready.recv_timeout(WAIT).unwrap();
            retired_tx
                .send(retiring_runtime.retire_cancelled_queue_prefix(saved))
                .unwrap();
        });
        let (start_producer, producer_ready) = mpsc::sync_channel(1);
        let (produced_tx, produced_rx) = mpsc::sync_channel(1);
        let producer_runtime = runtime.clone();
        let second_capture = Capture(Arc::clone(&drops));
        let second_ran = Arc::clone(&ran);
        let producer = thread::spawn(move || {
            producer_ready.recv_timeout(WAIT).unwrap();
            produced_tx
                .send(
                    producer_runtime
                        .spawn(move || {
                            second_ran.fetch_add(1, Ordering::AcqRel);
                            drop(second_capture);
                            Ok(Value::Null)
                        })
                        .unwrap(),
                )
                .unwrap();
        });
        start_retire.send(()).unwrap();
        start_producer.send(()).unwrap();
        let retired = retired_rx.recv_timeout(WAIT).unwrap();
        let second = produced_rx.recv_timeout(WAIT).unwrap();
        retiring.join().unwrap();
        producer.join().unwrap();
        let before = runtime.snapshot().unwrap();
        if !second.state.is_terminal() {
            runtime
                .cancel_handles_and_wait(std::slice::from_ref(&second), saved)
                .unwrap();
        }
        release.release();
        blocker.await_result().unwrap();
        assert_drained(&runtime, before.accepted_submissions);
        assert!(retired);
        assert!(first.state.accounting_done.load(Ordering::Acquire));
        assert!(before.queued_tasks <= 1);
        assert!(matches!(before.accepted_submissions, 2 | 3));
        assert_eq!(before.accepted_submissions + before.rejected_task_queue, 3);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert_eq!(ran.load(Ordering::Acquire), 0);
    }
}

#[test]
fn prefix_uses_shorter_release_deadline_outside_cleanup_window() {
    struct Tighten(CancellationContext);
    impl Drop for Tighten {
        fn drop(&mut self) {
            drop(CleanupGuard::enter(self.0));
        }
    }
    let runtime = TaskRuntime::with_limits(1, 1, 0, 0, 2);
    let (blocker, mut release) = held_worker(&runtime);
    let requested = context();
    let shorter = CancellationContext {
        reason: TerminationReason::Cancelled,
        cleanup_deadline: Instant::now() - Duration::from_millis(1),
    };
    let capture = Tighten(shorter);
    let child = runtime
        .spawn(move || {
            drop(capture);
            Ok(Value::Null)
        })
        .unwrap();
    let mut batch = TaskCancelBatch::prepare(std::slice::from_ref(&child), requested);
    assert!(current_cleanup_context().is_none());
    let poll = batch.poll(&runtime, Instant::now());
    let before = runtime.snapshot().unwrap();
    let premature = child.state.accounting_done.load(Ordering::Acquire);
    let effective = child.state.cancellation();
    // Recover through a normal consumer after taking observations. Assertion
    // failure still releases every held worker and cannot strand the fixture.
    if before.queued_tasks != 0 {
        assert!(runtime.help_one_bounded().unwrap());
    }
    release.release();
    blocker.await_result().unwrap();
    assert_drained(&runtime, 2);
    assert_eq!(batch.context(), requested);
    assert_eq!(poll, CancelBatchPoll::TerminalObserved);
    assert_eq!(before.queued_tasks, 1);
    assert_eq!(before.active_tasks, 2);
    assert_eq!(before.finished_tasks, 0);
    assert!(!premature);
    assert_eq!(
        effective,
        Some(CancellationContext {
            cleanup_deadline: shorter.cleanup_deadline,
            ..requested
        })
    );
    assert!(current_cleanup_context().is_none());
}
