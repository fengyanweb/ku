use super::*;
use std::rc::Rc;

const WAIT: Duration = Duration::from_secs(3);

fn until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while !predicate() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::yield_now();
    }
    true
}

fn code(value: &Value) -> Option<&str> {
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
struct DispatchReceipt {
    id: i64,
    depth: usize,
    thread: thread::ThreadId,
    stack: usize,
}

struct Chain {
    runtime: TaskRuntime,
    remaining: usize,
    ticket: Option<AwaitTicket>,
    receipts: SyncSender<DispatchReceipt>,
    first: bool,
    _local: Rc<()>,
}

impl Chain {
    fn new(runtime: TaskRuntime, remaining: usize, receipts: SyncSender<DispatchReceipt>) -> Self {
        Self {
            runtime,
            remaining,
            ticket: None,
            receipts,
            first: true,
            _local: Rc::new(()),
        }
    }
}

impl LocalTaskRunner for Chain {
    fn dispatch(&mut self, _: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        let stack_marker = 1u8;
        if self.first {
            self.first = false;
            self.receipts
                .try_send(DispatchReceipt {
                    id: current_task_id(),
                    depth: await_help_depth(),
                    thread: thread::current().id(),
                    stack: (&stack_marker as *const u8) as usize,
                })
                .unwrap();
        }
        if let Some(ticket) = &mut self.ticket {
            if ticket.poll() == AwaitPoll::Pending {
                return ControlFlow::Continue(());
            }
            return ControlFlow::Break(self.ticket.take().unwrap().finish_with(|_| {}));
        }
        if self.remaining == 0 {
            return ControlFlow::Break(Ok(Value::Int(17)));
        }
        let runtime = self.runtime.clone();
        let receipts = self.receipts.clone();
        let remaining = self.remaining - 1;
        let child = self
            .runtime
            .spawn_runner_deferred(|| move || Self::new(runtime, remaining, receipts))
            .unwrap();
        self.ticket = Some(AwaitTicket::begin(child));
        ControlFlow::Continue(())
    }

    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        self.ticket.as_mut()
    }
}

