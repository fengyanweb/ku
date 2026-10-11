use super::*;

const TEST_WAIT: Duration = Duration::from_secs(3);
const PANIC_MESSAGE: &str = "helper late owner destructor fixture panic";

struct ReleaseGate(Option<SyncSender<()>>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.try_send(());
        }
    }
}

struct PanicOwner;

impl Drop for PanicOwner {
    fn drop(&mut self) {
        panic!("{PANIC_MESSAGE}");
    }
}

#[derive(Debug)]
struct AwaitReceipt {
    panic: Option<String>,
    depth: usize,
    wait_edges: usize,
    status: &'static str,
    id: i64,
    thread: thread::ThreadId,
    cleanup: Option<CancellationContext>,
    execution: Option<CancellationContext>,
    target_retired: bool,
}

#[test]
fn helped_cleanup_panic_restores_await_depth_wait_edge_and_parent_status() {
    let _execution = ExecutionTerminationGuard::enter();
    let runtime = TaskRuntime::with_limits(1, 6, 0, 0, 6);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release = ReleaseGate(Some(release_tx));
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let parent_runtime = runtime.clone();
    let parent = runtime
        .spawn(move || {
            let target_runtime = parent_runtime.clone();
            let target = parent_runtime
                .spawn(move || {
                    let owner = PanicOwner;
                    let nested = target_runtime
                        .spawn(move || {
                            drop(owner);
                            Ok(Value::Null)
                        })
                        .unwrap();
                    let nested_id = nested.id();
                    let mut captures = crate::env::Env::new();
                    captures
                        .define_owned("nested".into(), Value::Task(nested), false, Span::default())
                        .unwrap();
                    ready_tx
                        .send((
                            current_task_id(),
                            nested_id,
                            thread::current().id(),
                            await_help_depth(),
                        ))
                        .unwrap();
                    release_rx.recv_timeout(TEST_WAIT).unwrap();
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
            let outcome = catch_unwind(AssertUnwindSafe(|| parent_runtime.await_task(&target)));
            let panic = outcome.as_ref().err().and_then(|payload| {
                payload.downcast_ref::<String>().cloned().or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                })
            });
            let state = CURRENT_TASK_STATE
                .with(|current| current.borrow().as_ref().and_then(Weak::upgrade).unwrap());
            receipt_tx
                .send(AwaitReceipt {
                    panic,
                    depth: await_help_depth(),
                    wait_edges: parent_runtime.snapshot().unwrap().wait_edges,
                    status: state.status_name(),
                    id: current_task_id(),
                    thread: thread::current().id(),
                    cleanup: current_cleanup_context(),
                    execution: current_execution_termination(),
                    target_retired: target.state.queue_consumed.load(Ordering::Acquire)
                        && target.state.accounting_finished.load(Ordering::Acquire),
                })
                .unwrap();
            drop(target);
            Ok(Value::Int(55))
        })
        .unwrap();
    let ready = ready_rx.recv_timeout(TEST_WAIT);
    let target = ready
        .as_ref()
        .ok()
        .and_then(|ready| runtime.state(ready.0).ok().flatten());
    let nested = ready
        .as_ref()
        .ok()
        .and_then(|ready| runtime.state(ready.1).ok().flatten());
    let context = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    };
    let cancelled = target
        .as_ref()
        .is_some_and(|target| target.request_cancel_with(context));
    drop(release);
    let receipt = receipt_rx.recv_timeout(TEST_WAIT);
    let (marker_tx, marker_rx) = mpsc::sync_channel(1);
    let marker = runtime.spawn(move || {
        marker_tx
            .send((
                thread::current().id(),
                await_help_depth(),
                current_task_id(),
            ))
            .unwrap();
        Ok(Value::Int(99))
    });
    let marker_receipt = marker_rx.recv_timeout(TEST_WAIT);
    let marker_deadline = Instant::now() + TEST_WAIT;
    if let Ok(marker) = &marker {
        while !marker.state.accounting_finished.load(Ordering::Acquire)
            && Instant::now() < marker_deadline
        {
            thread::yield_now();
        }
    }
    let parent_value = parent
        .state
        .result
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|result| {
            if let Ok(Value::Int(value)) = result {
                Some(*value)
            } else {
                None
            }
        });
    let target_context = target.as_ref().and_then(|target| target.cancellation());
    let nested_context = nested.as_ref().and_then(|nested| nested.cancellation());
    let target_retired = target
        .as_ref()
        .is_some_and(|state| state.accounting_finished.load(Ordering::Acquire));
    let nested_retired = nested
        .as_ref()
        .is_some_and(|state| state.accounting_finished.load(Ordering::Acquire));
    let shutdown = runtime.cancel_all_and_wait(Duration::from_millis(50));
    let snapshot = runtime.snapshot();
    eprintln!("helper_cleanup_panic ready={ready:?} cancelled={cancelled} receipt={receipt:?} marker={marker_receipt:?} parent_value={parent_value:?} target_retired={target_retired} nested_retired={nested_retired} shutdown={shutdown:?} snapshot={snapshot:?}");
    let parent_id = parent.id();
    let marker_id = marker.as_ref().ok().map(TaskHandle::id);
    drop(marker);
    drop(parent);
    drop(target);
    drop(nested);
    drop(runtime);

    let ready = ready.unwrap();
    assert_eq!(
        ready.3, 1,
        "target actually executes through the parent's helper"
    );
    assert!(cancelled);
    let receipt = receipt.unwrap();
    assert_eq!(receipt.panic.as_deref(), Some(PANIC_MESSAGE));
    assert_eq!(receipt.depth, 0);
    assert_eq!(receipt.wait_edges, 0);
    assert_eq!(receipt.status, "running");
    assert_eq!(receipt.id, parent_id);
    assert_eq!(receipt.thread, ready.2);
    assert_eq!(receipt.cleanup, None);
    assert_eq!(receipt.execution, None);
    assert!(receipt.target_retired);
    let marker = marker_receipt.unwrap();
    assert_eq!(marker.0, ready.2);
    assert_eq!(marker.1, 0);
    assert_eq!(Some(marker.2), marker_id);
    assert_eq!(parent_value, Some(55));
    assert_eq!(target_context, Some(context));
    assert_eq!(nested_context, Some(context));
    assert!(target_retired);
    assert!(nested_retired);
    shutdown.unwrap();
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.finished_tasks, 4);
    assert_eq!(snapshot.cleanup_timeouts, 0);
}
