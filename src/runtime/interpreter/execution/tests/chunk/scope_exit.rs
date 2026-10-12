use super::*;
use crate::runtime::task::{current_execution_termination, current_task_cancellation, TaskHandle};
use std::cell::Cell;
use std::sync::{atomic::AtomicBool, mpsc, Weak};

const WAIT: Duration = Duration::from_secs(3);
const NORMAL: &str = r#"
fn Entry(): int {
    parent = 0
    if (true) { marker = 41 }
    parent = parent + 1
    return parent
}
"#;
const RETURNING: &str = r#"
fn Entry(): int {
    parent = 0
    if (true) { marker = 41
        return marker }
    return parent
}
"#;
const TERMINATING: &str = r#"
fn Entry(): int {
    parent = 0
    if (true) { marker = 41
        broken = 1 / 0 }
    return parent
}
"#;
const PAYLOAD: &str = r#"
fn Make() {
    fn Payload(n: int): int { return n }
    return Payload.clone()
}
"#;

struct Gate(Option<SyncSender<()>>);
impl Gate {
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
impl Drop for Gate {
    fn drop(&mut self) {
        self.release();
    }
}

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

struct Blockers {
    gates: Vec<Gate>,
    _owners: Vec<TaskHandle>,
}
impl Blockers {
    fn start(runtime: &TaskRuntime, count: usize) -> Self {
        let mut blocked = Self {
            gates: Vec::new(),
            _owners: Vec::new(),
        };
        for _ in 0..count {
            let (gate, released) = Gate::pair();
            blocked.gates.push(gate);
            let (tx, rx) = mpsc::sync_channel(1);
            blocked._owners.push(
                runtime
                    .spawn(move || {
                        tx.send(thread::current().id()).unwrap();
                        released.recv_timeout(WAIT).expect("finite blocker release");
                        Ok(Value::Null)
                    })
                    .unwrap(),
            );
            assert_ne!(rx.recv_timeout(WAIT).unwrap(), thread::current().id());
        }
        blocked
    }
    fn release(&mut self) {
        for gate in &mut self.gates {
            gate.release();
        }
    }
}

#[derive(Debug)]
struct DropReceipt {
    context: Option<RuntimeTermination>,
    socket_released: bool,
}
struct NativeCapture {
    payload: Option<Value>,
    observer: Weak<HttpListenerLease>,
    shorter: Option<RuntimeTermination>,
    pause: Option<Receiver<()>>,
    started: SyncSender<Option<RuntimeTermination>>,
    finished: SyncSender<DropReceipt>,
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for NativeCapture {
    fn drop(&mut self) {
        if let Some(shorter) = self.shorter {
            drop(CleanupGuard::enter(shorter));
        }
        let _ = self.started.try_send(current_cleanup_context());
        if let Some(pause) = &self.pause {
            pause
                .recv_timeout(WAIT)
                .expect("finite actual losing-payload Drop release");
        }
        drop(self.payload.take());
        self.drops.fetch_add(1, Ordering::AcqRel);
        let _ = self.finished.try_send(DropReceipt {
            context: current_cleanup_context(),
            socket_released: self.observer.strong_count() == 0,
        });
        if self.panic {
            panic!("actual scope capture native Drop panic");
        }
    }
}

struct CaptureFixture {
    release: Gate,
    started: Receiver<Option<RuntimeTermination>>,
    finished: Receiver<DropReceipt>,
    observer: Weak<HttpListenerLease>,
    drops: Arc<AtomicUsize>,
    ran: Arc<AtomicBool>,
}
fn capture(
    pause: bool,
    shorter: Option<RuntimeTermination>,
    panic: bool,
) -> (CaptureFixture, NativeCapture) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let id = http_listener_registry::insert(listener).unwrap();
    let lease = HttpListenerLease::new(id);
    let observer = Arc::downgrade(&lease);
    let (release, released) = Gate::pair();
    let (start_tx, started) = mpsc::sync_channel(1);
    let (done_tx, finished) = mpsc::sync_channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let probe = NativeCapture {
        payload: Some(Value::HttpListenerLease(lease)),
        observer: observer.clone(),
        shorter,
        pause: pause.then_some(released),
        started: start_tx,
        finished: done_tx,
        drops: drops.clone(),
        panic,
    };
    (
        CaptureFixture {
            release,
            started,
            finished,
            observer,
            drops,
            ran,
        },
        probe,
    )
}

struct LateChild {
    body: Gate,
    capture: CaptureFixture,
    owner: Option<TaskHandle>,
}
impl LateChild {
    fn start(runtime: &TaskRuntime, pause_drop: bool) -> Self {
        let (body, released) = Gate::pair();
        let (capture, probe) = capture(pause_drop, None, false);
        let ran = capture.ran.clone();
        let child_runtime = runtime.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let owner = runtime
            .spawn(move || {
                let nested = child_runtime
                    .spawn(move || {
                        ran.store(true, Ordering::Release);
                        drop(probe);
                        Ok(Value::Null)
                    })
                    .unwrap();
                // This is a real source-created function value. The additional
                // capture is a deliberate kernel fixture owner, not new admission
                // of general Owned language values into the scalar Machine.
                let mut host = production_interpreter(PAYLOAD);
                let mut payload = host.call_function("Make", vec![], Span::default(), 0)?;
                let Value::Function { captures, .. } = &mut payload else {
                    unreachable!()
                };
                captures.define_owned(
                    "nested".into(),
                    Value::Task(nested),
                    false,
                    Span::default(),
                )?;
                tx.send(crate::runtime::task::current_task_id()).unwrap();
                released
                    .recv_timeout(WAIT)
                    .expect("finite running child release");
                assert!(
                    until(|| current_task_cancellation().is_some()),
                    "real scope must cancel its running child before completion"
                );
                Ok(payload)
            })
            .unwrap();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), owner.id());
        Self {
            body,
            capture,
            owner: Some(owner),
        }
    }
}