fn marker(runtime: &TaskRuntime) -> (thread::ThreadId, usize, usize, usize) {
    let (tx, rx) = mpsc::sync_channel(1);
    let task = runtime
        .spawn(move || {
            tx.send((
                thread::current().id(),
                await_help_depth(),
                FRAME_BACKING_BYTES.with(Cell::get),
                RUNNER_HEADER_BYTES.with(Cell::get),
            ))
            .unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    let receipt = rx.recv_timeout(WAIT);
    let retired = until(|| task.state.accounting_finished.load(Ordering::Acquire));
    assert!(retired);
    assert_eq!(task.await_result().unwrap(), Value::Null);
    receipt.unwrap()
}

#[test]
fn real_waiting_runners_help_flat_on_one_thread_through_the_existing_64_boundary() {
    for remaining in [6, 64, 65] {
        let runtime = TaskRuntime::with_limits(1, 8, 0, 0, MAX_TASKS);
        let child_runtime = runtime.clone();
        let (tx, rx) = mpsc::sync_channel(128);
        let root = runtime
            .spawn_runner_deferred(|| move || Chain::new(child_runtime, remaining, tx))
            .unwrap();
        // The host does not synchronously help while the actual one worker is
        // driving the chain. Observe real accounting before ordinary await.
        let retired = until(|| root.state.accounting_finished.load(Ordering::Acquire));
        let result = if retired {
            Some(root.await_result())
        } else {
            None
        };
        let receipt = marker(&runtime);
        let records: Vec<_> = rx.try_iter().collect();
        let shutdown = runtime.cancel_all_and_wait(WAIT);
        let snapshot = runtime.snapshot().unwrap();
        eprintln!("flat_waiting remaining={remaining} frame_size={} runner_size={} frame_budget={FRAME_BACKING_BUDGET} header_budget={RUNNER_HEADER_BUDGET} max_header={MAX_TYPED_RUNNER_HEADER} records={records:?} marker={receipt:?} snapshot={snapshot:?}", size_of::<LocalJobFrame>(), size_of::<Chain>());
        shutdown.unwrap();
        assert!(retired);
        let result = result.unwrap().unwrap();
        if remaining <= MAX_AWAIT_DEPTH {
            assert_eq!(result, Value::Int(17));
        } else {
            assert_eq!(code(&result), Some("await_depth"));
        }
        assert_eq!(records.len(), remaining.min(MAX_AWAIT_DEPTH) + 1);
        assert_eq!(records[0].id, root.id());
        for (depth, record) in records.iter().enumerate() {
            assert_eq!(record.depth, depth);
            assert_eq!(record.thread, receipt.0);
        }
        let low = records.iter().map(|record| record.stack).min().unwrap();
        let high = records.iter().map(|record| record.stack).max().unwrap();
        assert!(
            high - low < 256,
            "same dispatch call position must stay flat: {records:?}"
        );
        assert_eq!(receipt.1, 0);
        assert_eq!(
            receipt.3, 0,
            "all earlier typed runner header charges retired"
        );
        assert!(receipt.2 <= FRAME_BACKING_BUDGET);
        assert_eq!(snapshot.active_tasks, 0);
        assert_eq!(snapshot.registered_tasks, 0);
        assert_eq!(snapshot.queued_tasks, 0);
        assert_eq!(snapshot.wait_edges, 0);
        assert_eq!(snapshot.finished_tasks, remaining + 2);
    }
}

struct CountDrop(Arc<AtomicUsize>);
impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

struct Oversized {
    _header: [u8; MAX_TYPED_RUNNER_HEADER + 1],
    _capture: CountDrop,
}

impl LocalTaskRunner for Oversized {
    fn dispatch(&mut self, _: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        ControlFlow::Break(Ok(Value::Null))
    }
}

#[test]
fn typed_header_rejection_drops_real_queued_factory_before_completion_without_claim() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, MAX_TASKS);
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = CountDrop(drops.clone());
    let called = Arc::new(AtomicBool::new(false));
    let construction = called.clone();
    let task = runtime
        .spawn_runner_deferred(|| {
            move || {
                construction.store(true, Ordering::Release);
                Oversized {
                    _header: [0; MAX_TYPED_RUNNER_HEADER + 1],
                    _capture: capture,
                }
            }
        })
        .unwrap();
    let error = task.await_result().unwrap_err();
    let snapshot = runtime.snapshot().unwrap();
    runtime.cancel_all_and_wait(WAIT).unwrap();
    eprintln!(
        "typed_header_reject actual={} limit={MAX_TYPED_RUNNER_HEADER} snapshot={snapshot:?}",
        size_of::<Oversized>()
    );
    assert!(error
        .message
        .contains("internal execution storage rejected"));
    assert!(!called.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.accepted_submissions, 0);
    assert_eq!(snapshot.finished_tasks, 0);
}

const HELPER_PANIC: &str = "flat runner helper actual losing payload panic";

