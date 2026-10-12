use super::*;
use crate::runtime::task::{current_execution_termination, current_task_cancellation, TaskHandle};
use std::cell::Cell;
use std::sync::{atomic::AtomicBool, mpsc};

const WAIT: Duration = Duration::from_secs(3);
const NAMED: &str = r#"
fn Leaf(n: int): int { return n + 1 }
fn Entry(n: int): int {
    parent = 40
    answer = Leaf(n)
    parent = parent + answer
    return parent
}
"#;
const ENTRY: &str = "fn Entry(n: int): int { return n + 1 }";
const LOCAL: &str = r#"
fn Make() {
    OFFSET = 20
    fn local(n: int): int {
        if (n <= 0) { return OFFSET }
        return 1 + local(n - 1)
    }
    return local.clone()
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

struct Running {
    release: Gate,
    owner: Option<TaskHandle>,
    done: Receiver<(i64, Option<RuntimeTermination>)>,
}
impl Running {
    fn start(runtime: &TaskRuntime, wait_cancel: bool) -> Self {
        let (release, released) = Gate::pair();
        let (tx, started) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let owner = runtime
            .spawn(move || {
                let id = crate::runtime::task::current_task_id();
                tx.send((id, thread::current().id())).unwrap();
                released.recv_timeout(WAIT).expect("finite real task gate");
                if wait_cancel {
                    assert!(until(|| current_task_cancellation().is_some()));
                }
                done_tx.send((id, current_task_cancellation())).unwrap();
                Ok(Value::Null)
            })
            .unwrap();
        let (id, worker) = started.recv_timeout(WAIT).unwrap();
        assert_eq!(id, owner.id());
        assert_ne!(worker, thread::current().id());
        Self {
            release,
            owner: Some(owner),
            done,
        }
    }
}

struct CallHooks {
    trace: TraceHooks,
    target: Option<Span>,
    child: Option<TaskHandle>,
    release: Option<Gate>,
    injected: usize,
    fallback: Vec<(i64, i64, bool)>,
    normalized: usize,
}
impl CallHooks {
    fn new(source: &HashMap<String, FnDecl>) -> Self {
        let mut trace = TraceHooks::new();
        result_entry::install(&mut trace.host, source);
        Self {
            trace,
            target: None,
            child: None,
            release: None,
            injected: 0,
            fallback: Vec::new(),
            normalized: 0,
        }
    }
    fn fallback_witness(&mut self, env: &Env, result: &KuResult<Flow>, span: Span) {
        if self.target != Some(span) {
            return;
        }
        let id = env
            .with_value("held", span, |value| match value {
                Value::Task(task) => Ok(task.id()),
                _ => unreachable!(),
            })
            .unwrap();
        self.fallback.push((
            scalar(env, "n"),
            id,
            matches!(
                result,
                Ok(Flow::Return {
                    value: Value::Int(3),
                    ..
                })
            ),
        ));
        if let Some(mut gate) = self.release.take() {
            gate.release();
        }
    }
}
impl ExecutionHooks for CallHooks {
    fn host(&mut self) -> &mut Interpreter {
        &mut self.trace.host
    }
    fn tick(&mut self, span: Span) -> KuResult<()> {
        self.trace.tick(span)
    }
    fn prepare_call_exit(&mut self, env: &mut Env, span: Span, _kind: CallKind) -> KuResult<()> {
        if self.target == Some(span) {
            if let Some(child) = self.child.take() {
                // Explicit kernel fixture injection into the actual parameter
                // scope; this does not admit Owned language parameters.
                assert_eq!(scalar(env, "n"), 2);
                env.define_owned("held".into(), Value::Task(child), false, span)?;
                self.injected += 1;
            }
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
        self.trace.events.push(Event::Scope(span, pop));
        ExecutionHooks::finish_scope_with_feedback(
            &mut self.trace.host,
            env,
            result,
            span,
            pop,
            feedback,
        )
    }
    fn finish_call(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        self.fallback_witness(&env, &result, span);
        self.trace.finish_call(env, leases, result, span, kind)
    }
    fn finish_call_with_feedback(
        &mut self,
        env: Env,
        leases: Vec<BorrowLease>,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
        feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        self.fallback_witness(&env, &result, span);
        self.trace
            .finish_call_with_feedback(env, leases, result, span, kind, feedback)
    }
    fn finish_call_value(
        &mut self,
        result: KuResult<Flow>,
        span: Span,
        kind: CallKind,
    ) -> KuResult<Flow> {
        self.normalized += 1;
        self.trace.finish_call_value(result, span, kind)
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
    fn prepare_arguments_exit(
        &mut self,
        env: &mut Env,
        leases: &[BorrowLease],
        values: &mut [Value],
        span: Span,
    ) -> KuResult<()> {
        self.trace.prepare_arguments_exit(env, leases, values, span)
    }

    fn finish_arguments_with_feedback(
        &mut self,
        env: &mut Env,
        leases: Vec<BorrowLease>,
        values: Vec<Value>,
        result: KuResult<Flow>,
        span: Span,
        feedback: &Cell<Option<RuntimeTermination>>,
    ) -> KuResult<Flow> {
        self.trace
            .finish_arguments_with_feedback(env, leases, values, result, span, feedback)
    }

    fn finish_arguments_value(&mut self, result: KuResult<Flow>, span: Span) -> KuResult<Flow> {
        self.trace.finish_arguments_value(result, span)
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
fn scalar(env: &Env, name: &str) -> i64 {
    env.with_value(name, Span::default(), |value| match value {
        Value::Int(value) => Ok(*value),
        _ => unreachable!(),
    })
    .unwrap()
}
fn named(source: &HashMap<String, FnDecl>) -> Machine {
    let code = CodeHandle::compile_named(source, &source["Entry"], Limits::default()).unwrap();
    let mut machine = Machine::from_code(code, Limits::default(), 0);
    machine
        .start_once("Entry", &[Value::Int(2)], 0, false)
        .unwrap();
    machine
}
fn step(machine: &mut Machine, hooks: &mut CallHooks) -> ChunkProgress {
    let _execution = ExecutionTerminationGuard::enter();
    machine
        .dispatch_chunk(NonZeroUsize::new(1).unwrap(), hooks)
        .unwrap()
}
fn to_call(machine: &mut Machine, hooks: &mut CallHooks, owner: usize) {
    for _ in 0..20_000 {
        if machine.calls.len() == owner
            && matches!(
                machine.work.get(machine.work.len() - 1),
                Some(Work::CallExit)
            )
            && machine.exits.len() == 0
        {
            return;
        }
        assert!(!matches!(
            step(machine, hooks),
            ChunkProgress::Finished { .. }
        ));
    }
    panic!("finite source did not reach actual CallExit");
}
fn finish(machine: &mut Machine, hooks: &mut CallHooks) -> KuResult<Flow> {
    for _ in 0..20_000 {
        match step(machine, hooks) {
            ChunkProgress::More { .. } => {}
            ChunkProgress::Waiting { .. } => machine.wait_synchronously(&hooks.trace.host),
            ChunkProgress::Finished { result, .. } => return result,
        }
    }
    panic!("finite call fixture did not finish");
}
fn healthy_and_drain(runtime: &TaskRuntime, previous: usize) {
    assert!(until(
        || runtime.snapshot().unwrap().finished_tasks == previous
    ));
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
    let id = healthy.id();
    let receipt = rx.recv_timeout(WAIT);
    assert!(until(
        || runtime.snapshot().unwrap().finished_tasks == previous + 1
    ));
    let result = healthy.await_result();
    drop(healthy);
    runtime.cancel_all_and_wait(WAIT).unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(receipt.unwrap(), (None, None, id));
    assert_eq!(result.unwrap(), Value::Int(7));
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_tasks,
            snapshot.registered_tasks,
            snapshot.wait_edges,
            snapshot.finished_tasks
        ),
        (0, 0, 0, 0, previous + 1)
    );
}

#[test]
fn scalar_named_and_function_value_call_phases_keep_physical_frames_until_done() {
    for local in [false, true] {
        let (mut machine, mut hooks, expected) = if local {
            let mut producer = production_interpreter(LOCAL);
            let value = producer
                .call_function("Make", vec![], Span::default(), 0)
                .unwrap();
            let call = function_value_call(&value, 1, 0);
            let code =
                CodeHandle::compile_local(&producer.functions, &call, Limits::default()).unwrap();
            let mut machine = Machine::from_code(code, Limits::default(), 0);
            machine.captures = Some(call.captures.clone());
            machine.start_once("local", &call.args, 0, false).unwrap();
            (machine, CallHooks::new(&producer.functions), 21)
        } else {
            let source = functions(NAMED);
            (named(&source), CallHooks::new(&source), 43)
        };
        let mut phases = Vec::new();
        let mut result = None;
        for _ in 0..20_000 {
            let active = machine.exits.get(0).and_then(|exit| {
                if !matches!(exit.completion, ExitCompletion::Call(_)) {
                    return None;
                }
                let phase = exit.state.phase();
                let env = &machine.calls.get(exit.owner_call - 1).unwrap().env;
                let cleared = matches!(
                    phase,
                    cancellation::ExitPhase::DeadlineCheck
                        | cancellation::ExitPhase::Arbitrate
                        | cancellation::ExitPhase::Done
                );
                assert_eq!(env.contains("n"), !cleared);
                if !cleared {
                    let expected_parameter = if local {
                        i64::from(exit.owner_call == 1)
                    } else {
                        2
                    };
                    assert_eq!(scalar(env, "n"), expected_parameter);
                }
                assert_eq!(exit.owner_call, machine.calls.len());
                assert_eq!(machine.call_depth, machine.calls.len());
                assert!(machine.pending.is_none());
                if !local && exit.owner_call == 2 {
                    assert_eq!(scalar(&machine.calls.get(0).unwrap().env, "parent"), 40);
                }
                phases.push((exit.owner_call, phase));
                Some((
                    phase,
                    machine.calls.len(),
                    machine.work.len(),
                    machine.call_depth,
                    hooks.trace.ticks.len(),
                ))
            });
            let progress = step(&mut machine, &mut hooks);
            if let Some((phase, calls, work, depth, ticks)) = active {
                if phase == cancellation::ExitPhase::Done {
                    assert_eq!(
                        (machine.calls.len(), machine.call_depth),
                        (calls - 1, depth - 1)
                    );
                } else {
                    assert_eq!(
                        (machine.calls.len(), machine.work.len(), machine.call_depth),
                        (calls, work, depth)
                    );
                }
                assert_eq!(
                    hooks.trace.ticks.len(),
                    ticks,
                    "call epilogue cannot advance parent syntax"
                );
            }
            assert_eq!(
                (current_cleanup_context(), current_execution_termination()),
                (None, None)
            );
            if let ChunkProgress::Finished {
                result: outgoing, ..
            } = progress
            {
                result = Some(outgoing);
                break;
            }
        }
        let expected_phases = [
            cancellation::ExitPhase::Prepare,
            cancellation::ExitPhase::WaitTerminal,
            cancellation::ExitPhase::ReleaseOwners,
            cancellation::ExitPhase::DropHandles,
            cancellation::ExitPhase::Teardown,
            cancellation::ExitPhase::DeadlineCheck,
            cancellation::ExitPhase::Arbitrate,
            cancellation::ExitPhase::Done,
        ];
        for owner in [2, 1] {
            assert_eq!(
                phases
                    .iter()
                    .filter_map(|(at, phase)| (*at == owner).then_some(*phase))
                    .collect::<Vec<_>>(),
                expected_phases
            );
        }
        assert_eq!(
            result
                .unwrap()
                .unwrap()
                .into_call_value(Span::default(), "fixture")
                .unwrap(),
            Value::Int(expected)
        );
        assert_eq!(hooks.normalized, 2);
        assert!(hooks.fallback.is_empty());
        assert_eq!(
            hooks
                .trace
                .events
                .iter()
                .filter_map(|event| match event {
                    Event::Call(_, function_value) => Some(*function_value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![local; 2],
            "each real named/function-value call must have one Done epilogue"
        );
        assert_eq!(
            (
                machine.metrics.entered_call_exits,
                machine.metrics.retired_call_exits
            ),
            (2, 2)
        );
        assert_eq!(
            machine.metrics.entered_exit_frames,
            machine.metrics.retired_exit_frames
        );
        assert_drained(&machine.metrics, 0);
        eprintln!(
            "call local={local} phases={phases:?} header={} backing={}",
            std::mem::size_of::<PendingScopeExit>(),
            machine.exits.charged_storage_bytes()
        );
    }
}

#[test]
fn sole_call_outcomes_normalize_fail_once_and_block_loop_control_before_parent() {
    let failure = functions("fn Failure(): int! { fail \"sentinel\" }");
    let mut host = Interpreter::new();
    let actual_fail = host
        .exec_stmt(&failure["Failure"].body[0], &mut Env::new(), 0)
        .unwrap();
    assert!(matches!(&actual_fail, Flow::Fail { .. }));
    let source = functions(ENTRY);
    let mut machine = named(&source);
    let mut hooks = CallHooks::new(&source);
    to_call(&mut machine, &mut hooks, 1);
    // These sole outcome injections test the kernel call boundary, rather than
    // advertising Fail or escaped loop control as new scalar source admission.
    drop(machine.pending.replace(Ok(actual_fail)));
    let result = finish(&mut machine, &mut hooks).unwrap();
    let Flow::Return {
        value: Value::Result { ok: false, value },
        ..
    } = result
    else {
        panic!("Done must normalize the sole Fail before returning to its caller");
    };
    let Value::Object(error) = *value else {
        panic!("Fail must be wrapped exactly once");
    };
    assert_eq!(
        error.get("message"),
        Some(&Value::String("sentinel".into()))
    );
    assert_eq!(hooks.normalized, 1);
    assert_eq!(
        (
            machine.metrics.entered_call_exits,
            machine.metrics.retired_call_exits
        ),
        (1, 1)
    );
    assert_drained(&machine.metrics, 0);

    for control in ["break", "continue"] {
        let control_source = functions(&format!("fn Control() {{ while (true) {{ {control} }} }}"));
        let Stmt::While { body, .. } = &control_source["Control"].body[0] else {
            unreachable!()
        };
        let actual_control = host.exec_stmt(&body[0], &mut Env::new(), 0).unwrap();
        let source = functions("fn Leaf(n: int): int { return n }\nfn Entry(n: int): int { parent = 0\nfor index in 2 { parent = parent + 1\nLeaf(n)\nparent = parent + 10 }\nreturn parent }");
        let Stmt::For { body, .. } = &source["Entry"].body[1] else {
            unreachable!()
        };
        let later = stmt_span(&body[2]);
        let mut machine = named(&source);
        let mut hooks = CallHooks::new(&source);
        to_call(&mut machine, &mut hooks, 2);
        assert_eq!(scalar(&machine.calls.get(0).unwrap().env, "parent"), 1);
        drop(machine.pending.replace(Ok(actual_control)));
        let error = finish(&mut machine, &mut hooks)
            .err()
            .expect("escaped loop control is an error");
        assert_eq!(error.message, "loop control escaped function");
        assert!(!hooks.trace.ticks.contains(&later));
        assert_eq!(hooks.normalized, 2);
        assert_eq!(
            (machine.metrics.entered_calls, machine.metrics.retired_calls),
            (2, 2)
        );
        assert_drained(&machine.metrics, 0);
    }
}

#[test]
fn call_task_wait_and_budget_rejection_preserve_original_env_primary_and_accounting() {
    for reject in [None, Some(false), Some(true)] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut child = Running::start(&runtime, true);
        let id = child.owner.as_ref().unwrap().id();
        let source = functions(NAMED);
        let mut hooks = CallHooks::new(&source);
        hooks.trace.host.task_runtime = Some(runtime.clone());
        hooks.target = Some(source["Leaf"].span);
        hooks.child = child.owner.take();
        let mut machine = named(&source);
        to_call(&mut machine, &mut hooks, 2);
        assert!(matches!(
            machine.pending.as_ref(),
            Some(Ok(Flow::Return {
                value: Value::Int(3),
                ..
            }))
        ));
        if let Some(handles) = reject {
            hooks.release = Some(Gate(child.release.0.take()));
            if handles {
                machine.value_limits.max_storage_bytes =
                    machine.values.charged_storage_bytes() + machine.argument_bytes;
            } else {
                machine.work_limits.max_storage_bytes = machine.work.charged_storage_bytes()
                    + machine.finally.frames.charged_storage_bytes();
            }
        } else {
            let mut waiting = false;
            for _ in 0..32 {
                if matches!(
                    step(&mut machine, &mut hooks),
                    ChunkProgress::Waiting { .. }
                ) {
                    waiting = true;
                    break;
                }
            }
            assert!(waiting);
            let exit = machine.exits.get(0).unwrap();
            assert!(matches!(
                exit.completion,
                ExitCompletion::Call(CallKind::Named)
            ));
            assert_eq!(exit.owner_call, 2);
            let context = exit.state.context();
            let frozen = (
                machine.calls.len(),
                machine.call_depth,
                machine.work.len(),
                hooks.trace.ticks.len(),
            );
            assert_eq!(scalar(&machine.calls.get(1).unwrap().env, "n"), 2);
            assert_eq!(scalar(&machine.calls.get(0).unwrap().env, "parent"), 40);
            assert_eq!(
                machine
                    .calls
                    .get(1)
                    .unwrap()
                    .env
                    .current_scope_simple_task_count(hooks.target.unwrap())
                    .unwrap(),
                1
            );
            assert!(machine.pending.is_none() && machine.awaiting.is_none());
            assert_eq!(
                machine.exit_handle_bytes,
                machine.exit_handle_capacity * std::mem::size_of::<TaskHandle>()
            );
            assert!(machine.exit_handle_bytes > 0);
            for _ in 0..2 {
                machine.wait_synchronously(&hooks.trace.host);
                assert!(matches!(
                    step(&mut machine, &mut hooks),
                    ChunkProgress::Waiting { .. }
                ));
                assert_eq!(
                    (
                        machine.calls.len(),
                        machine.call_depth,
                        machine.work.len(),
                        hooks.trace.ticks.len()
                    ),
                    frozen
                );
                assert_eq!(machine.exits.get(0).unwrap().state.context(), context);
                assert_eq!(
                    (current_cleanup_context(), current_execution_termination()),
                    (None, None)
                );
            }
            assert_eq!(runtime.snapshot().unwrap().finished_tasks, 0);
            child.release.release();
        }
        let result = finish(&mut machine, &mut hooks);
        let completion = child.done.recv_timeout(WAIT);
        let calls = (machine.metrics.entered_calls, machine.metrics.retired_calls);
        let exit_counts = (
            machine.metrics.entered_call_exits,
            machine.metrics.retired_call_exits,
            machine.metrics.entered_exit_frames,
            machine.metrics.retired_exit_frames,
            machine.peak_exit_handle_bytes,
        );
        let charge = (
            machine.exits.len(),
            machine.exit_handle_capacity,
            machine.exit_handle_bytes,
        );
        let fallback = hooks.fallback.clone();
        drop(machine);
        healthy_and_drain(&runtime, 1);
        eprintln!("call reject={reject:?} fallback={fallback:?} error={:?} real_completion={completion:?} exit_counts={exit_counts:?} header={}", result.as_ref().err(), std::mem::size_of::<PendingScopeExit>());
        assert_eq!(hooks.injected, 1);
        assert_eq!(calls, (2, 2));
        assert_eq!(charge, (0, 0, 0));
        assert_eq!(exit_counts.0, exit_counts.1);
        assert_eq!(exit_counts.2, exit_counts.3);
        let (task_id, context) = completion.unwrap();
        assert_eq!(task_id, id);
        assert_eq!(context.unwrap().reason, TerminationReason::Cancelled);
        if reject.is_some() {
            assert_eq!(
                exit_counts.4, 0,
                "reject before allocating a handle candidate"
            );
            assert_eq!(fallback, vec![(2, id, true)]);
            assert!(result
                .err()
                .expect("rejected call storage is an error")
                .message
                .contains("internal execution storage rejected"));
        } else {
            assert_eq!((exit_counts.0, exit_counts.1), (2, 2));
            assert!(exit_counts.4 >= std::mem::size_of::<TaskHandle>());
            assert!(fallback.is_empty());
            assert_eq!(
                result
                    .unwrap()
                    .into_call_value(Span::default(), "fixture")
                    .unwrap(),
                Value::Int(43)
            );
            assert_eq!(hooks.normalized, 2);
        }
    }
}

struct NativeCapture {
    lease: Option<Arc<HttpListenerLease>>,
    shorter: RuntimeTermination,
    receipt: SyncSender<Option<RuntimeTermination>>,
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Drop for NativeCapture {
    fn drop(&mut self) {
        drop(CleanupGuard::enter(self.shorter));
        drop(self.lease.take());
        self.drops.fetch_add(1, Ordering::AcqRel);
        let _ = self.receipt.try_send(current_cleanup_context());
        if self.panic {
            panic!("actual call capture native Drop panic");
        }
    }
}

#[test]
fn actual_call_capture_drop_saves_shorter_d_and_native_panic_restores_tls() {
    for panics in [false, true] {
        let runtime = TaskRuntime::with_limits(1, 1024, 0, 0, 1024);
        let mut blocked = Running::start(&runtime, false);
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let lease = HttpListenerLease::new(http_listener_registry::insert(listener).unwrap());
        let observer = Arc::downgrade(&lease);
        let drops = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::sync_channel(1);
        let probe = NativeCapture {
            lease: Some(lease),
            shorter,
            receipt: tx,
            drops: drops.clone(),
            panic: panics,
        };
        let ran = Arc::new(AtomicBool::new(false));
        let callback_ran = ran.clone();
        let child = runtime
            .spawn(move || {
                callback_ran.store(true, Ordering::Release);
                drop(probe);
                Ok(Value::Null)
            })
            .unwrap();
        let source = functions(ENTRY);
        let mut hooks = CallHooks::new(&source);
        hooks.trace.host.task_runtime = Some(runtime.clone());
        hooks.target = Some(source["Entry"].span);
        hooks.child = Some(child);
        let mut machine = named(&source);
        to_call(&mut machine, &mut hooks, 1);
        {
            let _execution = ExecutionTerminationGuard::enter();
            let cancellation: KuResult<Flow> =
                Err(KuError::termination(primary, source["Entry"].span));
            hooks.trace.host.observe_termination(&cancellation);
        }
        let ambient_guard = CleanupGuard::enter(ambient);
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..32 {
                step(&mut machine, &mut hooks);
                if drops.load(Ordering::Acquire) != 0 {
                    return;
                }
            }
            panic!("finite call broadcast did not destroy its queued capture");
        }));
        let exit = machine.exits.get(0).unwrap();
        assert!(matches!(
            exit.completion,
            ExitCompletion::Call(CallKind::Named)
        ));
        let saved = exit.state.release_context();
        let meta = exit.state.context();
        let retained = (
            machine.calls.len(),
            machine.call_depth,
            scalar(&machine.calls.get(0).unwrap().env, "n"),
        );
        let aborted = machine.aborted_release;
        let after_phase = current_cleanup_context();
        let receipt = rx.try_recv();
        let before_retirement = runtime.snapshot().unwrap();
        drop(ambient_guard);
        let outside = (current_cleanup_context(), current_execution_termination());
        let result = observed.is_ok().then(|| finish(&mut machine, &mut hooks));
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(machine)));
        blocked.release.release();
        healthy_and_drain(&runtime, 2);
        eprintln!("call native={panics} meta={meta:?} saved={saved:?} aborted={aborted:?} before_retirement={before_retirement:?} receipt={receipt:?}");
        assert_eq!(hooks.injected, 1);
        assert_eq!(retained, (1, 1, 2));
        assert_eq!(meta.unwrap().reason, primary.reason);
        assert_eq!(saved, Some(effective));
        assert_eq!(after_phase, Some(effective));
        assert_eq!(outside, (None, None));
        assert_eq!(receipt.unwrap(), Some(effective));
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert_eq!(observer.strong_count(), 0);
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(
            (
                before_retirement.active_tasks,
                before_retirement.queued_tasks,
                before_retirement.finished_tasks
            ),
            (2, 1, 0)
        );
        assert!(dropped.is_ok());
        if panics {
            let panic = match observed {
                Err(panic) => panic,
                Ok(_) => panic!("captured native destructor must panic"),
            };
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("actual call capture native Drop panic")
            );
            assert_eq!(aborted, Some(effective));
            assert_eq!(hooks.normalized, 0);
        } else {
            observed.unwrap();
            let result = result.unwrap().unwrap();
            assert_eq!(
                cancellation::ScopeExit::cleanup_context(&result),
                Some(RuntimeTermination {
                    reason: primary.reason,
                    cleanup_deadline: shorter.cleanup_deadline
                })
            );
            assert_eq!(
                result.into_call_value(Span::default(), "fixture").unwrap(),
                Value::Int(3)
            );
            assert_eq!(hooks.normalized, 1);
            assert_eq!(aborted, None);
        }
    }
}