struct ScopeHooks {
    trace: TraceHooks,
    span: Span,
    child: Option<TaskHandle>,
    prepared: usize,
    fallback: Vec<(i64, i64, bool)>,
    fallback_continues: usize,
    feedback_calls: usize,
    fallback_release: Option<Gate>,
    terminate: Option<(Span, RuntimeTermination)>,
}
impl ScopeHooks {
    fn new(source: &HashMap<String, FnDecl>, runtime: &TaskRuntime, child: TaskHandle) -> Self {
        let Stmt::If { then_branch, .. } = &source["Entry"].body[1] else {
            unreachable!()
        };
        let mut trace = TraceHooks::new();
        result_entry::install(&mut trace.host, source);
        trace.host.task_runtime = Some(runtime.clone());
        Self {
            trace,
            span: stmt_span(&then_branch[0]),
            child: Some(child),
            prepared: 0,
            fallback: Vec::new(),
            fallback_continues: 0,
            feedback_calls: 0,
            fallback_release: None,
            terminate: None,
        }
    }

    fn witness_fallback(&mut self, env: &Env, result: &KuResult<Flow>, span: Span) {
        if span != self.span {
            return;
        }
        let id = env
            .with_value("held", span, |value| match value {
                Value::Task(task) => Ok(task.id()),
                _ => unreachable!(),
            })
            .unwrap();
        self.fallback.push((
            scalar_binding(env, "marker"),
            id,
            matches!(
                result,
                Ok(Flow::Return {
                    value: Value::Int(41),
                    ..
                })
            ),
        ));
        self.fallback_continues += usize::from(matches!(result, Ok(Flow::Continue)));
        if let Some(mut gate) = self.fallback_release.take() {
            gate.release();
        }
    }
}
fn scalar_binding(env: &Env, name: &str) -> i64 {
    env.with_value(name, Span::default(), |value| match value {
        Value::Int(value) => Ok(*value),
        _ => Err(KuError::runtime(
            "fixture scalar binding changed",
            Span::default(),
        )),
    })
    .unwrap()
}
impl ExecutionHooks for ScopeHooks {
    fn host(&mut self) -> &mut Interpreter {
        &mut self.trace.host
    }
    fn tick(&mut self, span: Span) -> KuResult<()> {
        if self.terminate.is_some_and(|(at, _)| at == span) {
            self.trace.ticks.push(span);
            return Err(KuError::termination(self.terminate.take().unwrap().1, span));
        }
        self.trace.tick(span)
    }
    fn prepare_scope_exit(&mut self, env: &mut Env, span: Span) -> KuResult<()> {
        if span == self.span {
            self.prepared += 1;
            assert_eq!(scalar_binding(env, "marker"), 41);
            env.define_owned(
                "held".into(),
                Value::Task(self.child.take().expect("inject sole real owner once")),
                false,
                span,
            )?;
        }
        Ok(())
    }
    fn finish_scope(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
    ) -> KuResult<Flow> {
        self.witness_fallback(env, &result, span);
        self.trace.finish_scope(env, result, span, pop)
    }
    fn finish_scope_with_feedback(
        &mut self,
        env: &mut Env,
        result: KuResult<Flow>,
        span: Span,
        pop: bool,
        feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        self.witness_fallback(env, &result, span);
        self.feedback_calls += 1;
        self.trace.events.push(Event::Scope(span, pop));
        // Keep the production feedback owner around the one actual cleanup;
        // calling TraceHooks::finish_scope here would use the older adapter.
        self.trace
            .host
            .finish_owned_scope_with_feedback(env, result, span, pop, feedback)
    }
    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        self.trace.finish_call(env, leases, result, span, kind)
    }
    fn finish_arguments(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
    ) -> KuResult<Flow> {
        self.trace
            .finish_arguments(env, leases, values, result, span)
    }
    fn prepare_finally(
        &mut self,
        env: &Env,
        primary: &KuResult<Flow>,
        span: Span,
    ) -> cancellation::FinallyMode {
        self.trace.prepare_finally(env, primary, span)
    }
    fn finish_finally(
        &mut self,
        primary: KuResult<Flow>,
        replacement: KuResult<Flow>,
        mode: cancellation::FinallyMode,
        span: Span,
    ) -> KuResult<Flow> {
        self.trace.finish_finally(primary, replacement, mode, span)
    }
}