struct Release(Option<SyncSender<()>>);
impl Release {
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

#[derive(Debug)]
struct NativeDropReceipt {
    context: Option<CancellationContext>,
    id: i64,
    depth: usize,
    thread: thread::ThreadId,
    socket_released: bool,
}

struct NativeProbe {
    shorter: CancellationContext,
    listener: Option<Value>,
    observer: Weak<crate::value::HttpListenerLease>,
    count: Arc<AtomicUsize>,
    receipt: SyncSender<NativeDropReceipt>,
}

impl Drop for NativeProbe {
    fn drop(&mut self) {
        let _short = CleanupGuard::enter(CancellationContext {
            reason: TerminationReason::Cancelled,
            ..self.shorter
        });
        self.count.fetch_add(1, Ordering::AcqRel);
        drop(self.listener.take());
        let _ = self.receipt.try_send(NativeDropReceipt {
            context: current_cleanup_context(),
            id: current_task_id(),
            depth: await_help_depth(),
            thread: thread::current().id(),
            socket_released: self.observer.upgrade().is_none(),
        });
        panic!("{HELPER_PANIC}");
    }
}

#[derive(Debug)]
struct ParentDropReceipt {
    id: i64,
    depth: usize,
    waits: usize,
    status: &'static str,
    context: Option<CancellationContext>,
    error_context: Option<CancellationContext>,
    cleanup_after: Option<CancellationContext>,
    execution_after: Option<CancellationContext>,
    owner_before: usize,
    owner_after: usize,
}

struct NativeParent {
    runtime: TaskRuntime,
    ticket: Option<AwaitTicket>,
    target: Option<Arc<TaskState>>,
    owner: Option<Weak<TaskOwnerLease>>,
    probe: Option<NativeProbe>,
    ready: Option<SyncSender<Arc<TaskState>>>,
    release: Option<Receiver<()>>,
    receipt: SyncSender<ParentDropReceipt>,
    observer_ran: Arc<AtomicBool>,
    nested_ran: Arc<AtomicBool>,
}

impl LocalTaskRunner for NativeParent {
    fn dispatch(&mut self, _: NonZeroUsize) -> ControlFlow<KuResult<Value>> {
        if let Some(ticket) = &mut self.ticket {
            if ticket.poll() == AwaitPoll::Pending {
                return ControlFlow::Continue(());
            }
            let observer = self.observer_ran.clone();
            return ControlFlow::Break(self.ticket.take().unwrap().finish_with(|_| {
                observer.store(true, Ordering::Release);
            }));
        }
        let runtime = self.runtime.clone();
        let probe = self.probe.take().unwrap();
        let ready = self.ready.take().unwrap();
        let release = self.release.take().unwrap();
        let nested_ran = self.nested_ran.clone();
        let target = self
            .runtime
            .spawn_runner_deferred(|| {
                move || {
                    OnceTaskRunner::new(move || {
                        let nested = runtime
                            .spawn(move || {
                                nested_ran.store(true, Ordering::Release);
                                drop(probe);
                                Ok(Value::Null)
                            })
                            .unwrap();
                        let mut captures = crate::env::Env::new();
                        captures
                            .define_owned(
                                "nested".into(),
                                Value::Task(nested),
                                false,
                                Span::default(),
                            )
                            .unwrap();
                        let state = CURRENT_TASK_STATE
                            .with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade).unwrap());
                        ready.send(state).unwrap();
                        release
                            .recv_timeout(WAIT)
                            .expect("finite host release of running B");
                        Ok(Value::Function {
                            params: Vec::new(),
                            param_modes: Vec::new(),
                            body: Vec::new(),
                            captures,
                            self_name: None,
                            is_async: false,
                        })
                    })
                }
            })
            .unwrap();
        self.target = Some(target.state.clone());
        self.owner = Some(Arc::downgrade(&target.owner));
        self.ticket = Some(AwaitTicket::begin(target));
        ControlFlow::Continue(())
    }

    fn waiting(&mut self) -> Option<&mut AwaitTicket> {
        self.ticket.as_mut()
    }
}

impl Drop for NativeParent {
    fn drop(&mut self) {
        let Some(target) = &self.target else {
            return;
        };
        let context = target.cancellation();
        let error_context = target
            .result
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .and_then(KuError::runtime_termination);
        let owner = self.owner.as_ref().unwrap();
        let owner_before = owner.strong_count();
        drop(self.ticket.take());
        let state =
            CURRENT_TASK_STATE.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade).unwrap());
        let _ = self.receipt.try_send(ParentDropReceipt {
            id: current_task_id(),
            depth: await_help_depth(),
            waits: self.runtime.snapshot().unwrap().wait_edges,
            status: state.status_name(),
            context,
            error_context,
            cleanup_after: current_cleanup_context(),
            execution_after: current_execution_termination(),
            owner_before,
            owner_after: owner.strong_count(),
        });
    }
}