fn machine(source: &HashMap<String, FnDecl>) -> Machine {
    let code = CodeHandle::compile_named(source, &source["Entry"], Limits::default()).unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    machine.start_once("Entry", &[], 0, false).unwrap();
    machine
}
fn step(machine: &mut Machine, hooks: &mut ScopeHooks) -> ChunkProgress {
    // Direct library driving uses the same short execution TLS boundary as
    // a claimant. The actual persistent exit retains no synchronous guard.
    let _execution = ExecutionTerminationGuard::enter();
    machine
        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), hooks)
        .unwrap()
}
fn to_scope(machine: &mut Machine, hooks: &mut ScopeHooks) {
    for _ in 0..10_000 {
        if matches!(machine.work.get(machine.work.len() - 1), Some(Work::ScopeExit { span, .. }) if machine.code.span(*span) == hooks.span)
        {
            return;
        }
        assert!(!matches!(
            step(machine, hooks),
            ChunkProgress::Finished { .. }
        ));
    }
    panic!("finite source never reached its real ScopeExit");
}
fn finish(machine: &mut Machine, hooks: &mut ScopeHooks) -> KuResult<Value> {
    for _ in 0..10_000 {
        match step(machine, hooks) {
            ChunkProgress::More { .. } => {}
            ChunkProgress::Waiting { .. } => machine.wait_synchronously(&hooks.trace.host),
            ChunkProgress::Finished { result, .. } => {
                return result.and_then(|flow| flow.into_call_value(Span::default(), "function"))
            }
        }
    }
    panic!("finite source did not complete");
}
fn drain(runtime: &TaskRuntime, finished: usize) {
    assert!(until(
        || runtime.snapshot().unwrap().finished_tasks == finished
    ));
    runtime.cancel_all_and_wait(WAIT).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges,
            snapshot.finished_tasks
        ),
        (0, 0, 0, 0, finished)
    );
}

#[test]
fn pending_scope_keeps_real_env_and_parent_work_but_terminal_is_not_payload_retirement() {
    let runtime = TaskRuntime::with_limits(2, 1024, 0, 0, 1024);
    let mut blocked = Blockers::start(&runtime, 1);
    let mut child = LateChild::start(&runtime, true);
    let source = functions(NORMAL);
    let mut hooks = ScopeHooks::new(&source, &runtime, child.owner.take().unwrap());
    let mut machine = machine(&source);
    to_scope(&mut machine, &mut hooks);
    let mut paused = false;
    for _ in 0..32 {
        if matches!(
            step(&mut machine, &mut hooks),
            ChunkProgress::Waiting { .. }
        ) {
            paused = true;
            break;
        }
    }
    assert!(paused);
    let exit = machine.exits.get(0).unwrap();
    let context = exit.state.context().unwrap();
    let owner = exit.owner_call;
    let env = &machine.calls.get(owner - 1).unwrap().env;
    let frozen = (
        machine.work.len(),
        machine.call_depth,
        hooks.trace.ticks.len(),
        scalar_binding(env, "parent"),
        scalar_binding(env, "marker"),
    );
    assert_eq!(frozen.3, 0);
    assert_eq!(frozen.4, 41);
    assert_eq!(env.current_scope_simple_task_count(hooks.span).unwrap(), 1);
    assert!(machine.awaiting.is_none() && machine.pending.is_none());
    assert_eq!((machine.exits.len(), owner), (1, 1));
    assert_eq!(exit.state.phase(), cancellation::ExitPhase::WaitTerminal);
    assert!(machine.exit_handle_capacity > 0);
    assert_eq!(
        machine.exit_handle_bytes,
        machine.exit_handle_capacity * std::mem::size_of::<TaskHandle>()
    );
    assert_eq!(current_cleanup_context(), None);
    assert_eq!(current_execution_termination(), None);
    machine.wait_synchronously(&hooks.trace.host);
    assert!(matches!(
        step(&mut machine, &mut hooks),
        ChunkProgress::Waiting { .. }
    ));
    let env = &machine.calls.get(owner - 1).unwrap().env;
    assert_eq!(
        frozen,
        (
            machine.work.len(),
            machine.call_depth,
            hooks.trace.ticks.len(),
            scalar_binding(env, "parent"),
            scalar_binding(env, "marker")
        )
    );
    assert_eq!(machine.exits.get(0).unwrap().state.context(), Some(context));
    assert_eq!(hooks.prepared, 1);
    assert!(
        hooks.fallback.is_empty(),
        "task-bearing scope must not call the synchronous hook"
    );
    assert_eq!(current_cleanup_context(), None);
    child.body.release();
    let dropping = child.capture.started.recv_timeout(WAIT);
    let terminal = env
        .with_value("held", hooks.span, |value| match value {
            Value::Task(task) => Ok(task.status()),
            _ => unreachable!(),
        })
        .unwrap();
    let result = finish(&mut machine, &mut hooks);
    let before_release = runtime.snapshot().unwrap();
    let socket_still_held = child.capture.observer.strong_count() != 0;
    let drops_before = child.capture.drops.load(Ordering::Acquire);
    child.capture.release.release();
    let released = child.capture.finished.recv_timeout(WAIT);
    blocked.release();
    drain(&runtime, 3);
    eprintln!("scope terminal={terminal} dropping={dropping:?} before_actual_payload_release={before_release:?} released={released:?} exit_header={} retained={} peak_handles={}", std::mem::size_of::<PendingScopeExit>(), machine.exits.charged_storage_bytes(), machine.peak_exit_handle_bytes);
    assert_eq!(result.unwrap(), Value::Int(1));
    assert_eq!(terminal, "cancelled");
    assert_eq!(dropping.unwrap(), Some(context));
    assert_eq!(before_release.finished_tasks, 0);
    assert_eq!(
        (before_release.active_tasks, before_release.queued_tasks),
        (3, 1)
    );
    assert!(socket_still_held);
    assert_eq!(drops_before, 0);
    let released = released.unwrap();
    assert_eq!(released.context, Some(context));
    assert!(released.socket_released);
    assert_eq!(child.capture.drops.load(Ordering::Acquire), 1);
    assert!(!child.capture.ran.load(Ordering::Acquire));
    assert_eq!(
        (
            machine.metrics.entered_exit_frames,
            machine.metrics.retired_exit_frames,
            machine.metrics.remaining_exit_frames
        ),
        (1, 1, 0)
    );
    assert_eq!(
        (machine.exit_handle_capacity, machine.exit_handle_bytes),
        (0, 0)
    );
    assert_drained(&machine.metrics, 0);
}