#[test]
fn real_flat_helper_cleanup_panic_retires_child_then_aborts_parent_ticket_under_parent_identity() {
    let runtime = TaskRuntime::with_limits(1, 8, 0, 0, MAX_TASKS);
    let now = Instant::now();
    let original = CancellationContext {
        reason: TerminationReason::TimedOut,
        cleanup_deadline: now - Duration::from_millis(1),
    };
    let shorter = CancellationContext {
        cleanup_deadline: original.cleanup_deadline - Duration::from_millis(1),
        ..original
    };
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let id = crate::runtime::http_listener_registry::insert(socket).unwrap();
    let listener = crate::value::HttpListenerLease::new(id);
    let observer = Arc::downgrade(&listener);
    let count = Arc::new(AtomicUsize::new(0));
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let probe = NativeProbe {
        shorter,
        listener: Some(Value::HttpListenerLease(listener)),
        observer: observer.clone(),
        count: count.clone(),
        receipt: drop_tx,
    };
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let mut release = Release(Some(release_tx));
    let (receipt_tx, receipt_rx) = mpsc::sync_channel(1);
    let observer_ran = Arc::new(AtomicBool::new(false));
    let nested_ran = Arc::new(AtomicBool::new(false));
    let ran = observer_ran.clone();
    let nested = nested_ran.clone();
    let child_runtime = runtime.clone();
    let parent = runtime
        .spawn_runner_deferred(|| {
            move || NativeParent {
                runtime: child_runtime,
                ticket: None,
                target: None,
                owner: None,
                probe: Some(probe),
                ready: Some(ready_tx),
                release: Some(release_rx),
                receipt: receipt_tx,
                observer_ran: ran,
                nested_ran: nested,
            }
        })
        .unwrap();
    let target = ready_rx.recv_timeout(WAIT);
    let cancelled = target
        .as_ref()
        .ok()
        .is_some_and(|target| target.request_cancel_with(original));
    release.release();
    let dropped = drop_rx.recv_timeout(WAIT);
    let parent_drop = receipt_rx.recv_timeout(WAIT);
    let retired = until(|| parent.state.accounting_finished.load(Ordering::Acquire));
    let value = if retired {
        Some(parent.await_result())
    } else {
        None
    };
    let marker = marker(&runtime);
    let shutdown = runtime.cancel_all_and_wait(WAIT);
    let snapshot = runtime.snapshot().unwrap();
    eprintln!("flat_native_helper cancelled={cancelled} dropped={dropped:?} parent_drop={parent_drop:?} marker={marker:?} snapshot={snapshot:?}");
    shutdown.unwrap();
    assert!(cancelled);
    assert!(retired);
    assert_eq!(code(&value.unwrap().unwrap()), Some("panic"));
    assert_eq!(parent.status(), "panicked");
    let dropped = dropped.unwrap();
    let parent_drop = parent_drop.unwrap();
    assert_eq!(dropped.context, Some(shorter));
    assert_eq!(dropped.id, parent.id());
    assert_eq!(dropped.depth, 1);
    assert_eq!(dropped.thread, marker.0);
    assert!(dropped.socket_released);
    assert_eq!(parent_drop.id, parent.id());
    assert_eq!(parent_drop.depth, 0);
    assert_eq!(parent_drop.waits, 0);
    assert_eq!(parent_drop.status, "running");
    assert_eq!(parent_drop.context, Some(shorter));
    assert_eq!(parent_drop.error_context, Some(shorter));
    assert_eq!(parent_drop.cleanup_after, None);
    assert_eq!(parent_drop.execution_after, None);
    assert_eq!(parent_drop.owner_before, 1);
    assert_eq!(parent_drop.owner_after, 0);
    assert_eq!(count.load(Ordering::Acquire), 1);
    assert!(observer.upgrade().is_none());
    assert!(!observer_ran.load(Ordering::Acquire));
    assert!(!nested_ran.load(Ordering::Acquire));
    assert_eq!(marker.1, 0);
    assert_eq!(marker.3, 0);
    assert_eq!(snapshot.active_tasks, 0);
    assert_eq!(snapshot.registered_tasks, 0);
    assert_eq!(snapshot.wait_edges, 0);
    assert_eq!(snapshot.queued_tasks, 0);
    assert_eq!(snapshot.finished_tasks, 4);
}