#[test]
fn scope_phase_panic_saves_minimum_d_separately_from_primary_cause_and_restores_tls() {
    for panics in [false, true] {
        let runtime = TaskRuntime::with_limits(2, 1024, 0, 0, 1024);
        let mut blocked = Blockers::start(&runtime, 2);
        let now = Instant::now();
        let primary = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now + Duration::from_secs(3),
        };
        let ambient = RuntimeTermination {
            reason: TerminationReason::TimedOut,
            cleanup_deadline: now + Duration::from_secs(2),
        };
        let shorter = RuntimeTermination {
            reason: TerminationReason::Cancelled,
            cleanup_deadline: now + Duration::from_secs(1),
        };
        let effective = RuntimeTermination {
            reason: ambient.reason,
            cleanup_deadline: shorter.cleanup_deadline,
        };
        let (capture, probe) = capture(false, Some(shorter), panics);
        let ran = capture.ran.clone();
        let child = runtime
            .spawn(move || {
                ran.store(true, Ordering::Release);
                drop(probe);
                Ok(Value::Null)
            })
            .unwrap();
        let source = functions(TERMINATING);
        let mut hooks = ScopeHooks::new(&source, &runtime, child);
        let Stmt::If { then_branch, .. } = &source["Entry"].body[1] else {
            unreachable!()
        };
        hooks.terminate = Some((stmt_span(&then_branch[1]), primary));
        let mut machine = machine(&source);
        to_scope(&mut machine, &mut hooks);
        let ambient_guard = CleanupGuard::enter(ambient);
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..32 {
                step(&mut machine, &mut hooks);
                if capture.drops.load(Ordering::Acquire) != 0 {
                    return;
                }
            }
            panic!("finite real broadcast never reached its queued capture");
        }));
        let saved = machine.exits.get(0).unwrap().state.release_context();
        let meta = machine.exits.get(0).unwrap().state.context();
        let aborted = machine.aborted_release;
        let after_phase = current_cleanup_context();
        drop(ambient_guard);
        let outside = current_cleanup_context();
        let result = if observed.is_ok() {
            Some(finish(&mut machine, &mut hooks))
        } else {
            None
        };
        let drop_machine = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(machine)));
        let receipt = capture.finished.recv_timeout(WAIT);
        blocked.release();
        assert!(until(|| runtime.snapshot().unwrap().finished_tasks == 3));
        let (tx, rx) = mpsc::sync_channel(1);
        let healthy = runtime
            .spawn(move || {
                tx.send((
                    current_cleanup_context(),
                    current_execution_termination(),
                    crate::runtime::task::current_task_id(),
                ))
                .unwrap();
                Ok(Value::Int(7))
            })
            .unwrap();
        let healthy_context = rx.recv_timeout(WAIT);
        let healthy_id = healthy.id();
        assert!(until(|| runtime.snapshot().unwrap().finished_tasks == 4));
        let healthy_result = healthy.await_result();
        drop(healthy);
        drain(&runtime, 4);
        eprintln!("scope native={panics} meta={meta:?} saved={saved:?} aborted={aborted:?} phase={after_phase:?} released={receipt:?}");
        assert_eq!(observed.is_err(), panics);
        assert_eq!(meta.unwrap().reason, primary.reason);
        assert_eq!(saved, Some(effective));
        assert_eq!(after_phase, Some(effective));
        assert_eq!(outside, None);
        assert!(drop_machine.is_ok());
        assert_eq!(receipt.unwrap().context, Some(effective));
        assert_eq!(capture.drops.load(Ordering::Acquire), 1);
        assert_eq!(capture.observer.strong_count(), 0);
        assert!(!capture.ran.load(Ordering::Acquire));
        assert_eq!(healthy_context.unwrap(), (None, None, healthy_id));
        assert_eq!(healthy_result.unwrap(), Value::Int(7));
        if panics {
            assert_eq!(aborted, Some(effective));
        } else {
            let error = result.unwrap().unwrap_err();
            assert_eq!(error.runtime_termination().unwrap().reason, primary.reason);
            assert_eq!(
                error.scope_cleanup(),
                Some(RuntimeTermination {
                    reason: primary.reason,
                    cleanup_deadline: shorter.cleanup_deadline
                })
            );
        }
    }
}

#[test]
fn scope_frame_and_actual_handle_backing_rejection_leave_original_primary_and_env_for_cleanup() {
    for handles in [false, true] {
        let runtime = TaskRuntime::with_limits(2, 1024, 0, 0, 1024);
        let mut blocked = Blockers::start(&runtime, 1);
        let mut child = LateChild::start(&runtime, false);
        let id = child.owner.as_ref().unwrap().id();
        let source = functions(RETURNING);
        let mut hooks = ScopeHooks::new(&source, &runtime, child.owner.take().unwrap());
        hooks.fallback_release = Some(Gate(child.body.0.take()));
        let mut machine = machine(&source);
        to_scope(&mut machine, &mut hooks);
        assert!(matches!(
            machine.pending.as_ref(),
            Some(Ok(Flow::Return {
                value: Value::Int(41),
                ..
            }))
        ));
        if handles {
            machine.value_limits.max_storage_bytes =
                machine.values.charged_storage_bytes() + machine.argument_bytes;
        } else {
            machine.work_limits.max_storage_bytes = machine.work.charged_storage_bytes()
                + machine.finally.frames.charged_storage_bytes();
        }
        let result = finish(&mut machine, &mut hooks);
        let released = child.capture.finished.recv_timeout(WAIT);
        blocked.release();
        drain(&runtime, 3);
        eprintln!("scope reject handles={handles} fallback={:?} result={result:?} frame_size={} frame_bytes={} peak_handles={}", hooks.fallback, std::mem::size_of::<PendingScopeExit>(), machine.exits.charged_storage_bytes(), machine.peak_exit_handle_bytes);
        assert_eq!(
            hooks.fallback,
            vec![(41, id, true)],
            "admission failure must pass the real sole primary and original scope to cleanup"
        );
        assert!(result
            .unwrap_err()
            .message
            .contains("internal execution storage rejected"));
        assert_eq!(
            (
                machine.metrics.entered_exit_frames,
                machine.metrics.retired_exit_frames,
                machine.metrics.remaining_exit_frames
            ),
            (0, 0, 0)
        );
        assert_eq!(
            (machine.exit_handle_capacity, machine.exit_handle_bytes),
            (0, 0)
        );
        assert_eq!(
            machine.peak_exit_handle_bytes, 0,
            "reject before allocating a handle candidate"
        );
        assert!(released.unwrap().socket_released);
        assert_eq!(child.capture.drops.load(Ordering::Acquire), 1);
        assert!(!child.capture.ran.load(Ordering::Acquire));
        assert_eq!(current_cleanup_context(), None);
        assert_drained(&machine.metrics, 0);
    }
}

#[test]
fn scope_continue_budget_fallback_keeps_feedback_after_normal_and_native_cleanup() {
    for handles in [false, true] {
        for panics in [false, true] {
            let runtime = TaskRuntime::with_limits(2, 1024, 0, 0, 1024);
            let mut blocked = Blockers::start(&runtime, 2);
            let now = Instant::now();
            let primary = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now + WAIT,
            };
            let ambient = RuntimeTermination {
                reason: TerminationReason::TimedOut,
                cleanup_deadline: now + Duration::from_secs(2),
            };
            let shorter = RuntimeTermination {
                reason: TerminationReason::Cancelled,
                cleanup_deadline: now + Duration::from_secs(1),
            };
            let effective = RuntimeTermination {
                reason: ambient.reason,
                cleanup_deadline: shorter.cleanup_deadline,
            };
            let metadata = RuntimeTermination {
                reason: primary.reason,
                cleanup_deadline: shorter.cleanup_deadline,
            };
            let (capture, probe) = capture(false, Some(shorter), panics);
            let ran = capture.ran.clone();
            let child = runtime
                .spawn(move || {
                    ran.store(true, Ordering::Release);
                    drop(probe);
                    Ok(Value::Null)
                })
                .unwrap();
            let child_id = child.id();
            let source = functions(NORMAL);
            let mut hooks = ScopeHooks::new(&source, &runtime, child);
            let mut machine = machine(&source);
            to_scope(&mut machine, &mut hooks);
            assert!(machine.pending.is_none(), "real source reaches Continue");

            // Inject only the real interpreter's cancellation observation at
            // the dispatch boundary; the source outcome remains Continue and
            // the queued Task still has its actual sole owner in the real Env.
            // This selects metadata's first cause independently of ambient TLS.
            {
                let _execution = ExecutionTerminationGuard::enter();
                let cancellation: KuResult<Flow> = Err(KuError::termination(primary, hooks.span));
                hooks.trace.host.observe_termination(&cancellation);
            }
            if handles {
                machine.value_limits.max_storage_bytes =
                    machine.values.charged_storage_bytes() + machine.argument_bytes;
            } else {
                machine.work_limits.max_storage_bytes = machine.work.charged_storage_bytes()
                    + machine.finally.frames.charged_storage_bytes();
            }

            let ambient_guard = CleanupGuard::enter(ambient);
            let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                step(&mut machine, &mut hooks)
            }));
            // No persistent scope was admitted. On native unwind the adapter's
            // local ExitState has already dropped; the Machine Cell must still
            // contain the destructor's tighter D, including ambient's cause.
            let saved = machine.fallback_release.get();
            let first_cause = machine.fallback_metadata;
            let aborted = machine.aborted_release;
            let state = machine.state;
            let after_adapter = current_cleanup_context();
            let execution_after_adapter = current_execution_termination();
            let before_retirement = runtime.snapshot().unwrap();
            // This queued capture is destroyed synchronously by the adapter.
            // Read completed receipts immediately, without waiting out D or
            // releasing the workers so that they could execute its callback.
            let started = capture.started.try_recv();
            let released = capture.finished.try_recv();
            drop(ambient_guard);
            let outside = (current_cleanup_context(), current_execution_termination());
            let result = if observed.is_ok() {
                Some(finish(&mut machine, &mut hooks))
            } else {
                None
            };
            let exit_metrics = (
                machine.metrics.entered_exit_frames,
                machine.metrics.retired_exit_frames,
                machine.metrics.remaining_exit_frames,
            );
            let drained = machine.metrics.entered_calls == machine.metrics.retired_calls
                && machine.metrics.remaining_calls == 0
                && machine.metrics.remaining_work == 0
                && machine.metrics.remaining_values == 0
                && machine.metrics.entered_finally_frames == machine.metrics.retired_finally_frames
                && machine.metrics.remaining_finally_frames == 0
                && machine.metrics.logical_depth == 0
                && machine.metrics.retained_backing_bytes
                    <= machine.metrics.peak_backing_bytes_upper_bound;
            let admission = (
                machine.exits.len(),
                machine.exit_handle_capacity,
                machine.exit_handle_bytes,
                machine.peak_exit_handle_bytes,
            );
            let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(machine)));

            // The cancelled queue node is still real accounting, rather than
            // a manufactured cleanup ACK. Release worker gates before checking
            // outcomes, and await the healthy marker only after real retirement.
            blocked.release();
            let retired = until(|| runtime.snapshot().unwrap().finished_tasks == 3);
            let (tx, rx) = mpsc::sync_channel(1);
            let healthy = runtime
                .spawn(move || {
                    tx.send((
                        current_cleanup_context(),
                        current_execution_termination(),
                        crate::runtime::task::current_task_id(),
                    ))
                    .unwrap();
                    Ok(Value::Int(7))
                })
                .unwrap();
            let healthy_id = healthy.id();
            let healthy_context = rx.recv_timeout(WAIT);
            let marker_retired = until(|| runtime.snapshot().unwrap().finished_tasks == 4);
            let healthy_result = healthy.await_result();
            drop(healthy);
            drain(&runtime, 4);
            eprintln!("scope Continue reject handles={handles} native={panics} first_cause={first_cause:?} saved={saved:?} aborted={aborted:?} before_retirement={before_retirement:?} released={released:?}");

            assert_eq!(hooks.prepared, 1);
            assert_eq!(hooks.fallback, vec![(41, child_id, false)]);
            assert_eq!((hooks.fallback_continues, hooks.feedback_calls), (1, 1));
            assert_eq!(
                hooks
                    .trace
                    .events
                    .iter()
                    .filter(
                        |event| matches!(event, Event::Scope(span, true) if *span == hooks.span)
                    )
                    .count(),
                1,
                "feedback delegation must preserve one real scope epilogue"
            );
            assert_eq!(admission, (0, 0, 0, 0));
            assert_eq!(exit_metrics, (0, 0, 0));
            assert_eq!(saved, Some(effective));
            assert_eq!(first_cause.unwrap().reason, primary.reason);
            assert_eq!(after_adapter, Some(effective));
            assert_eq!(execution_after_adapter, None);
            assert_eq!(outside, (None, None));
            assert_eq!(started.unwrap(), Some(effective));
            let released = released.unwrap();
            assert_eq!(released.context, Some(effective));
            assert!(released.socket_released);
            assert_eq!(capture.drops.load(Ordering::Acquire), 1);
            assert_eq!(capture.observer.strong_count(), 0);
            assert!(!capture.ran.load(Ordering::Acquire));
            assert_eq!(
                (
                    before_retirement.active_tasks,
                    before_retirement.queued_tasks,
                    before_retirement.finished_tasks
                ),
                (3, 1, 0)
            );
            assert!(dropped.is_ok());
            assert!(retired && marker_retired);
            assert_eq!(healthy_context.unwrap(), (None, None, healthy_id));
            assert_eq!(healthy_result.unwrap(), Value::Int(7));
            assert_eq!(current_cleanup_context(), None);
            assert_eq!(current_execution_termination(), None);
            if panics {
                let panic = match observed {
                    Err(panic) => panic,
                    Ok(_) => panic!("actual captured native destructor must panic"),
                };
                assert_eq!(
                    panic.downcast_ref::<&str>().copied(),
                    Some("actual scope capture native Drop panic")
                );
                assert_eq!(state, MachineState::Aborted);
                assert_eq!(aborted, Some(effective));
            } else {
                assert!(matches!(observed.unwrap(), ChunkProgress::More { .. }));
                assert_eq!(first_cause, Some(metadata));
                let error = result.unwrap().unwrap_err();
                assert!(error
                    .message
                    .contains("internal execution storage rejected"));
                assert_eq!(error.runtime_termination(), None);
                assert_eq!(error.scope_cleanup(), Some(metadata));
                assert_eq!(aborted, None);
                assert!(drained);
            }
        }
    }
}
